//! Привязки клавиш: разбор цепочек, проверка конфликтов, генерация Lua для Hyprland
//! и список для просмотра.
//!
//! Цепочка — сочетания через пробел («SUPER+W 3 f»). Одиночное сочетание — цепочка
//! из одного звена, оно становится прямой привязкой. Действия демона уходят
//! подкомандой `workspaced …`, остальные (`exec`, `dispatch`, `lua`) композитор
//! выполняет сам.
//!
//! Цепочка с выходом (раздел `[sticky.<имя>]`, изменение sticky-chains) —
//! режим: сочетание входа открывает подкарту корневого состояния, и подкарта
//! остаётся открытой после каждого действия, пока не нажата клавиша выхода,
//! Escape или Backspace в корне. Каждое состояние — своя подкарта
//! `ws-sticky:<имя>/<состояние>/…`; нажатия без привязки поглощает `catchall`.
//!
//! Многозвенная цепочка из любого раздела конфига тоже становится режимом
//! (изменение chains-sticky): все цепочки с общим первым сочетанием образуют
//! автоматический режим `ws-sticky:<первое сочетание>`, следующее общее звено —
//! вложенное состояние `ws-sticky:<первое>/<второе>/…`, а последнее звено —
//! клавиша выхода: выполняет действие и закрывает режим.

use std::fmt::Write;

use anyhow::{Result, bail};

use crate::config::{Action, Bind, Config, Desk, Dispatch, HalfAction, MoveAction, PlaceAction, STICKY_APPS, StickyState, TargetAction};

/// Одно сочетание в каноническом виде: модификаторы по фиксированному порядку,
/// клавиша как написана.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Combo {
    pub mods: Vec<String>,
    pub key: String,
}

impl Combo {
    /// Вид для Lua: «SUPER + SHIFT + left».
    pub fn lua(&self) -> String {
        let mut parts = self.mods.clone();
        parts.push(self.key.clone());
        parts.join(" + ")
    }
    /// Компактный вид для списка и сообщений: «SUPER+SHIFT+left».
    pub fn compact(&self) -> String {
        let mut parts = self.mods.clone();
        parts.push(self.key.clone());
        parts.join("+")
    }
    /// Ключ сравнения: клавиши в Hyprland не зависят от регистра.
    pub(crate) fn cmp_key(&self) -> (Vec<String>, String) {
        (self.mods.clone(), self.key.to_ascii_lowercase())
    }
}

/// Разобранная цепочка.
pub type Chain = Vec<Combo>;

fn mod_rank(m: &str) -> (u8, String) {
    let r = match m {
        "SUPER" => 0,
        "CTRL" | "CONTROL" => 1,
        "ALT" => 2,
        "SHIFT" => 3,
        _ => 4,
    };
    (r, m.to_string())
}

/// «shift+super+E» → модификаторы SUPER, SHIFT, клавиша E.
pub fn parse_combo(s: &str) -> Result<Combo> {
    let parts: Vec<&str> = s.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    let Some((key, mods)) = parts.split_last() else {
        bail!("пустое сочетание {s:?}");
    };
    let mut mods: Vec<String> = mods.iter().map(|m| m.to_ascii_uppercase()).collect();
    mods.sort_by_key(|m| mod_rank(m));
    mods.dedup();
    Ok(Combo { mods, key: key.to_string() })
}

/// «SUPER+W 3 f» → [SUPER+W, 3, f].
pub fn parse_chain(s: &str) -> Result<Chain> {
    let chain: Chain = s.split_whitespace().map(parse_combo).collect::<Result<_>>()?;
    if chain.is_empty() {
        bail!("пустая цепочка {s:?}");
    }
    Ok(chain)
}

/// Ключ сравнения цепочки: модификаторы по порядку и клавиши без учёта
/// регистра, как их сравнивает Hyprland.
pub type ChainKey = Vec<(Vec<String>, String)>;

/// Ключ сравнения разобранной цепочки.
pub fn chain_key(chain: &Chain) -> ChainKey {
    chain.iter().map(Combo::cmp_key).collect()
}

/// Две записи цепочки означают одно и то же. Цепочка с ошибкой ни с чем
/// не совпадает: конфиг с ней не проходит проверку.
pub fn same_chain(a: &str, b: &str) -> bool {
    match (parse_chain(a), parse_chain(b)) {
        (Ok(x), Ok(y)) => chain_key(&x) == chain_key(&y),
        _ => false,
    }
}

pub fn chain_compact(chain: &Chain) -> String {
    chain.iter().map(Combo::compact).collect::<Vec<_>>().join(" ")
}

/// Что делает привязка.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// Подкоманда демона (`workspaced raise dots`).
    Daemon(String),
    /// Команда для композитора.
    Exec(String),
    /// Выражения диспетчеров без префикса `hl.dsp.`.
    Dispatch(Vec<String>),
    /// Тело функции на Lua.
    Lua(String),
    /// Вход в цепочку с выходом: открыть подкарту её корневого состояния.
    Sticky(String),
}

impl Act {
    /// Строка для списка: «raise dots», «exec wezterm-gui», «dispatch window.close()».
    pub fn describe(&self) -> String {
        match self {
            Act::Daemon(cmd) => {
                let cmd = cmd.strip_prefix("workspaced ").unwrap_or(cmd);
                // Цепочка клавиши приложения в списке без кавычек оболочки.
                for head in ["key --new ", "key "] {
                    if let Some(chain) = cmd.strip_prefix(head).and_then(|c| c.strip_prefix('\'')).and_then(|c| c.strip_suffix('\'')) {
                        return format!("{head}{}", chain.replace("'\\''", "'"));
                    }
                }
                cmd.to_string()
            }
            Act::Exec(cmd) => format!("exec {cmd}"),
            Act::Dispatch(v) => format!("dispatch {}", v.join("; ")),
            Act::Lua(body) => {
                let first = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
                format!("lua {first}{}", if body.trim().lines().count() > 1 { " …" } else { "" })
            }
            Act::Sticky(name) => format!("sticky {name}"),
        }
    }
}

/// Флаги привязки Hyprland.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    pub locked: bool,
    pub repeating: bool,
    pub mouse: bool,
    pub release: bool,
}

impl Flags {
    fn names(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.locked {
            v.push("locked");
        }
        if self.repeating {
            v.push("repeating");
        }
        if self.mouse {
            v.push("mouse");
        }
        if self.release {
            v.push("release");
        }
        v
    }
    fn any(&self) -> bool {
        !self.names().is_empty()
    }
    /// Флаги для `hl.bind`. Удержание кнопки мыши — `mouse`, как в примере из
    /// поставки Hyprland; флаг `drag` из заглушек API означает другое: привязка
    /// срабатывает по отпусканию кнопки, и перетаскивание начиналось бы со
    /// второго клика.
    fn lua(&self) -> String {
        let name = |n: &str| format!("{n} = true");
        format!("{{ {} }}", self.names().iter().map(|n| name(n)).collect::<Vec<_>>().join(", "))
    }
}

/// Привязка: цепочка, действие, флаги и откуда она взялась (для сообщений).
/// `origin` — номер записи `[[binds]]`, из которой привязка получена; у клавиш
/// `[keys]`, приложений и workspace его нет.
#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    pub chain: Chain,
    pub source: String,
    pub act: Act,
    pub flags: Flags,
    pub origin: Option<usize>,
}

fn plain(chain: Chain, source: String, command: String) -> Binding {
    Binding { chain, source, act: Act::Daemon(command), flags: Flags::default(), origin: None }
}

/// Запись с подстановкой `n` на место `$n` в chain, exec, dispatch, desc
/// и номере стола действия `move`; поле `range` снимается.
pub(crate) fn substitute(b: &Bind, n: &str) -> Bind {
    let sub = |s: &String| s.replace("$n", n);
    let mut e = b.clone();
    e.range = None;
    e.chain = sub(&b.chain);
    e.exec = b.exec.as_ref().map(sub);
    e.desc = b.desc.as_ref().map(sub);
    // Номер стола в действии `move` подставляется так же, как в цепочке:
    // одна запись разворачивается в серию Ctrl+Super+1…8.
    e.action = b.action.as_ref().map(|a| match a {
        Action::Move(m) => Action::Move(MoveAction { desktop: Desk::Str(sub(&m.desktop.text())) }),
        other => other.clone(),
    });
    e.dispatch = b.dispatch.as_ref().map(|d| match d {
        Dispatch::One(s) => Dispatch::One(sub(s)),
        Dispatch::Many(v) => Dispatch::Many(v.iter().map(sub).collect()),
    });
    e
}

/// Развернуть запись с `range`: подстановка `$n` в chain, exec и dispatch.
pub(crate) fn expand_range(b: &Bind) -> Vec<Bind> {
    let Some((from, to)) = b.range else {
        return vec![b.clone()];
    };
    (from..=to).map(|n| substitute(b, &n.to_string())).collect()
}

/// Действие записи `[[binds]]`. `strict` — номер стола в `move` обязан быть
/// числом; без него (подпись серии «1…8» в подсказке клавиш) текст номера
/// попадает в команду как есть.
pub(crate) fn bind_act(b: &Bind, strict: bool) -> Result<Act> {
    let act = if let Some(a) = &b.action {
        Act::Daemon(match a {
            Action::Named(n) => match n.as_str() {
                "sessions" => "workspaced sessions".to_string(),
                "save-session" => "workspaced save-session".to_string(),
                "save-workspace" => "workspaced save-workspace".to_string(),
                "next-workspace" => "workspaced next".to_string(),
                "maximize" => "workspaced maximize".to_string(),
                "arrange" => "workspaced arrange".to_string(),
                "detach" => "workspaced detach".to_string(),
                other => bail!("привязка {:?}: неизвестное действие {other:?}", b.chain),
            },
            Action::Half(HalfAction { half }) => format!("workspaced half {half}"),
            Action::Place(PlaceAction { place }) => format!("workspaced place {place}"),
            // Номер стола проверяется здесь, а не при разборе конфига:
            // в записи с `range` на его месте стоит `$n`, и число
            // появляется только после разворачивания серии.
            Action::Move(MoveAction { desktop }) => match desktop.number() {
                Some(n) => format!("workspaced move-desktop {n}"),
                None if !strict => format!("workspaced move-desktop {}", desktop.text()),
                None => bail!("привязка {:?}: move должен быть номером стола от 1 до 8, получено {:?}", b.chain, desktop.text()),
            },
            Action::Target(TargetAction { desktop, workspace, app, pull, new }) => {
                let mut cmd = match (workspace, app) {
                    (_, Some(a)) => format!("workspaced app {a}"),
                    (Some(w), None) => format!("workspaced raise {w}"),
                    (None, None) => bail!("привязка {:?}: в action нужен workspace или app", b.chain),
                };
                if let (Some(w), Some(_)) = (workspace, app) {
                    write!(cmd, " --workspace {w}").unwrap();
                }
                if let Some(d) = desktop {
                    write!(cmd, " --desktop {d}").unwrap();
                }
                if *pull == Some(true) {
                    cmd.push_str(" --pull");
                }
                if *new == Some(true) {
                    cmd.push_str(" --new");
                }
                cmd
            }
        })
    } else if let Some(cmd) = &b.exec {
        Act::Exec(cmd.clone())
    } else if let Some(d) = &b.dispatch {
        Act::Dispatch(d.exprs())
    } else if let Some(body) = &b.lua {
        Act::Lua(body.clone())
    } else {
        bail!("привязка {:?}: нет действия", b.chain);
    };
    Ok(act)
}

/// Цепочка в одинарных кавычках оболочки: в ней бывают пробелы
/// (`SUPER+TAB v`), а привязка выполняется через `sh -c`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Собрать все привязки из конфига в порядке файла: `[keys]`, приложения,
/// workspace, `[[binds]]`. Все отклики на одну цепочку клавиши приложения
/// объединяются в одну команду `workspaced key '<цепочка>'`.
pub fn collect(cfg: &Config) -> Result<Vec<Binding>> {
    let mut out: Vec<Binding> = Vec::new();
    let named = [
        (&cfg.keys.save_session, "keys.save_session", "workspaced save-session"),
        (&cfg.keys.save_workspace, "keys.save_workspace", "workspaced save-workspace"),
        (&cfg.keys.sessions, "keys.sessions", "workspaced sessions"),
        (&cfg.keys.next_workspace, "keys.next_workspace", "workspaced next"),
    ];
    for (chain, source, command) in named {
        if let Some(c) = chain {
            out.push(plain(parse_chain(c)?, source.to_string(), command.to_string()));
        }
    }
    // Клавиши приложений (изменение workspace-overrides, решение D3):
    // собственные клавиши и ключи записей workspace, отличные от собственной
    // клавиши. Все отклики на одну цепочку дают одну привязку
    // `workspaced key '<цепочка>'`, а какое приложение и в каком workspace она
    // вызывает, демон решает при нажатии по активному workspace стола.
    let mut by_chain: Vec<(Chain, Vec<String>)> = Vec::new();
    let mut respond = |chain: Chain, source: String| {
        let k = chain_key(&chain);
        match by_chain.iter_mut().find(|(c, _)| chain_key(c) == k) {
            Some((_, sources)) => sources.push(source),
            None => by_chain.push((chain, vec![source])),
        }
    };
    for (name, app) in &cfg.apps {
        if let Some(c) = &app.chain {
            respond(parse_chain(c)?, format!("apps.{name}"));
        }
    }
    for (wname, w) in &cfg.workspaces {
        for (aname, e) in &w.apps {
            if let Some(c) = e.chain.as_deref().filter(|_| cfg.overrides_key(wname, aname)) {
                respond(parse_chain(c)?, format!("workspaces.{wname}.apps.{aname}"));
            }
        }
    }
    // Две записи одного workspace с одним ключом — ошибка (решение D8):
    // ключ записи — переопределение, а без него собственная клавиша.
    for (wname, w) in &cfg.workspaces {
        let mut seen: Vec<(ChainKey, &str)> = Vec::new();
        for aname in w.apps.keys() {
            let Some(k) = cfg.app_key(wname, aname) else { continue };
            let chain = parse_chain(k)?;
            let key = chain_key(&chain);
            if let Some((_, other)) = seen.iter().find(|(x, _)| *x == key) {
                bail!("workspace {wname}: приложения {other} и {aname} имеют одну цепочку {:?}", chain_compact(&chain));
            }
            seen.push((key, aname.as_str()));
        }
    }
    by_chain.sort_by(|a, b| a.0.cmp(&b.0));
    for (chain, sources) in by_chain {
        let command = format!("workspaced key {}", shell_quote(&chain_compact(&chain)));
        out.push(plain(chain, sources.join(","), command));
    }
    for (name, w) in &cfg.workspaces {
        if let Some(c) = &w.chain {
            out.push(plain(parse_chain(c)?, format!("workspaces.{name}"), format!("workspaced raise {name}")));
        }
    }
    for (i, raw) in cfg.binds.iter().enumerate() {
        for b in expand_range(raw) {
            let source = format!("binds[{i}] {:?}", b.chain);
            let act = bind_act(&b, true)?;
            let flags = Flags { locked: b.locked, repeating: b.repeating, mouse: b.mouse, release: b.release };
            out.push(Binding { chain: parse_chain(&b.chain)?, source, act, flags, origin: Some(i) });
        }
    }
    // Сочетание входа цепочки с выходом — обычная цепочка сессии из одного
    // звена: она проходит общие проверки совпадения, начала цепочки
    // и `reserved` (решение D8). Клавиши состояний сочетаниями сессии
    // не являются и сюда не попадают.
    for (name, root) in &cfg.sticky {
        let Some(enter) = &root.enter else {
            bail!("sticky.{name}: нужно поле enter — сочетание входа");
        };
        let chain = parse_chain(enter)?;
        if chain.len() != 1 {
            bail!("sticky.{name}: enter {enter:?} должно быть одним сочетанием, вход звеном многозвенной цепочки не допускается");
        }
        out.push(Binding { chain, source: format!("sticky.{name}"), act: Act::Sticky(name.clone()), flags: Flags::default(), origin: None });
    }
    Ok(out)
}

/// Префикс имён подкарт цепочек с выходом; по нему их узнаёт индикатор панели.
pub const STICKY_PREFIX: &str = "ws-sticky:";

/// Куда ведёт клавиша состояния.
#[derive(Debug, Clone, PartialEq)]
pub enum StickyGo {
    /// Перейти в дочернее состояние с этим именем.
    State(String),
    /// Выполнить действие.
    Act(Act),
}

/// Клавиша состояния цепочки с выходом после наследования и дополнения.
#[derive(Debug, Clone)]
pub struct StickyBind {
    /// Имя клавиши Hyprland.
    pub key: String,
    /// Модификаторы клавиши. Пусто у клавиш разделов `[sticky.*]` и у звеньев
    /// без модификаторов: такая клавиша привязывается с `ignore_mods`.
    /// Звено автоматического режима с модификаторами (`SUPER+CTRL+w`)
    /// привязывается точно, с этими модификаторами.
    pub mods: Vec<String>,
    /// Флаги записи `[[binds]]`, из которой взято звено автоматического режима.
    pub flags: Flags,
    pub go: StickyGo,
    pub exit: bool,
    /// Описание из поля `desc`.
    pub desc: Option<String>,
    /// Источник унаследованной клавиши приложения (`apps.neovide`,
    /// `workspaces.surf.apps.chrome-ai`), если действие взято от неё.
    pub inherited: Option<String>,
    /// Запись с явным действием: по ней подсказка выводит описание.
    pub explicit: Option<Bind>,
    /// Привязка сессии, последнее звено которой — эта клавиша
    /// автоматического режима; по ней подсказка выводит описание.
    pub binding: Option<Binding>,
}

impl StickyBind {
    /// Сочетание клавиши с её модификаторами.
    pub fn combo(&self) -> Combo {
        Combo { mods: self.mods.clone(), key: self.key.clone() }
    }
}

/// Состояние цепочки с выходом, развёрнутое в плоский список (решение D10).
#[derive(Debug, Clone)]
pub struct StickyNode {
    /// Имя цепочки (`apps`).
    pub chain: String,
    /// Имена состояний от корня, без корня: пусто у корня.
    pub names: Vec<String>,
    /// Подписи состояний от корня до этого состояния включительно.
    pub titles: Vec<String>,
    /// Имя подкарты: `ws-sticky:apps`, `ws-sticky:apps/raise`.
    pub submap: String,
    /// Подкарта родителя; у корня нет.
    pub parent: Option<String>,
    /// Сочетание входа и клавиши переходов до этого состояния.
    pub prefix: Chain,
    /// Раздел конфига: `sticky.apps`, `sticky.apps.raise`.
    pub source: String,
    pub keys: Vec<StickyBind>,
    /// Автоматический режим многозвенных цепочек (изменение chains-sticky),
    /// а не раздел `[sticky.*]`.
    pub auto: bool,
}

impl StickyNode {
    /// Путь клавиши состояния: вход, переходы и сама клавиша.
    pub fn key_chain(&self, k: &StickyBind) -> Chain {
        self.combo_chain(k.combo())
    }
    fn combo_chain(&self, combo: Combo) -> Chain {
        let mut c = self.prefix.clone();
        c.push(combo);
        c
    }
}

/// Имя цепочки или состояния: входит в имя подкарты (решение D9).
fn sticky_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Клавиши приложений, которые наследует состояние с полем `apps`
/// (решение D5): цепочки клавиш приложений из одного сочетания
/// с модификатором ровно Super. Возвращает клавишу в нижнем регистре,
/// цепочку и источник, в порядке цепочек приложений.
fn app_keys(binds: &[Binding]) -> Vec<(String, Chain, String)> {
    binds
        .iter()
        .filter(|b| b.origin.is_none() && b.chain.len() == 1 && b.chain[0].mods == ["SUPER"])
        .filter(|b| matches!(&b.act, Act::Daemon(c) if c.starts_with("workspaced key ")))
        .map(|b| (b.chain[0].key.to_ascii_lowercase(), b.chain.clone(), b.source.clone()))
        .collect()
}

/// Все состояния всех цепочек с выходом в порядке обхода (корень, затем
/// дочерние по порядку файла, вглубь) с проверками решений D2, D3, D5.
pub fn sticky_nodes(cfg: &Config) -> Result<Vec<StickyNode>> {
    let binds = collect(cfg)?;
    let inherited = app_keys(&binds);
    let mut out = Vec::new();
    for (name, root) in &cfg.sticky {
        if !sticky_name(name) {
            bail!("sticky.{name}: имя цепочки должно состоять из латинских строчных букв, цифр и дефиса");
        }
        let enter = binds.iter().find(|b| b.act == Act::Sticky(name.clone())).map(|b| b.chain.clone()).unwrap_or_default();
        sticky_state(cfg, &inherited, name, root, Vec::new(), Vec::new(), enter, None, &mut out)?;
    }
    let user: Vec<String> = out.iter().map(|n| n.submap.clone()).collect();
    for node in auto_nodes(&binds)? {
        if user.contains(&node.submap) {
            bail!("режим цепочек {:?} ({}) совпадает по имени подкарты {:?} с цепочкой с выходом sticky.{}", chain_compact(&node.prefix), node.source, node.submap, node.chain);
        }
        out.push(node);
    }
    Ok(out)
}

/// Дерево многозвенных цепочек: звенья с одним ключом сравнения сливаются
/// в один узел, порядок детей — порядок первого появления в конфиге.
struct Trie {
    combo: Combo,
    children: Vec<Trie>,
    leaf: Option<Binding>,
}

fn trie_insert(list: &mut Vec<Trie>, chain: &[Combo], b: &Binding) {
    let k = chain[0].cmp_key();
    let i = match list.iter().position(|t| t.combo.cmp_key() == k) {
        Some(i) => i,
        None => {
            list.push(Trie { combo: chain[0].clone(), children: Vec::new(), leaf: None });
            list.len() - 1
        }
    };
    if chain.len() == 1 {
        list[i].leaf = Some(b.clone());
    } else {
        trie_insert(&mut list[i].children, &chain[1..], b);
    }
}

/// Источники всех привязок под узлом, без повторов.
fn trie_sources(t: &Trie, out: &mut Vec<String>) {
    if let Some(b) = &t.leaf {
        let s = b.source.split(' ').next().unwrap_or("").to_string();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    for c in &t.children {
        trie_sources(c, out);
    }
}

/// Автоматические режимы многозвенных цепочек (изменение chains-sticky,
/// решения D1–D3): корень — первое сочетание, вложенные состояния — общие
/// промежуточные звенья, клавиши выхода — последние звенья.
pub fn auto_nodes(binds: &[Binding]) -> Result<Vec<StickyNode>> {
    let mut roots: Vec<Trie> = Vec::new();
    for b in binds.iter().filter(|b| b.chain.len() > 1) {
        trie_insert(&mut roots, &b.chain, b);
    }
    let mut out = Vec::new();
    for root in &roots {
        auto_state(root, &root.combo.compact(), Vec::new(), Vec::new(), vec![root.combo.clone()], None, &mut out)?;
    }
    Ok(out)
}

fn auto_state(t: &Trie, chain: &str, names: Vec<String>, mut titles: Vec<String>, prefix: Chain, parent: Option<String>, out: &mut Vec<StickyNode>) -> Result<()> {
    let here = chain_compact(&prefix);
    if let Some(b) = &t.leaf {
        // Проверка совпадения цепочек называет обе записи раньше; здесь —
        // на случай вызова без неё.
        bail!("цепочка {here:?} ({}) является началом другой цепочки", b.source);
    }
    titles.push(crate::keys_help::combo_label(&t.combo));
    let submap = if names.is_empty() { format!("{STICKY_PREFIX}{chain}") } else { format!("{STICKY_PREFIX}{chain}/{}", names.join("/")) };
    let mut sources = Vec::new();
    trie_sources(t, &mut sources);
    let mut node = StickyNode {
        chain: chain.to_string(),
        names: names.clone(),
        titles: titles.clone(),
        submap: submap.clone(),
        parent,
        prefix: prefix.clone(),
        source: sources.join(","),
        keys: Vec::new(),
        auto: true,
    };
    for c in &t.children {
        let lower = c.combo.key.to_ascii_lowercase();
        if lower == "escape" || lower == "backspace" {
            bail!("цепочка {} ({}): звено {:?} недопустимо: Escape и BackSpace в режиме цепочки закрывают режим и возвращают назад", chain_compact(&node.combo_chain(c.combo.clone())), sources.join(","), c.combo.compact());
        }
        // Звено без модификаторов привязывается с `ignore_mods` и перехватило
        // бы звено с той же клавишей и модификаторами (решение D2).
        if let Some(o) = node.keys.iter().find(|k| k.key.eq_ignore_ascii_case(&c.combo.key) && (k.mods.is_empty() || c.combo.mods.is_empty())) {
            bail!(
                "цепочки {:?} и {:?} в режиме {here:?} неразличимы: звено без модификаторов срабатывает при любых модификаторах",
                chain_compact(&node.key_chain(o)),
                chain_compact(&node.combo_chain(c.combo.clone()))
            );
        }
        let (go, exit, flags, binding) = if c.children.is_empty() {
            let b = c.leaf.clone().expect("лист дерева цепочек без привязки");
            (StickyGo::Act(b.act.clone()), true, b.flags, Some(b))
        } else {
            (StickyGo::State(c.combo.compact()), false, Flags::default(), None)
        };
        node.keys.push(StickyBind { key: c.combo.key.clone(), mods: c.combo.mods.clone(), flags, go, exit, desc: None, inherited: None, explicit: None, binding });
    }
    out.push(node);
    for c in t.children.iter().filter(|c| !c.children.is_empty()) {
        let mut sub_names = names.clone();
        sub_names.push(c.combo.compact());
        let mut sub_prefix = prefix.clone();
        sub_prefix.push(c.combo.clone());
        auto_state(c, chain, sub_names, titles.clone(), sub_prefix, Some(submap.clone()), out)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn sticky_state(
    cfg: &Config,
    inherited: &[(String, Chain, String)],
    chain: &str,
    st: &StickyState,
    names: Vec<String>,
    mut titles: Vec<String>,
    prefix: Chain,
    parent: Option<String>,
    out: &mut Vec<StickyNode>,
) -> Result<()> {
    let own = names.last().cloned().unwrap_or_else(|| chain.to_string());
    titles.push(st.title.clone().unwrap_or_else(|| own.clone()));
    let submap = if names.is_empty() { format!("{STICKY_PREFIX}{chain}") } else { format!("{STICKY_PREFIX}{chain}/{}", names.join("/")) };
    let source = std::iter::once(format!("sticky.{chain}")).chain(names.iter().cloned()).collect::<Vec<_>>().join(".");
    let mut node = StickyNode { chain: chain.to_string(), names: names.clone(), titles: titles.clone(), submap: submap.clone(), parent, prefix: prefix.clone(), source, keys: Vec::new(), auto: false };
    let at = || format!("sticky.{chain}, {}", node_label(&names));
    if !names.is_empty() && st.enter.is_some() {
        bail!("{}: поле enter допустимо только у корня цепочки", at());
    }
    let action = match st.apps.as_deref() {
        None => None,
        Some("raise") => Some("workspaced key "),
        Some("spawn") => Some("workspaced key --new "),
        Some(other) => bail!("{}: apps должно быть одним из {}, получено {other:?}", at(), STICKY_APPS.join(", ")),
    };
    let mut rest: Vec<StickyBind> = match action {
        Some(head) => inherited
            .iter()
            .map(|(key, c, src)| StickyBind {
                key: key.clone(),
                mods: Vec::new(),
                flags: Flags::default(),
                go: StickyGo::Act(Act::Daemon(format!("{head}{}", shell_quote(&chain_compact(c))))),
                exit: false,
                desc: None,
                inherited: Some(src.clone()),
                explicit: None,
                binding: None,
            })
            .collect(),
        None => Vec::new(),
    };
    for (key, v) in &st.keys {
        let lower = key.to_ascii_lowercase();
        if key.trim().is_empty() || key.contains('+') || key.contains(char::is_whitespace) || lower.starts_with("mouse") || lower == "escape" || lower == "backspace" {
            bail!("{}: клавиша {key:?} недопустима: нужно имя одной клавиши без модификаторов; Escape, BackSpace и кнопки мыши заняты", at());
        }
        if node.keys.iter().any(|k| k.key.eq_ignore_ascii_case(key)) {
            bail!("{}: клавиша {key:?} задана дважды", at());
        }
        let mut fields: Vec<&str> = v.as_bind("").action_fields();
        if v.state.is_some() {
            fields.push("state");
        }
        if fields.len() > 1 {
            bail!("{}: клавиша {key:?}: задано несколько действий ({}), допустимо одно", at(), fields.join(", "));
        }
        let path = chain_compact(&node.combo_chain(Combo { mods: Vec::new(), key: key.clone() }));
        if let Some(target) = &v.state {
            if v.exit {
                bail!("{}: клавиша {key:?}: exit не сочетается с переходом state", at());
            }
            if !st.states.contains_key(target) {
                bail!("{}: клавиша {key:?} ведёт в состояние {target:?}, которого нет среди дочерних состояний этого состояния", at());
            }
            node.keys.push(StickyBind {
                key: key.clone(),
                mods: Vec::new(),
                flags: Flags::default(),
                go: StickyGo::State(target.clone()),
                exit: false,
                desc: v.desc.clone(),
                inherited: None,
                explicit: None,
                binding: None,
            });
        } else if fields.is_empty() {
            // Дополнение унаследованной клавиши: признак выхода и описание.
            let Some(i) = rest.iter().position(|k| k.key == lower) else {
                bail!("{}: клавиша {key:?} без действия: такой клавиши нет среди унаследованных клавиш приложений", at());
            };
            let mut k = rest.remove(i);
            k.exit = v.exit;
            k.desc = v.desc.clone();
            node.keys.push(k);
        } else {
            let b = v.as_bind(&path);
            cfg.check_action(&b)?;
            let act = bind_act(&b, true)?;
            rest.retain(|k| k.key != lower);
            node.keys.push(StickyBind {
                key: key.clone(),
                mods: Vec::new(),
                flags: Flags::default(),
                go: StickyGo::Act(act),
                exit: v.exit,
                desc: v.desc.clone(),
                inherited: None,
                explicit: Some(b),
                binding: None,
            });
        }
    }
    node.keys.extend(rest);
    if node.keys.is_empty() {
        bail!("{}: в состоянии нет ни одной клавиши", at());
    }
    for child in st.states.keys() {
        if !sticky_name(child) {
            bail!("{}: имя состояния {child:?} должно состоять из латинских строчных букв, цифр и дефиса", at());
        }
        if !node.keys.iter().any(|k| k.go == StickyGo::State(child.clone())) {
            bail!("{}: в состояние {child} не ведёт ни одна клавиша", at());
        }
    }
    let children: Vec<(String, Chain)> = node.keys.iter().filter_map(|k| match &k.go {
        StickyGo::State(s) => Some((s.clone(), node.key_chain(k))),
        StickyGo::Act(_) => None,
    }).collect();
    out.push(node);
    for (name, st_child) in &st.states {
        // Состояние может быть достижимо несколькими клавишами: путь
        // строится по первой из них.
        let Some((_, path)) = children.iter().find(|(s, _)| s == name) else { continue };
        let mut sub_names = names.clone();
        sub_names.push(name.clone());
        sticky_state(cfg, inherited, chain, st_child, sub_names, titles.clone(), path.clone(), Some(submap.clone()), out)?;
    }
    Ok(())
}

fn node_label(names: &[String]) -> String {
    match names.last() {
        Some(n) => format!("состояние {n}"),
        None => "корневое состояние".to_string(),
    }
}

/// Проверки: одна цепочка не может совпадать с другой или быть её началом;
/// зарезервированные сочетания заняты быть не могут; `mouse` — только у
/// одиночного сочетания с одним диспетчером.
pub fn check_chains(cfg: &Config) -> Result<()> {
    let binds = collect(cfg)?;
    let reserved: Vec<(Combo, String)> = cfg.keys.reserved.iter().map(|r| parse_combo(r).map(|c| (c, r.clone()))).collect::<Result<_>>()?;
    for b in &binds {
        for combo in &b.chain {
            if let Some((_, text)) = reserved.iter().find(|(r, _)| r.cmp_key() == combo.cmp_key()) {
                bail!("привязка {:?} ({}) занимает зарезервированное сочетание {text:?}", chain_compact(&b.chain), b.source);
            }
        }
        if b.flags.mouse {
            if b.chain.len() != 1 {
                bail!("привязка {:?} ({}): флаг mouse допустим только у одиночного сочетания", chain_compact(&b.chain), b.source);
            }
            if !matches!(&b.act, Act::Dispatch(v) if v.len() == 1) {
                bail!("привязка {:?} ({}): флаг mouse требует одного выражения dispatch", chain_compact(&b.chain), b.source);
            }
        }
    }
    let keyed: Vec<Vec<(Vec<String>, String)>> = binds.iter().map(|b| b.chain.iter().map(Combo::cmp_key).collect()).collect();
    for (i, a) in binds.iter().enumerate() {
        for (j, b) in binds.iter().enumerate().skip(i + 1) {
            let (short, long, sk, lk) = if a.chain.len() <= b.chain.len() { (a, b, &keyed[i], &keyed[j]) } else { (b, a, &keyed[j], &keyed[i]) };
            if lk.starts_with(sk) {
                if short.chain.len() == long.chain.len() {
                    bail!("цепочка {:?} задана дважды: {} и {}", chain_compact(&short.chain), short.source, long.source);
                }
                bail!(
                    "цепочка {:?} ({}) является началом цепочки {:?} ({})",
                    chain_compact(&short.chain),
                    short.source,
                    chain_compact(&long.chain),
                    long.source
                );
            }
        }
    }
    // Состояния цепочек с выходом и автоматических режимов проверяются после
    // совпадения цепочек: так ошибка начала цепочки называет обе записи.
    sticky_nodes(cfg)?;
    Ok(())
}

fn lua_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
}

/// Выражение действия для `hl.bind`: диспетчер или функция. `reset` — сбросить
/// подкарту после действия (для последнего звена цепочки).
fn action_lua(act: &Act, chain: &Chain, reset: bool) -> String {
    let reset_line = "hl.dispatch(hl.dsp.submap(\"reset\"))";
    match act {
        Act::Daemon(cmd) | Act::Exec(cmd) => {
            if reset {
                format!("ws_run({})", lua_str(cmd))
            } else {
                format!("hl.dsp.exec_cmd({})", lua_str(cmd))
            }
        }
        Act::Dispatch(exprs) => {
            if exprs.len() == 1 && !reset {
                format!("hl.dsp.{}", exprs[0])
            } else {
                let mut body: Vec<String> = exprs.iter().map(|e| format!("hl.dispatch(hl.dsp.{e})")).collect();
                if reset {
                    body.push(reset_line.to_string());
                }
                format!("function() {} end", body.join("; "))
            }
        }
        Act::Lua(body) => {
            let name = chain_compact(chain);
            let mut s = String::new();
            s.push_str("function()\n");
            s.push_str("  local ok, err = pcall(function()\n");
            for line in body.lines() {
                writeln!(s, "    {line}").unwrap();
            }
            s.push_str("  end)\n");
            writeln!(s, "  if not ok then hl.notification.create({{ text = \"workspaced: привязка {name}: \" .. tostring(err), timeout = 5000, icon = \"error\" }}) end").unwrap();
            if reset {
                writeln!(s, "  {reset_line}").unwrap();
            }
            s.push_str("end");
            s
        }
        Act::Sticky(name) => format!("hl.dsp.submap({})", lua_str(&format!("{STICKY_PREFIX}{name}"))),
    }
}

/// Код Lua: прямые привязки одиночных сочетаний, вход в режимы и подкарты
/// состояний режимов для конфига Hyprland.
pub fn to_lua(cfg: &Config) -> Result<String> {
    let binds = collect(cfg)?;
    check_chains(cfg)?;
    let nodes = sticky_nodes(cfg)?;
    let mut out = String::new();
    out.push_str("-- Сгенерировано командой `workspaced keys --lua`, не править руками.\n");
    out.push_str("-- Источник: ~/.config/workspaced/config.toml\n");
    out.push_str("local function ws_run(cmd)\n  return function()\n    hl.dispatch(hl.dsp.exec_cmd(cmd))\n    hl.dispatch(hl.dsp.submap(\"reset\"))\n  end\nend\n");
    // Прямые привязки в порядке конфига; первое сочетание многозвенной
    // цепочки — один раз, при первом появлении: оно открывает корень
    // автоматического режима.
    let mut entered: Vec<(Vec<String>, String)> = Vec::new();
    for b in &binds {
        let combo = &b.chain[0];
        if b.chain.len() == 1 {
            let action = action_lua(&b.act, &b.chain, false);
            if b.flags.any() {
                writeln!(out, "hl.bind({}, {action}, {})", lua_str(&combo.lua()), b.flags.lua()).unwrap();
            } else {
                writeln!(out, "hl.bind({}, {action})", lua_str(&combo.lua())).unwrap();
            }
        } else if !entered.contains(&combo.cmp_key()) {
            entered.push(combo.cmp_key());
            let Some(root) = nodes.iter().find(|n| n.auto && n.names.is_empty() && n.prefix[0].cmp_key() == combo.cmp_key()) else {
                bail!("нет режима для цепочки {:?}", chain_compact(&b.chain));
            };
            writeln!(out, "hl.bind({}, hl.dsp.submap({}))", lua_str(&combo.lua()), lua_str(&root.submap)).unwrap();
        }
    }
    let mut names: Vec<String> = Vec::new();
    for node in &nodes {
        emit_sticky(node, &mut out);
        names.push(node.submap.clone());
    }
    // Подкарта, открытая до перечитывания конфига, которой в новом коде нет,
    // осталась бы без привязок, в том числе без Escape, и все сочетания сессии
    // перестали бы работать (решение D9 изменения sticky-chains): такая
    // подкарта закрывается. Префикс `ws:` остался от одноразовых подкарт
    // прежних версий. Проверка обёрнута в pcall, чтобы сбой в ней не лишал
    // сессию привязок.
    out.push_str("do\n  local known = {\n");
    for n in &names {
        writeln!(out, "    [{}] = true,", lua_str(n)).unwrap();
    }
    out.push_str("  }\n");
    out.push_str("  pcall(function()\n");
    out.push_str("    local cur = hl.get_current_submap()\n");
    writeln!(
        out,
        "    if type(cur) == \"string\" and (cur:sub(1, 3) == \"ws:\" or cur:sub(1, {}) == {}) and not known[cur] then",
        STICKY_PREFIX.len(),
        lua_str(STICKY_PREFIX)
    )
    .unwrap();
    out.push_str("      hl.dispatch(hl.dsp.submap(\"reset\"))\n    end\n  end)\nend\n");
    Ok(out)
}

/// Подкарта состояния режима (решения D3, D4, D9 изменения sticky-chains
/// и D2 изменения chains-sticky): клавиши состояния, BackSpace к родителю
/// (в корне — выход), Escape и последним — `catchall` с пустым действием,
/// который поглощает остальные нажатия. Клавиша без модификаторов, BackSpace,
/// Escape и `catchall` привязываются с `ignore_mods`: они срабатывают и при
/// удержанном модификаторе сочетания входа. Звено с модификаторами
/// привязывается точно. Действие `catchall` должно оставаться пустым:
/// композитор выполняет его и вместе с совпавшей привязкой.
fn emit_sticky(node: &StickyNode, out: &mut String) {
    const FLAGS: &str = "{ ignore_mods = true }";
    writeln!(out, "hl.define_submap({}, function()", lua_str(&node.submap)).unwrap();
    for k in &node.keys {
        let action = match &k.go {
            StickyGo::State(s) => format!("hl.dsp.submap({})", lua_str(&format!("{}/{s}", node.submap))),
            StickyGo::Act(act) => action_lua(act, &node.key_chain(k), k.exit),
        };
        let action = action.replace('\n', "\n  ");
        let mut flags: Vec<String> = k.flags.names().iter().map(|n| format!("{n} = true")).collect();
        if k.mods.is_empty() {
            flags.push("ignore_mods = true".to_string());
        }
        let key = lua_str(&k.combo().lua());
        if flags.is_empty() {
            writeln!(out, "  hl.bind({key}, {action})").unwrap();
        } else {
            writeln!(out, "  hl.bind({key}, {action}, {{ {} }})", flags.join(", ")).unwrap();
        }
    }
    let back = match &node.parent {
        Some(p) => lua_str(p),
        None => lua_str("reset"),
    };
    writeln!(out, "  hl.bind(\"BackSpace\", hl.dsp.submap({back}), {FLAGS})").unwrap();
    writeln!(out, "  hl.bind(\"Escape\", hl.dsp.submap(\"reset\"), {FLAGS})").unwrap();
    writeln!(out, "  hl.bind(\"catchall\", hl.dsp.no_op(), {FLAGS})").unwrap();
    out.push_str("end)\n");
}

/// Таблица привязок для просмотра: цепочка, действие, флаги, источник.
pub fn to_list(cfg: &Config) -> Result<String> {
    let binds = collect(cfg)?;
    check_chains(cfg)?;
    let nodes = sticky_nodes(cfg)?;
    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    for b in &binds {
        // Последнее звено многозвенной цепочки — клавиша выхода её режима
        // (изменение chains-sticky).
        let mut flags = b.flags.names();
        if b.chain.len() > 1 {
            flags.push("exit");
        }
        rows.push((chain_compact(&b.chain), b.act.describe(), flags.join(","), b.source.split(' ').next().unwrap_or("").to_string()));
        // Цепочка с выходом — строками путей сразу после сочетания входа
        // (решение D10).
        if let Act::Sticky(name) = &b.act {
            for r in sticky_rows(&nodes, name) {
                rows.push((chain_compact(&r.chain), r.action, if r.exit { "exit".to_string() } else { String::new() }, r.source));
            }
        }
    }
    let w0 = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
    let w1 = rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(0);
    let w2 = rows.iter().map(|r| r.2.chars().count()).max().unwrap_or(0);
    let mut out = String::new();
    for (chain, act, flags, source) in rows {
        let line = format!("{chain:<w0$}  {act:<w1$}  {flags:<w2$}  {source}");
        out.push_str(line.trim_end());
        out.push('\n');
    }
    Ok(out)
}

/// Строка пути цепочки с выходом для `keys --list` и `keys --json`.
#[derive(Debug, Clone, PartialEq)]
pub struct StickyRow {
    /// Путь: вход, переходы и клавиша.
    pub chain: Chain,
    /// `state raise`, `key SUPER+V`, `key --new SUPER+V`, `exec …`.
    pub action: String,
    pub exit: bool,
    /// Раздел конфига состояния, которому принадлежит клавиша.
    pub source: String,
    /// Номер состояния в списке `sticky_nodes` и номер клавиши в нём.
    pub node: usize,
    pub key: usize,
}

/// Строки путей цепочки `name`: клавиши каждого состояния по порядку
/// обхода состояний.
pub fn sticky_rows(nodes: &[StickyNode], name: &str) -> Vec<StickyRow> {
    let mut out = Vec::new();
    for (ni, node) in nodes.iter().enumerate().filter(|(_, n)| n.chain == name) {
        for (ki, k) in node.keys.iter().enumerate() {
            let action = match &k.go {
                StickyGo::State(s) => format!("state {s}"),
                StickyGo::Act(a) => a.describe(),
            };
            out.push(StickyRow { chain: node.key_chain(k), action, exit: k.exit, source: node.source.clone(), node: ni, key: ki });
        }
    }
    out
}

/// Путь копии последнего удачного кода привязок.
pub fn cache_path() -> std::path::PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/state"))
        .join("workspaced")
        .join("keys.lua")
}

/// Записать копию кода атомарно: временный файл и переименование.
pub fn write_cache(code: &str) -> Result<()> {
    let path = cache_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("lua.tmp");
    std::fs::write(&tmp, code)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> String {
        r#"
[templates.t]
main = "c"
cells = { c = { x = 0, y = 0, w = 10, h = 10 }, l = { x = 0, y = 0, w = 5, h = 10 } }
[apps.firefox-front]
cmd = "firefox"
chain = "SUPER+A b"
[apps.firefox-chat]
cmd = "firefox"
chain = "SUPER+A b"
[apps.firefox-back]
cmd = "firefox"
[workspaces.dev-front]
template = "t"
chain = "SUPER+W f"
apps = { firefox-front = "c" }
[workspaces.chat]
template = "t"
apps = { firefox-chat = "c" }
"#
        .to_string()
    }

    /// Тело подкарты `name` в коде Lua.
    fn sticky_block(lua: &str, name: &str) -> String {
        let head = format!("hl.define_submap(\"{name}\", function()\n");
        let start = lua.find(&head).unwrap_or_else(|| panic!("нет подкарты {name}:\n{lua}"));
        let end = lua[start..].find("end)\n").unwrap() + start;
        lua[start..end].to_string()
    }

    fn luac(code: &str) {
        let Ok(mut child) = std::process::Command::new("luac")
            .args(["-p", "-"])
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        else {
            return; // luac не установлен: синтаксис проверит только Hyprland
        };
        use std::io::Write as _;
        child.stdin.take().unwrap().write_all(code.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "luac: {}\n{code}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn parse_chain_normalizes() {
        let c = parse_chain("shift+SUPER+W  3 f").unwrap();
        assert_eq!(c.iter().map(Combo::lua).collect::<Vec<_>>(), vec!["SUPER + SHIFT + W", "3", "f"]);
        assert_eq!(chain_compact(&c), "SUPER+SHIFT+W 3 f");
    }

    #[test]
    fn same_chain_in_different_workspaces_ok() {
        Config::parse(&base()).unwrap();
    }

    #[test]
    fn same_chain_in_one_workspace_is_error() {
        let text = base()
            .replace("[apps.firefox-back]\ncmd = \"firefox\"", "[apps.firefox-back]\ncmd = \"firefox\"\nchain = \"SUPER+A b\"")
            .replace("apps = { firefox-front = \"c\" }", "apps = { firefox-front = \"c\", firefox-back = \"l\" }");
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("dev-front") && err.contains("firefox-front") && err.contains("firefox-back"), "{err}");
    }

    /// Браузеры с собственными клавишами и workspace `surf`, который
    /// переопределяет клавишу `chrome-ai` (изменение workspace-overrides).
    fn overrides() -> String {
        r#"
[templates.t]
main = "c"
cells = { c = { x = 0, y = 0, w = 10, h = 10 }, l = { x = 0, y = 0, w = 5, h = 10 }, r = { x = 5, y = 0, w = 5, h = 10 } }
[apps.chrome]
cmd = "chrome"
chain = "SUPER+B"
[apps.chrome-ai]
cmd = "chrome"
chain = "SUPER+SHIFT+V"
[workspaces.surf]
template = "t"
chain = "SUPER+TAB s"
[workspaces.surf.apps]
chrome = "l"
chrome-ai = { cell = "r", chain = "SUPER+V" }
"#
        .to_string()
    }

    #[test]
    fn override_keys_collected() {
        let cfg = Config::parse(&overrides()).unwrap();
        let binds = collect(&cfg).unwrap();
        let by = |c: &str| binds.iter().find(|b| chain_compact(&b.chain) == c).unwrap_or_else(|| panic!("нет привязки {c}: {binds:?}"));
        assert_eq!(by("SUPER+V").act, Act::Daemon("workspaced key 'SUPER+V'".into()));
        assert_eq!(by("SUPER+V").source, "workspaces.surf.apps.chrome-ai");
        assert_eq!(by("SUPER+SHIFT+V").act, Act::Daemon("workspaced key 'SUPER+SHIFT+V'".into()));
        assert_eq!(by("SUPER+SHIFT+V").source, "apps.chrome-ai");
        let list = to_list(&cfg).unwrap();
        assert!(list.lines().any(|l| l.starts_with("SUPER+V ") && l.contains("key SUPER+V") && l.ends_with("workspaces.surf.apps.chrome-ai")), "{list}");
        assert!(list.lines().any(|l| l.starts_with("SUPER+SHIFT+V ") && l.contains("key SUPER+SHIFT+V") && l.ends_with("apps.chrome-ai")), "{list}");
        let lua = to_lua(&cfg).unwrap();
        luac(&lua);
        assert!(lua.contains("hl.bind(\"SUPER + V\", hl.dsp.exec_cmd(\"workspaced key 'SUPER+V'\"))"), "{lua}");
        // Цепочка из нескольких звеньев — в кавычках, одним словом команды.
        let text = overrides().replace("chain = \"SUPER+V\"", "chain = \"SUPER+TAB v\"");
        let lua = to_lua(&Config::parse(&text).unwrap()).unwrap();
        luac(&lua);
        assert!(lua.contains("ws_run(\"workspaced key 'SUPER+TAB v'\")"), "{lua}");
        // Одна цепочка у собственной клавиши и у ключа другого workspace — одна
        // привязка с двумя источниками.
        let text = overrides() + "[apps.mail]\ncmd = \"mail\"\nchain = \"super+v\"\n";
        let cfg = Config::parse(&text).unwrap();
        let binds = collect(&cfg).unwrap();
        let v: Vec<&Binding> = binds.iter().filter(|b| chain_key(&b.chain) == chain_key(&parse_chain("SUPER+V").unwrap())).collect();
        assert_eq!(v.len(), 1, "{binds:?}");
        assert_eq!(v[0].source, "apps.mail,workspaces.surf.apps.chrome-ai");
    }

    #[test]
    fn override_frees_own_key() {
        // Ключи в surf различаются: chrome — Super+V, chrome-ai — Super+B.
        let text = overrides()
            .replace("chain = \"SUPER+SHIFT+V\"", "chain = \"SUPER+V\"")
            .replace("chrome = \"l\"", "chrome = { cell = \"l\", chain = \"SUPER+V\" }")
            .replace("chrome-ai = { cell = \"r\", chain = \"SUPER+V\" }", "chrome-ai = { cell = \"r\", chain = \"SUPER+B\" }");
        let cfg = Config::parse(&text).unwrap();
        assert_eq!(cfg.responds("surf", "SUPER+V"), Some("chrome".into()));
        assert_eq!(cfg.responds("surf", "SUPER+B"), Some("chrome-ai".into()));
    }

    #[test]
    fn override_conflicts_in_workspace() {
        let text = overrides().replace("chain = \"SUPER+V\"", "chain = \"SUPER+B\"");
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("surf") && err.contains("chrome") && err.contains("chrome-ai") && err.contains("SUPER+B"), "{err}");
    }

    #[test]
    fn override_conflicts_with_workspace_chain() {
        let text = overrides().replace("chain = \"SUPER+V\"", "chain = \"SUPER+TAB s\"");
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("workspaces.surf.apps.chrome-ai") && err.contains("workspaces.surf") && err.contains("SUPER+TAB s"), "{err}");
        // Зарезервированное сочетание в переопределении тоже недопустимо.
        let text = overrides() + "[keys]\nreserved = [\"SUPER+V\"]\n";
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("workspaces.surf.apps.chrome-ai") && err.contains("зарезервированное"), "{err}");
    }

    #[test]
    fn two_save_chains_share_a_prefix() {
        // Общий первый сегмент у двух цепочек допустим: обе — клавиши выхода
        // одного режима, и ни одна не является началом другой.
        let text = base() + "\n[keys]\nsave_session = \"CTRL+SUPER+s CTRL+SUPER+s\"\nsave_workspace = \"CTRL+SUPER+s CTRL+SUPER+w\"\n";
        let cfg = Config::parse(&text).unwrap();
        let lua = to_lua(&cfg).unwrap();
        luac(&lua);
        // Модификаторы приводятся к каноническому порядку: SUPER, CTRL, ALT, SHIFT.
        assert!(lua.contains("hl.bind(\"SUPER + CTRL + s\", hl.dsp.submap(\"ws-sticky:SUPER+CTRL+s\"))"), "{lua}");
        // Звено с модификаторами привязывается точно, без ignore_mods:
        // срабатывает, пока Ctrl и Super удерживаются после входа.
        let block = sticky_block(&lua, "ws-sticky:SUPER+CTRL+s");
        assert!(block.contains("  hl.bind(\"SUPER + CTRL + s\", ws_run(\"workspaced save-session\"))\n"), "{block}");
        assert!(block.contains("  hl.bind(\"SUPER + CTRL + w\", ws_run(\"workspaced save-workspace\"))\n"), "{block}");
        assert!(block.trim_end().ends_with("hl.bind(\"catchall\", hl.dsp.no_op(), { ignore_mods = true })"), "{block}");
        assert!(!lua.contains("\"ws:S"), "{lua}");
        let list = to_list(&cfg).unwrap();
        let words = |c: &str| list.lines().find(|l| l.starts_with(&format!("{c} "))).map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
        assert_eq!(words("SUPER+CTRL+s SUPER+CTRL+s"), "SUPER+CTRL+s SUPER+CTRL+s save-session exit keys.save_session", "{list}");
        assert_eq!(words("SUPER+CTRL+s SUPER+CTRL+w"), "SUPER+CTRL+s SUPER+CTRL+w save-workspace exit keys.save_workspace", "{list}");
        assert!(!list.contains("SUPER+TAB"), "{list}");

        // Действие сохранения сессии доступно и записью [[binds]].
        let text = base() + "\n[[binds]]\nchain = \"SUPER+K\"\naction = \"save-session\"\n";
        assert!(to_list(&Config::parse(&text).unwrap()).unwrap().contains("save-session"));
        let bad = base() + "\n[[binds]]\nchain = \"SUPER+K\"\naction = \"save-sesion\"\n";
        assert!(Config::parse(&bad).unwrap_err().to_string().contains("save-sesion"));
    }

    #[test]
    fn prefix_chain_is_error() {
        let text = base() + "\n[[binds]]\nchain = \"SUPER+W f 3\"\naction = { desktop = 3, workspace = \"dev-front\" }\n";
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("SUPER+W f") && err.contains("SUPER+W f 3"), "{err}");
    }

    #[test]
    fn single_combo_conflicts_with_chain_start() {
        let text = base() + "\n[[binds]]\nchain = \"super+w\"\nexec = \"wezterm-gui\"\n";
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("SUPER+w") && err.contains("SUPER+W f") && err.contains("binds[0]"), "{err}");
    }

    #[test]
    fn two_actions_is_error() {
        let text = base() + "\n[[binds]]\nchain = \"SUPER+Return\"\nexec = \"wezterm-gui\"\ndispatch = \"window.close()\"\n";
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("SUPER+Return") && err.contains("exec") && err.contains("dispatch"), "{err}");
        let text = base() + "\n[[binds]]\nchain = \"SUPER+Return\"\n";
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("нужно одно из полей"), "{err}");
    }

    #[test]
    fn reserved_is_error() {
        let text = base() + "\n[keys]\nreserved = [\"SUPER+space\", \"ALT+E\"]\n[[binds]]\nchain = \"alt+e\"\nexec = \"true\"\n";
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("ALT+e") && err.contains("\"ALT+E\"") && err.contains("зарезервированное"), "{err}");
        // Зарезервированное сочетание внутри цепочки тоже недопустимо.
        let text = base() + "\n[keys]\nreserved = [\"SUPER+T\"]\n[[binds]]\nchain = \"SUPER+W SUPER+T\"\nexec = \"true\"\n";
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn mouse_only_single_dispatch() {
        let text = base() + "\n[[binds]]\nchain = \"SUPER+W mouse:272\"\nmouse = true\ndispatch = \"window.drag()\"\n";
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("mouse") && err.contains("одиночного"), "{err}");
        let text = base() + "\n[[binds]]\nchain = \"SUPER+mouse:272\"\nmouse = true\nexec = \"true\"\n";
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("одного выражения dispatch"), "{err}");
        let text = base() + "\n[[binds]]\nchain = \"SUPER+mouse:272\"\nmouse = true\ndispatch = \"window.drag()\"\n";
        let lua = to_lua(&Config::parse(&text).unwrap()).unwrap();
        assert!(lua.contains("hl.bind(\"SUPER + mouse:272\", hl.dsp.window.drag(), { mouse = true })"), "{lua}");
    }

    #[test]
    fn move_desktop_and_arrange_actions() {
        // Перенос workspace на стол: серия Ctrl+Super+1…8 из одной записи,
        // номер стола подставляется в действие так же, как в цепочку.
        let text = base() + "\n[[binds]]\nchain = \"CTRL+SUPER+$n\"\nrange = [1, 8]\naction = { move = \"$n\" }\n";
        let cfg = Config::parse(&text).unwrap();
        let binds = collect(&cfg).unwrap();
        let series: Vec<&Binding> = binds.iter().filter(|b| b.source.starts_with("binds[0]")).collect();
        assert_eq!(series.len(), 8);
        assert_eq!(chain_compact(&series[2].chain), "SUPER+CTRL+3");
        assert_eq!(series[2].act, Act::Daemon("workspaced move-desktop 3".to_string()));
        // Число в действии тоже допустимо.
        let one = base() + "\n[[binds]]\nchain = \"CTRL+SUPER+F1\"\naction = { move = 2 }\n";
        assert!(to_list(&Config::parse(&one).unwrap()).unwrap().contains("move-desktop 2"));
        // Стол вне 1…8 отклоняется с указанием цепочки.
        let bad = base() + "\n[[binds]]\nchain = \"CTRL+SUPER+F1\"\naction = { move = 9 }\n";
        let err = Config::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("CTRL+SUPER+F1") && err.contains("9") && err.contains("от 1 до 8"), "{err}");

        // Расстановка окон по конфигу: служебное действие строкой.
        let text = base() + "\n[[binds]]\nchain = \"CTRL+SUPER+space\"\naction = \"arrange\"\n";
        let cfg = Config::parse(&text).unwrap();
        let lua = to_lua(&cfg).unwrap();
        luac(&lua);
        assert!(lua.contains("hl.bind(\"SUPER + CTRL + space\", hl.dsp.exec_cmd(\"workspaced arrange\"))"), "{lua}");
        let bad = base() + "\n[[binds]]\nchain = \"CTRL+SUPER+space\"\naction = \"arange\"\n";
        assert!(Config::parse(&bad).unwrap_err().to_string().contains("arange"));
    }

    #[test]
    fn range_expands() {
        let text = base() + "\n[[binds]]\nchain = \"SUPER+$n\"\nrange = [1, 8]\ndispatch = \"focus({ workspace = $n })\"\n";
        let cfg = Config::parse(&text).unwrap();
        let binds = collect(&cfg).unwrap();
        let series: Vec<&Binding> = binds.iter().filter(|b| b.source.starts_with("binds[0]")).collect();
        assert_eq!(series.len(), 8);
        assert_eq!(chain_compact(&series[2].chain), "SUPER+3");
        assert_eq!(series[2].act, Act::Dispatch(vec!["focus({ workspace = 3 })".to_string()]));
        // Без $n в цепочке серия дала бы дубли: это ошибка конфига.
        let text = base() + "\n[[binds]]\nchain = \"SUPER+X\"\nrange = [1, 2]\nexec = \"true\"\n";
        assert!(Config::parse(&text).unwrap_err().to_string().contains("$n"));
    }

    #[test]
    fn lua_for_each_action_kind() {
        let text = base()
            + r#"
[keys]
sessions = "SUPER+S"
[[binds]]
chain = "SUPER+Return"
exec = "wezterm-gui"
[[binds]]
chain = "SUPER+C"
dispatch = "window.close()"
[[binds]]
chain = "ALT+Tab"
dispatch = ["window.cycle_next({ next = true })", "window.bring_to_top()"]
[[binds]]
chain = "XF86AudioRaiseVolume"
exec = "~/.scripts/change-volume.sh +"
locked = true
repeating = true
[[binds]]
chain = "SUPER+SHIFT+left"
action = { half = "left" }
[[binds]]
chain = "SUPER+X"
action = "maximize"
[[binds]]
chain = "ALT+SUPER+Home"
action = { place = "top-left" }
[[binds]]
chain = "SUPER+SHIFT+C"
lua = """
local soft = hl.get_config("cursor.no_hardware_cursors")
hl.config({ ["cursor.no_hardware_cursors"] = not soft })
"""
[[binds]]
chain = "SUPER+W 3 f"
action = { desktop = 3, workspace = "dev-front" }
[[binds]]
chain = "SUPER+W x"
dispatch = "exit()"
"#;
        let cfg = Config::parse(&text).unwrap();
        let a = to_lua(&cfg).unwrap();
        let b = to_lua(&cfg).unwrap();
        assert_eq!(a, b);
        luac(&a);
        assert!(a.contains("hl.bind(\"SUPER + S\", hl.dsp.exec_cmd(\"workspaced sessions\"))"), "{a}");
        assert!(a.contains("hl.bind(\"SUPER + Return\", hl.dsp.exec_cmd(\"wezterm-gui\"))"), "{a}");
        assert!(a.contains("hl.bind(\"SUPER + C\", hl.dsp.window.close())"), "{a}");
        assert!(a.contains("hl.bind(\"ALT + Tab\", function() hl.dispatch(hl.dsp.window.cycle_next({ next = true })); hl.dispatch(hl.dsp.window.bring_to_top()) end)"), "{a}");
        assert!(a.contains("hl.bind(\"XF86AudioRaiseVolume\", hl.dsp.exec_cmd(\"~/.scripts/change-volume.sh +\"), { locked = true, repeating = true })"), "{a}");
        assert!(a.contains("hl.bind(\"SUPER + SHIFT + left\", hl.dsp.exec_cmd(\"workspaced half left\"))"), "{a}");
        assert!(a.contains("hl.bind(\"SUPER + X\", hl.dsp.exec_cmd(\"workspaced maximize\"))"), "{a}");
        assert!(a.contains("Home\", hl.dsp.exec_cmd(\"workspaced place top-left\"))"), "{a}");
        assert!(a.contains("hl.bind(\"SUPER + SHIFT + C\", function()\n  local ok, err = pcall(function()\n    local soft"), "{a}");
        // Цепочка из трёх звеньев — режим с вложенным состоянием.
        let w = sticky_block(&a, "ws-sticky:SUPER+W");
        assert!(w.contains("hl.bind(\"3\", hl.dsp.submap(\"ws-sticky:SUPER+W/3\"), { ignore_mods = true })"), "{w}");
        assert!(w.contains("hl.bind(\"f\", ws_run(\"workspaced raise dev-front\"), { ignore_mods = true })"), "{w}");
        let w3 = sticky_block(&a, "ws-sticky:SUPER+W/3");
        assert!(w3.contains("hl.bind(\"f\", ws_run(\"workspaced raise dev-front --desktop 3\"), { ignore_mods = true })"), "{w3}");
        assert!(w3.contains("hl.bind(\"BackSpace\", hl.dsp.submap(\"ws-sticky:SUPER+W\"), { ignore_mods = true })"), "{w3}");
        assert!(a.contains("ws_run(\"workspaced key 'SUPER+A b'\")"), "{a}");
        assert!(a.contains("hl.bind(\"SUPER + A\", hl.dsp.submap(\"ws-sticky:SUPER+A\"))"), "{a}");
        // Первое сочетание открывает режим один раз.
        assert_eq!(a.matches("hl.bind(\"SUPER + W\", ").count(), 1, "{a}");
        // Диспетчер в конце цепочки закрывает режим.
        assert!(a.contains("hl.bind(\"x\", function() hl.dispatch(hl.dsp.exit()); hl.dispatch(hl.dsp.submap(\"reset\")) end, { ignore_mods = true })"), "{a}");
        assert!(!a.contains("\"ws:S"), "{a}");
        let list = to_list(&cfg).unwrap();
        assert!(list.contains("SUPER+Return") && list.contains("exec wezterm-gui") && list.contains("binds[1]"), "{list}");
        assert!(list.contains("SUPER+W f") && list.contains("raise dev-front") && list.contains("workspaces.dev-front"), "{list}");
        assert!(list.contains("locked,repeating"), "{list}");
        assert!(list.contains("SUPER+X") && list.contains("maximize"), "{list}");
        assert!(list.contains("Home") && list.contains("place top-left"), "{list}");
    }

    /// Приложения сессии: собственные клавиши herdr, Chromium и neovide,
    /// ключи браузеров в `surf` и цепочка `[sticky.apps]` из решения D1.
    fn sticky_base() -> String {
        r#"
[keys]
sessions = "SHIFT+SUPER+S"
reserved = ["SUPER+space"]
[templates.t]
main = "c"
cells = { c = { x = 0, y = 0, w = 10, h = 10 }, l = { x = 0, y = 0, w = 5, h = 10 }, r = { x = 5, y = 0, w = 5, h = 10 } }
[apps.herdr]
cmd = "wezterm"
chain = "SUPER+T"
[apps.chromium]
cmd = "chromium"
chain = "SUPER+C"
[apps.neovide]
cmd = "neovide"
chain = "SUPER+E"
[apps.chrome]
cmd = "chrome"
chain = "SUPER+SHIFT+B"
[apps.chrome-ai]
cmd = "chrome"
chain = "SUPER+SHIFT+V"
[apps.yandex]
cmd = "yandex"
chain = "SUPER+SHIFT+Y"
[workspaces.surf]
template = "t"
chain = "SUPER+TAB s"
[workspaces.surf.apps]
chrome = { cell = "l", chain = "SUPER+B" }
chrome-ai = { cell = "r", chain = "SUPER+V" }
yandex = { cell = "c", chain = "SUPER+Y" }
"#
        .to_string()
    }

    fn sticky_apps() -> String {
        r#"
[sticky.apps]
enter = "SUPER+S"
title = "Приложения"
[sticky.apps.keys]
r = { state = "raise" }
s = { state = "spawn" }
[sticky.apps.states.raise]
title = "Поднять"
apps = "raise"
[sticky.apps.states.raise.keys]
e = { exit = true }
[sticky.apps.states.spawn]
title = "Новое окно"
apps = "spawn"
[sticky.apps.states.spawn.keys]
e = { exit = true }
"#
        .to_string()
    }

    fn sticky_cfg() -> Config {
        Config::parse(&(sticky_base() + &sticky_apps())).unwrap()
    }

    fn sticky_err(extra: &str) -> String {
        Config::parse(&(sticky_base() + extra)).unwrap_err().to_string()
    }

    #[test]
    fn sticky_nodes_inherit_app_keys() {
        let cfg = sticky_cfg();
        let nodes = sticky_nodes(&cfg).unwrap();
        let subs: Vec<&str> = nodes.iter().map(|n| n.submap.as_str()).collect();
        assert_eq!(subs, ["ws-sticky:apps", "ws-sticky:apps/raise", "ws-sticky:apps/spawn", "ws-sticky:SUPER+TAB"]);
        assert!(nodes[3].auto && !nodes[0].auto);
        assert_eq!(nodes[1].titles, ["Приложения", "Поднять"]);
        assert_eq!(nodes[1].parent.as_deref(), Some("ws-sticky:apps"));
        assert_eq!(nodes[1].source, "sticky.apps.raise");
        // Явные клавиши — первыми, затем унаследованные по порядку цепочек;
        // Shift+Super+буква не наследуется.
        let keys: Vec<&str> = nodes[1].keys.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(keys, ["e", "b", "c", "t", "v", "y"]);
        let e = &nodes[1].keys[0];
        assert!(e.exit);
        assert_eq!(e.go, StickyGo::Act(Act::Daemon("workspaced key 'SUPER+E'".into())));
        assert_eq!(e.inherited.as_deref(), Some("apps.neovide"));
        let v = nodes[2].keys.iter().find(|k| k.key == "v").unwrap();
        assert_eq!(v.go, StickyGo::Act(Act::Daemon("workspaced key --new 'SUPER+V'".into())));
        assert!(!v.exit);
    }

    #[test]
    fn sticky_list_rows() {
        let list = to_list(&sticky_cfg()).unwrap();
        let row = |chain: &str| list.lines().find(|l| l.split("  ").next().map(str::trim) == Some(chain)).unwrap_or_else(|| panic!("нет строки {chain}:\n{list}")).to_string();
        let words = |l: String| l.split_whitespace().map(String::from).collect::<Vec<_>>().join(" ");
        assert_eq!(words(row("SUPER+S")), "SUPER+S sticky apps sticky.apps");
        assert_eq!(words(row("SUPER+S r")), "SUPER+S r state raise sticky.apps");
        assert_eq!(words(row("SUPER+S r v")), "SUPER+S r v key SUPER+V sticky.apps.raise");
        assert_eq!(words(row("SUPER+S s v")), "SUPER+S s v key --new SUPER+V sticky.apps.spawn");
        assert_eq!(words(row("SUPER+S s e")), "SUPER+S s e key --new SUPER+E exit sticky.apps.spawn");
        assert_eq!(words(row("SUPER+S r e")), "SUPER+S r e key SUPER+E exit sticky.apps.raise");
        // Пути в проверке совпадения цепочек не участвуют: «SUPER+S r» —
        // не цепочка сессии, и Super+S остаётся одиночным сочетанием.
        assert!(!list.contains("SUPER+SHIFT+V exit"), "{list}");
    }

    #[test]
    fn sticky_lua() {
        let cfg = sticky_cfg();
        let lua = to_lua(&cfg).unwrap();
        luac(&lua);
        assert_eq!(lua, to_lua(&cfg).unwrap());
        assert!(lua.contains("hl.bind(\"SUPER + S\", hl.dsp.submap(\"ws-sticky:apps\"))"), "{lua}");
        let block = |name: &str| sticky_block(&lua, name);
        let root = block("ws-sticky:apps");
        assert!(root.contains("hl.bind(\"r\", hl.dsp.submap(\"ws-sticky:apps/raise\"), { ignore_mods = true })"), "{root}");
        assert!(root.contains("hl.bind(\"BackSpace\", hl.dsp.submap(\"reset\"), { ignore_mods = true })"), "{root}");
        let raise = block("ws-sticky:apps/raise");
        // Действие без выхода подкарту не закрывает, с выходом — закрывает.
        assert!(raise.contains("hl.bind(\"v\", hl.dsp.exec_cmd(\"workspaced key 'SUPER+V'\"), { ignore_mods = true })"), "{raise}");
        assert!(raise.contains("hl.bind(\"e\", ws_run(\"workspaced key 'SUPER+E'\"), { ignore_mods = true })"), "{raise}");
        assert!(raise.contains("hl.bind(\"BackSpace\", hl.dsp.submap(\"ws-sticky:apps\"), { ignore_mods = true })"), "{raise}");
        assert!(raise.contains("hl.bind(\"Escape\", hl.dsp.submap(\"reset\"), { ignore_mods = true })"), "{raise}");
        // catchall — последней привязкой подкарты и с пустым действием.
        assert!(raise.trim_end().ends_with("hl.bind(\"catchall\", hl.dsp.no_op(), { ignore_mods = true })"), "{raise}");
        // Три состояния цепочки apps и режим Super+Tab.
        assert_eq!(lua.matches("\"catchall\"").count(), 4, "{lua}");
        let spawn = block("ws-sticky:apps/spawn");
        assert!(spawn.contains("hl.bind(\"v\", hl.dsp.exec_cmd(\"workspaced key --new 'SUPER+V'\"), { ignore_mods = true })"), "{spawn}");
        // Многозвенная цепочка тоже даёт режим: catchall, BackSpace, Escape.
        let tab = block("ws-sticky:SUPER+TAB");
        assert!(tab.contains("hl.bind(\"s\", ws_run(\"workspaced raise surf\"), { ignore_mods = true })"), "{tab}");
        assert!(tab.contains("catchall") && tab.contains("\"BackSpace\"") && tab.contains("\"Escape\""), "{tab}");
        // Проверка устаревшей подкарты знает все подкарты этого кода.
        let tail = &lua[lua.rfind("local known").unwrap()..];
        for name in ["ws-sticky:SUPER+TAB", "ws-sticky:apps", "ws-sticky:apps/raise", "ws-sticky:apps/spawn"] {
            assert!(tail.contains(&format!("[\"{name}\"] = true")), "{tail}");
        }
        assert!(tail.contains("hl.get_current_submap()") && tail.contains("hl.dispatch(hl.dsp.submap(\"reset\"))"), "{tail}");
    }

    #[test]
    fn sticky_nested_and_explicit_keys() {
        // Вложенное состояние: BackSpace ведёт к родителю, клавиша lua без
        // выхода не закрывает подкарту, диспетчер с выходом закрывает.
        let extra = r#"
[sticky.win]
enter = "SUPER+W"
[sticky.win.keys]
m = { state = "move" }
[sticky.win.states.move]
[sticky.win.states.move.keys]
h = { action = { half = "left" } }
q = { dispatch = "window.close()", exit = true, desc = "Закрыть окно" }
d = { state = "deep" }
[sticky.win.states.move.states.deep]
[sticky.win.states.move.states.deep.keys]
x = { lua = "return" }
"#;
        let cfg = Config::parse(&(sticky_base() + extra)).unwrap();
        let lua = to_lua(&cfg).unwrap();
        luac(&lua);
        assert!(lua.contains("hl.define_submap(\"ws-sticky:win/move/deep\", function()"), "{lua}");
        assert!(lua.contains("hl.bind(\"BackSpace\", hl.dsp.submap(\"ws-sticky:win/move\"), { ignore_mods = true })"), "{lua}");
        assert!(lua.contains("hl.bind(\"h\", hl.dsp.exec_cmd(\"workspaced half left\"), { ignore_mods = true })"), "{lua}");
        assert!(lua.contains("hl.bind(\"q\", function() hl.dispatch(hl.dsp.window.close()); hl.dispatch(hl.dsp.submap(\"reset\")) end, { ignore_mods = true })"), "{lua}");
        let list = to_list(&cfg).unwrap();
        assert!(list.lines().any(|l| l.starts_with("SUPER+W m d x ") && l.ends_with("sticky.win.move.deep")), "{list}");
        assert!(list.lines().any(|l| l.starts_with("SUPER+W m q ") && l.contains("exit")), "{list}");
    }

    #[test]
    fn sticky_augment_and_replace() {
        let text = sticky_base() + &sticky_apps().replace("[sticky.apps.states.raise.keys]\ne = { exit = true }", "[sticky.apps.states.raise.keys]\ne = { exit = true }\nc = { exec = \"chromium --incognito\" }");
        let cfg = Config::parse(&text).unwrap();
        let list = to_list(&cfg).unwrap();
        assert!(list.lines().any(|l| l.starts_with("SUPER+S r e ") && l.contains("key SUPER+E") && l.contains("exit")), "{list}");
        assert!(list.lines().any(|l| l.starts_with("SUPER+S r c ") && l.contains("exec chromium --incognito")), "{list}");
        assert_eq!(list.lines().filter(|l| l.starts_with("SUPER+S r c ")).count(), 1, "{list}");
        // Клавиша без действия, которой нет среди унаследованных, — ошибка.
        let bad = sticky_base() + &sticky_apps().replace("[sticky.apps.states.raise.keys]\ne = { exit = true }", "[sticky.apps.states.raise.keys]\ne = { exit = true }\nq = { exit = true }");
        let err = Config::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("sticky.apps") && err.contains("raise") && err.contains("\"q\"") && err.contains("без действия"), "{err}");
    }

    #[test]
    fn sticky_errors() {
        // Вход совпадает с привязкой.
        let err = sticky_err(&sticky_apps().replace("SUPER+S\"\ntitle", "SHIFT+SUPER+S\"\ntitle"));
        assert!(err.contains("sticky.apps") && err.contains("keys.sessions"), "{err}");
        // Вход занимает зарезервированное сочетание.
        let err = sticky_err(&sticky_apps().replace("enter = \"SUPER+S\"", "enter = \"SUPER+space\""));
        assert!(err.contains("sticky.apps") && err.contains("\"SUPER+space\"") && err.contains("зарезервированное"), "{err}");
        // Вход — начало другой цепочки.
        let err = sticky_err(&sticky_apps().replace("enter = \"SUPER+S\"", "enter = \"SUPER+TAB\""));
        assert!(err.contains("sticky.apps") && err.contains("workspaces.surf"), "{err}");
        // Вход звеном многозвенной цепочки.
        let err = sticky_err(&sticky_apps().replace("enter = \"SUPER+S\"", "enter = \"SUPER+A b\""));
        assert!(err.contains("sticky.apps") && err.contains("одним сочетанием"), "{err}");
        // Недопустимые клавиши состояния.
        for key in ["Escape", "\"SHIFT+v\"", "BackSpace", "\"mouse:272\""] {
            let err = sticky_err(&sticky_apps().replace("s = { state = \"spawn\" }", &format!("s = {{ state = \"spawn\" }}\n{key} = {{ exec = \"true\" }}")));
            assert!(err.contains("sticky.apps") && err.contains("корневое состояние") && err.contains(key.trim_matches('"')), "{key}: {err}");
        }
        // Переход в чужое состояние.
        let err = sticky_err(&sticky_apps().replace("[sticky.apps.states.raise.keys]\ne = { exit = true }", "[sticky.apps.states.raise.keys]\ne = { exit = true }\ns = { state = \"spawn\" }"));
        assert!(err.contains("sticky.apps") && err.contains("состояние raise") && err.contains("spawn"), "{err}");
        // Недостижимое состояние.
        let err = sticky_err(&(sticky_apps() + "[sticky.apps.states.move]\napps = \"raise\"\n"));
        assert!(err.contains("sticky.apps") && err.contains("move") && err.contains("не ведёт"), "{err}");
        // Состояние без клавиш.
        let err = sticky_err(&(sticky_apps().replace("s = { state = \"spawn\" }", "s = { state = \"spawn\" }\nm = { state = \"move\" }") + "[sticky.apps.states.move]\ntitle = \"Пусто\"\n"));
        assert!(err.contains("состояние move") && err.contains("нет ни одной клавиши"), "{err}");
        // Недопустимое значение apps.
        let err = sticky_err(&sticky_apps().replace("apps = \"raise\"", "apps = \"lift\""));
        assert!(err.contains("raise, spawn") && err.contains("lift"), "{err}");
        // exit вместе с переходом.
        let err = sticky_err(&sticky_apps().replace("r = { state = \"raise\" }", "r = { state = \"raise\", exit = true }"));
        assert!(err.contains("exit") && err.contains("\"r\""), "{err}");
        // Два действия у одной клавиши.
        let err = sticky_err(&sticky_apps().replace("r = { state = \"raise\" }", "r = { state = \"raise\", exec = \"true\" }"));
        assert!(err.contains("несколько действий"), "{err}");
        // Имя цепочки и состояния — строчные латинские буквы, цифры и дефис.
        let err = sticky_err("[sticky.Apps]\nenter = \"SUPER+S\"\nkeys = { x = { exec = \"true\" } }\n");
        assert!(err.contains("sticky.Apps") && err.contains("строчных"), "{err}");
        // Нет сочетания входа.
        let err = sticky_err("[sticky.apps]\nkeys = { x = { exec = \"true\" } }\n");
        assert!(err.contains("sticky.apps") && err.contains("enter"), "{err}");
        // Действие клавиши проходит проверки записи [[binds]].
        let err = sticky_err("[sticky.apps]\nenter = \"SUPER+S\"\nkeys = { x = { action = { app = \"nope\" } } }\n");
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn chains_become_modes() {
        // Цепочки с одним первым сочетанием из разных разделов — один режим;
        // регистр клавиши звена на слияние не влияет.
        let text = base()
            + r#"
[keys]
next_workspace = "SUPER+Tab Tab"
[[binds]]
chain = "SUPER+TAB v"
exec = "true"
locked = true
[[binds]]
chain = "SUPER+W 3 f"
action = { desktop = 3, workspace = "dev-front" }
"#;
        let cfg = Config::parse(&text).unwrap();
        let nodes = sticky_nodes(&cfg).unwrap();
        let subs: Vec<&str> = nodes.iter().map(|n| n.submap.as_str()).collect();
        assert_eq!(subs, ["ws-sticky:SUPER+Tab", "ws-sticky:SUPER+A", "ws-sticky:SUPER+W", "ws-sticky:SUPER+W/3"]);
        let tab = &nodes[0];
        assert_eq!(tab.titles, ["Super+Tab"]);
        let keys: Vec<(&str, bool)> = tab.keys.iter().map(|k| (k.key.as_str(), k.exit)).collect();
        assert_eq!(keys, [("Tab", true), ("v", true)]);
        assert_eq!(tab.source, "keys.next_workspace,binds[0]");
        let w = &nodes[2];
        assert_eq!(w.keys[1].go, StickyGo::State("3".into()));
        assert!(!w.keys[1].exit && w.keys[0].exit);
        assert_eq!(nodes[3].titles, ["Super+W", "3"]);
        assert_eq!(nodes[3].parent.as_deref(), Some("ws-sticky:SUPER+W"));
        let lua = to_lua(&cfg).unwrap();
        luac(&lua);
        assert_eq!(lua.matches("hl.bind(\"SUPER + TAB\", ").count() + lua.matches("hl.bind(\"SUPER + Tab\", ").count(), 1, "{lua}");
        // Флаги записи [[binds]] остаются у звена вместе с ignore_mods.
        let block = sticky_block(&lua, "ws-sticky:SUPER+Tab");
        assert!(block.contains("hl.bind(\"v\", ws_run(\"true\"), { locked = true, ignore_mods = true })"), "{block}");
        let list = to_list(&cfg).unwrap();
        assert!(list.lines().any(|l| l.starts_with("SUPER+TAB v ") && l.contains("locked,exit")), "{list}");
        assert!(list.lines().any(|l| l.starts_with("SUPER+W 3 f ") && l.contains(" exit ")), "{list}");
    }

    #[test]
    fn chain_mode_errors() {
        // Escape и BackSpace звеньями режима быть не могут.
        for key in ["Escape", "BackSpace", "SUPER+Escape"] {
            let text = base() + &format!("\n[[binds]]\nchain = \"SUPER+X {key}\"\nexec = \"true\"\n");
            let err = Config::parse(&text).unwrap_err().to_string();
            assert!(err.contains(&format!("SUPER+X {key}")) && err.contains("binds[0]") && err.contains("Escape и BackSpace"), "{key}: {err}");
        }
        // Звено без модификаторов перехватило бы то же звено с модификаторами.
        let text = base() + "\n[[binds]]\nchain = \"SUPER+X w\"\nexec = \"true\"\n[[binds]]\nchain = \"SUPER+X SUPER+w\"\nexec = \"false\"\n";
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("SUPER+X w") && err.contains("SUPER+X SUPER+w") && err.contains("неразличимы"), "{err}");
        // Звенья с одной клавишей и разными модификаторами различимы.
        let text = base() + "\n[[binds]]\nchain = \"SUPER+X SUPER+w\"\nexec = \"true\"\n[[binds]]\nchain = \"SUPER+X CTRL+w\"\nexec = \"false\"\n";
        Config::parse(&text).unwrap();
        // Клавиша с действием не может быть и началом режима.
        let text = base() + "\n[[binds]]\nchain = \"SUPER+W f 3\"\nexec = \"true\"\n";
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("является началом") && err.contains("workspaces.dev-front") && err.contains("binds[0]"), "{err}");
        // Имя подкарты режима совпадает с цепочкой с выходом.
        let text = sticky_base() + "[sticky.f1]\nenter = \"SUPER+S\"\nkeys = { x = { exec = \"true\" } }\n[[binds]]\nchain = \"f1 x\"\nexec = \"true\"\n";
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("ws-sticky:f1") && err.contains("sticky.f1") && err.contains("binds[0]"), "{err}");
    }

    #[test]
    fn new_instance_action() {
        let text = sticky_base() + "[[binds]]\nchain = \"SUPER+SHIFT+E\"\naction = { app = \"neovide\", new = true }\n";
        let cfg = Config::parse(&text).unwrap();
        let list = to_list(&cfg).unwrap();
        assert!(list.lines().any(|l| l.starts_with("SUPER+SHIFT+E ") && l.contains("app neovide --new")), "{list}");
        for bad in ["{ workspace = \"surf\", new = true }", "{ app = \"neovide\", workspace = \"surf\", new = true }", "{ app = \"neovide\", pull = true, new = true }"] {
            let text = sticky_base() + &format!("[[binds]]\nchain = \"SUPER+SHIFT+E\"\naction = {bad}\n");
            let err = Config::parse(&text).unwrap_err().to_string();
            assert!(err.contains("SUPER+SHIFT+E") && err.contains("new"), "{bad}: {err}");
        }
        // В клавише состояния — то же действие.
        let text = sticky_base() + "[sticky.apps]\nenter = \"SUPER+S\"\nkeys = { n = { action = { app = \"neovide\", new = true }, exit = true } }\n";
        let lua = to_lua(&Config::parse(&text).unwrap()).unwrap();
        assert!(lua.contains("hl.bind(\"n\", ws_run(\"workspaced app neovide --new\"), { ignore_mods = true })"), "{lua}");
    }
}
