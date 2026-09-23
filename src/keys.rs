//! Привязки клавиш: разбор цепочек, проверка конфликтов, генерация Lua для Hyprland
//! и список для просмотра.
//!
//! Цепочка — сочетания через пробел («SUPER+W 3 f»). Одиночное сочетание — цепочка
//! из одного звена, оно становится прямой привязкой. У длинной цепочки первое
//! сочетание открывает подкарту, каждое следующее ведёт глубже, последнее
//! выполняет действие и сбрасывает подкарту; Escape в каждой подкарте сбрасывает
//! без действия. Действия демона уходят подкомандой `workspaced …`, остальные
//! (`exec`, `dispatch`, `lua`) композитор выполняет сам.

use std::collections::BTreeMap;
use std::fmt::Write;

use anyhow::{Result, bail};

use crate::config::{Action, Bind, Config, Desk, Dispatch, HalfAction, MoveAction, PlaceAction, TargetAction};

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
}

impl Act {
    /// Строка для списка: «raise dots», «exec wezterm-gui», «dispatch window.close()».
    pub fn describe(&self) -> String {
        match self {
            Act::Daemon(cmd) => {
                let cmd = cmd.strip_prefix("workspaced ").unwrap_or(cmd);
                // Цепочка клавиши приложения в списке без кавычек оболочки.
                match cmd.strip_prefix("key '").and_then(|c| c.strip_suffix('\'')) {
                    Some(chain) => format!("key {}", chain.replace("'\\''", "'")),
                    None => cmd.to_string(),
                }
            }
            Act::Exec(cmd) => format!("exec {cmd}"),
            Act::Dispatch(v) => format!("dispatch {}", v.join("; ")),
            Act::Lua(body) => {
                let first = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
                format!("lua {first}{}", if body.trim().lines().count() > 1 { " …" } else { "" })
            }
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
    fn describe(&self) -> String {
        self.names().join(",")
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
fn expand_range(b: &Bind) -> Vec<Bind> {
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
            Action::Target(TargetAction { desktop, workspace, app, pull }) => {
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
    Ok(out)
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
    Ok(())
}

/// Узел дерева подкарт.
#[derive(Default)]
struct Node {
    children: BTreeMap<Combo, Node>,
    leaf: Option<(Act, Flags, Chain)>,
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
    }
}

/// Код Lua: прямые привязки, подкарты и цепочки для конфига Hyprland.
pub fn to_lua(cfg: &Config) -> Result<String> {
    let binds = collect(cfg)?;
    check_chains(cfg)?;
    let mut root = Node::default();
    for b in &binds {
        let mut node = &mut root;
        for combo in &b.chain {
            node = node.children.entry(combo.clone()).or_default();
        }
        node.leaf = Some((b.act.clone(), b.flags, b.chain.clone()));
    }
    let mut out = String::new();
    out.push_str("-- Сгенерировано командой `workspaced keys --lua`, не править руками.\n");
    out.push_str("-- Источник: ~/.config/workspaced/config.toml\n");
    out.push_str("local function ws_run(cmd)\n  return function()\n    hl.dispatch(hl.dsp.exec_cmd(cmd))\n    hl.dispatch(hl.dsp.submap(\"reset\"))\n  end\nend\n");
    let mut submaps = String::new();
    emit(&root, &[], &mut out, &mut submaps);
    out.push_str(&submaps);
    Ok(out)
}

/// Привязки узла: листья — действие, ветви — вход в подкарту. Подкарты пишутся
/// отдельно, чтобы корневые привязки шли первыми и читались подряд.
fn emit(node: &Node, path: &[Combo], binds: &mut String, submaps: &mut String) {
    for (combo, child) in &node.children {
        let mut sub_path = path.to_vec();
        sub_path.push(combo.clone());
        if let Some((act, flags, chain)) = &child.leaf {
            let in_chain = !path.is_empty();
            let action = action_lua(act, chain, in_chain);
            if flags.any() {
                writeln!(binds, "hl.bind({}, {action}, {})", lua_str(&combo.lua()), flags.lua()).unwrap();
            } else {
                writeln!(binds, "hl.bind({}, {action})", lua_str(&combo.lua())).unwrap();
            }
        } else {
            let name = format!("ws:{}", sub_path.iter().map(Combo::lua).collect::<Vec<_>>().join(" "));
            writeln!(binds, "hl.bind({}, hl.dsp.submap({}))", lua_str(&combo.lua()), lua_str(&name)).unwrap();
            let mut inner = String::new();
            emit(child, &sub_path, &mut inner, submaps);
            writeln!(submaps, "hl.define_submap({}, function()", lua_str(&name)).unwrap();
            for line in inner.lines() {
                writeln!(submaps, "  {line}").unwrap();
            }
            submaps.push_str("  hl.bind(\"Escape\", hl.dsp.submap(\"reset\"))\nend)\n");
        }
    }
}

/// Таблица привязок для просмотра: цепочка, действие, флаги, источник.
pub fn to_list(cfg: &Config) -> Result<String> {
    let binds = collect(cfg)?;
    check_chains(cfg)?;
    let rows: Vec<(String, String, String, String)> = binds
        .iter()
        .map(|b| (chain_compact(&b.chain), b.act.describe(), b.flags.describe(), b.source.split(' ').next().unwrap_or("").to_string()))
        .collect();
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
        // Общий первый сегмент у двух цепочек допустим: обе живут в одной
        // подкарте и ни одна не является началом другой.
        let text = base() + "\n[keys]\nsave_session = \"CTRL+SUPER+s CTRL+SUPER+s\"\nsave_workspace = \"CTRL+SUPER+s CTRL+SUPER+w\"\n";
        let cfg = Config::parse(&text).unwrap();
        let lua = to_lua(&cfg).unwrap();
        luac(&lua);
        // Модификаторы приводятся к каноническому порядку: SUPER, CTRL, ALT, SHIFT.
        assert!(lua.contains("hl.bind(\"SUPER + CTRL + s\", hl.dsp.submap(\"ws:SUPER + CTRL + s\"))"), "{lua}");
        assert!(lua.contains("ws_run(\"workspaced save-session\")"), "{lua}");
        assert!(lua.contains("ws_run(\"workspaced save-workspace\")"), "{lua}");
        let list = to_list(&cfg).unwrap();
        assert!(list.contains("SUPER+CTRL+s SUPER+CTRL+s") && list.contains("save-session") && list.contains("keys.save_session"), "{list}");
        assert!(list.contains("SUPER+CTRL+s SUPER+CTRL+w") && list.contains("save-workspace"), "{list}");
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
        assert!(a.contains("hl.define_submap(\"ws:SUPER + W 3\", function()"), "{a}");
        assert!(a.contains("ws_run(\"workspaced raise dev-front --desktop 3\")"), "{a}");
        assert!(a.contains("ws_run(\"workspaced key 'SUPER+A b'\")"), "{a}");
        assert!(a.contains("hl.bind(\"SUPER + A\", hl.dsp.submap(\"ws:SUPER + A\"))"), "{a}");
        // Диспетчер в конце цепочки сбрасывает подкарту.
        assert!(a.contains("hl.bind(\"x\", function() hl.dispatch(hl.dsp.exit()); hl.dispatch(hl.dsp.submap(\"reset\")) end)"), "{a}");
        assert!(!a.contains("ws:SUPER +\""), "{a}");
        let list = to_list(&cfg).unwrap();
        assert!(list.contains("SUPER+Return") && list.contains("exec wezterm-gui") && list.contains("binds[1]"), "{list}");
        assert!(list.contains("SUPER+W f") && list.contains("raise dev-front") && list.contains("workspaces.dev-front"), "{list}");
        assert!(list.contains("locked,repeating"), "{list}");
        assert!(list.contains("SUPER+X") && list.contains("maximize"), "{list}");
        assert!(list.contains("Home") && list.contains("place top-left"), "{list}");
    }
}
