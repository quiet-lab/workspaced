//! Перечень клавиш для окна подсказки панели (`workspaced keys --json`).
//!
//! Каждая привязка получает человекочитаемую подпись цепочки («Shift+Super+/»),
//! описание и группу по назначению. Описание и группу записи `[[binds]]` можно
//! задать полями `desc` и `group`; без них они выводятся из действия. Серия
//! `range` показывается одной строкой с подписью «1…8». Зарезервированные
//! сочетания `[keys]` попадают в группу «Раскладка» с описаниями из
//! `keys.reserved_desc`. Цепочки с выходом дают строки путей в группе
//! «Цепочки с выходом» и отдельный раздел `sticky` — плоский список состояний
//! для индикатора панели (изменение sticky-chains, решение D10). В разделе
//! `sticky` есть и автоматические режимы многозвенных цепочек (изменение
//! chains-sticky): подпись режима — подпись префикса, описания клавиш — как
//! у строк подсказки.

use anyhow::Result;
use serde_json::{Value, json};

use crate::config::{Action, Bind, Config, TargetAction};
use crate::keys::{Act, Binding, Chain, Combo, StickyBind, StickyGo, StickyNode, bind_act, chain_compact, chain_key, check_chains, collect, expand_range, parse_chain, parse_combo, sticky_nodes, sticky_rows, substitute};

/// Группы в порядке показа. Группа из поля `group`, которой здесь нет,
/// идёт после них в порядке первого появления.
pub const GROUPS: [&str; 10] = ["Workspace", "Столы", "Приложения", "Цепочки с выходом", "Окна", "Уведомления", "Звук и яркость", "Снимки экрана", "Раскладка", "Сессия и прочее"];

const G_WORKSPACE: &str = GROUPS[0];
const G_DESKTOPS: &str = GROUPS[1];
const G_APPS: &str = GROUPS[2];
const G_STICKY: &str = GROUPS[3];
const G_WINDOWS: &str = GROUPS[4];
const G_NOTIFICATIONS: &str = GROUPS[5];
const G_SOUND: &str = GROUPS[6];
const G_SCREENSHOTS: &str = GROUPS[7];
const G_LAYOUT: &str = GROUPS[8];
const G_OTHER: &str = GROUPS[9];

/// Строка подсказки.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Цепочка в каноническом виде, как в `keys --list`.
    pub chain: String,
    /// Подпись для человека: «Ctrl+Super+S Ctrl+Super+W».
    pub label: String,
    pub desc: String,
    pub group: String,
    /// Действие, как в `keys --list`.
    pub action: String,
    /// Откуда привязка: `apps.chrome`, `binds[3]`, `keys.reserved`.
    pub source: String,
}

/// Подпись клавиши: стрелки и знаки — символами, служебные имена keysym —
/// привычными названиями.
fn key_label(key: &str, with_mods: bool) -> String {
    // Hyprland сравнивает имена клавиш без учёта регистра («TAB» и «Tab»).
    let named = match key.to_ascii_lowercase().as_str() {
        "slash" => "/",
        "question" => "?",
        "period" => ".",
        "comma" => ",",
        "minus" => "-",
        "equal" => "=",
        "space" => "Пробел",
        "return" => "Enter",
        "backspace" => "Backspace",
        "page_up" => "PageUp",
        "page_down" => "PageDown",
        "left" => "←",
        "right" => "→",
        "up" => "↑",
        "down" => "↓",
        "mouse:272" => "ЛКМ",
        "mouse:273" => "ПКМ",
        "mouse:274" => "СКМ",
        "tab" => "Tab",
        "escape" => "Escape",
        "delete" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "print" => "Print",
        "menu" => "Menu",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_string();
    }
    if let Some(rest) = key.strip_prefix("XF86") {
        return rest.to_string();
    }
    // Буква с модификаторами пишется заглавной («Super+T»), звено цепочки
    // без модификаторов — как в конфиге («Super+Tab w»).
    if key.chars().count() == 1 && with_mods {
        return key.to_uppercase();
    }
    key.to_string()
}

/// Подпись сочетания: модификаторы в порядке Ctrl, Alt, Shift, Super, как их
/// пишет документация сессии.
pub fn combo_label(c: &Combo) -> String {
    let rank = |m: &str| match m {
        "CTRL" | "CONTROL" => 0,
        "ALT" => 1,
        "SHIFT" => 2,
        "SUPER" => 3,
        _ => 4,
    };
    let mut mods: Vec<&String> = c.mods.iter().collect();
    mods.sort_by_key(|m| rank(m));
    let mut parts: Vec<String> = mods
        .iter()
        .map(|m| match m.as_str() {
            "CTRL" | "CONTROL" => "Ctrl".to_string(),
            "ALT" => "Alt".to_string(),
            "SHIFT" => "Shift".to_string(),
            "SUPER" => "Super".to_string(),
            other => other.to_string(),
        })
        .collect();
    parts.push(key_label(&c.key, !c.mods.is_empty()));
    parts.join("+")
}

pub fn chain_label(chain: &Chain) -> String {
    chain.iter().map(combo_label).collect::<Vec<_>>().join(" ")
}

/// Описание по команде демона (без префикса `workspaced `).
fn daemon_desc(cmd: &str) -> Option<String> {
    let mut words = cmd.split_whitespace();
    let head = words.next()?;
    let arg = words.next().unwrap_or("");
    let s = match (head, arg) {
        ("save-session", _) => "Записать снимок сессии".to_string(),
        ("save-workspace", _) => "Записать workspace в конфиг".to_string(),
        ("sessions", _) => "Окно выбора сессии".to_string(),
        ("next", _) => "Следующий workspace стола".to_string(),
        ("maximize", _) => "Развернуть окно; повторно — вернуть прежнюю геометрию".to_string(),
        ("arrange", _) => "Расставить окна стола по активному workspace".to_string(),
        ("detach", _) => "Отделить окно от workspace и закрыть".to_string(),
        ("move-desktop", n) => format!("Перенести workspace на стол {n} и перейти туда"),
        ("half", side) => match side {
            "left" => "Окно в левую половину",
            "right" => "Окно в правую половину",
            "up" => "Окно в верхнюю половину",
            "down" => "Окно в нижнюю половину",
            _ => return None,
        }
        .to_string(),
        ("place", pos) => match pos {
            "top-left" => "Окно в верхний левый угол",
            "top-center" => "Окно вверх по центру",
            "top-right" => "Окно в верхний правый угол",
            "bottom-left" => "Окно в нижний левый угол",
            "bottom-center" => "Окно вниз по центру",
            "bottom-right" => "Окно в нижний правый угол",
            "center" => "Окно по центру во всю высоту",
            "full" => "Окно на всю рабочую область",
            _ => return None,
        }
        .to_string(),
        _ => return None,
    };
    Some(s)
}

/// Описание действия `{ app = …, workspace = …, desktop = … }`.
fn target_desc(t: &TargetAction) -> String {
    let mut s = match (&t.workspace, &t.app) {
        (Some(w), Some(a)) => format!("Приложение {a} в workspace {w}"),
        (None, Some(a)) => format!("Приложение {a}"),
        (Some(w), None) => format!("Поднять workspace {w}"),
        (None, None) => String::new(),
    };
    if let Some(d) = &t.desktop {
        s.push_str(&format!(" на столе {d}"));
    }
    if t.pull == Some(true) {
        s.push_str(" с переносом в активный workspace");
    }
    s
}

/// Описание клавиши приложения по её источникам: `apps.herdr` →
/// «Приложение herdr», `workspaces.surf.apps.chrome` → «chrome в workspace surf».
fn app_key_desc(source: &str) -> String {
    source
        .split(',')
        .map(|s| {
            let parts: Vec<&str> = s.split('.').collect();
            match parts.as_slice() {
                ["apps", a] => format!("Приложение {a}"),
                ["workspaces", w, "apps", a] => format!("{a} в workspace {w}"),
                _ => s.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Группа записи `[[binds]]` по её действию.
fn default_group(b: &Bind, act: &Act) -> &'static str {
    match act {
        Act::Daemon(cmd) => {
            let cmd = cmd.strip_prefix("workspaced ").unwrap_or(cmd);
            let head = cmd.split_whitespace().next().unwrap_or("");
            match head {
                "half" | "place" | "maximize" | "detach" => G_WINDOWS,
                "move-desktop" => G_DESKTOPS,
                "app" => G_APPS,
                _ => G_WORKSPACE,
            }
        }
        Act::Dispatch(exprs) => {
            let text = exprs.join("; ");
            if text.contains("exit()") {
                G_OTHER
            } else if text.contains("workspace = ") && !text.contains("special:") {
                // Переход на стол и перенос окна на стол.
                G_DESKTOPS
            } else {
                G_WINDOWS
            }
        }
        Act::Exec(cmd) => {
            if cmd.contains("ipc call notifications") {
                G_NOTIFICATIONS
            } else if cmd.contains("screenshot") {
                G_SCREENSHOTS
            } else if cmd.contains("change-volume")
                || cmd.contains("change-brightness")
                || cmd.contains("playerctl")
                || b.chain.contains("XF86Audio")
                || b.chain.contains("XF86MonBrightness")
            {
                G_SOUND
            } else {
                G_OTHER
            }
        }
        Act::Lua(_) => G_OTHER,
        Act::Sticky(_) => G_STICKY,
    }
}

/// Описание записи `[[binds]]` без поля `desc`.
fn default_desc(b: &Bind, act: &Act) -> String {
    if let Some(Action::Target(t)) = &b.action {
        return target_desc(t);
    }
    if let Act::Daemon(cmd) = act
        && let Some(d) = daemon_desc(cmd.strip_prefix("workspaced ").unwrap_or(cmd))
    {
        return d;
    }
    act.describe()
}

/// Описание и группа привязки из `[keys]`, приложений и workspace.
fn own_desc(b: &Binding) -> (String, &'static str) {
    let cmd = match &b.act {
        Act::Daemon(c) => c.strip_prefix("workspaced ").unwrap_or(c).to_string(),
        _ => String::new(),
    };
    if cmd.starts_with("key ") {
        (app_key_desc(&b.source), G_APPS)
    } else if let Some(w) = cmd.strip_prefix("raise ") {
        (format!("Поднять workspace {w}"), G_WORKSPACE)
    } else {
        (daemon_desc(&cmd).unwrap_or_else(|| b.act.describe()), G_WORKSPACE)
    }
}

/// Описание привязки сессии, как в подсказке: у записи `[[binds]]` — её
/// поле `desc` (с подстановкой номера серии) либо описание действия.
fn binding_desc(cfg: &Config, b: &Binding) -> String {
    let Some(i) = b.origin else {
        return own_desc(b).0;
    };
    let key = chain_key(&b.chain);
    let raw = &cfg.binds[i];
    match expand_range(raw).into_iter().find(|e| parse_chain(&e.chain).is_ok_and(|c| chain_key(&c) == key)) {
        Some(e) => e.desc.clone().unwrap_or_else(|| default_desc(&e, &b.act)),
        None => b.act.describe(),
    }
}

/// Подпись клавиши состояния с её модификаторами.
fn sticky_key_label(k: &StickyBind) -> String {
    combo_label(&k.combo())
}

/// Описание клавиши состояния без подписи состояния: поле `desc`, для
/// перехода — подпись дочернего состояния (у автоматического режима —
/// клавиши следующего звена), для унаследованной клавиши — описание клавиши
/// приложения, для звена многозвенной цепочки — описание её привязки,
/// иначе описание действия.
fn sticky_key_desc(cfg: &Config, nodes: &[StickyNode], node: &StickyNode, k: &StickyBind) -> String {
    if let Some(d) = &k.desc {
        return d.clone();
    }
    match &k.go {
        StickyGo::State(s) => {
            let sub = format!("{}/{s}", node.submap);
            let child = nodes.iter().find(|n| n.submap == sub);
            match child {
                Some(n) if n.auto => format!("Далее: {}", n.keys.iter().map(sticky_key_label).collect::<Vec<_>>().join(", ")),
                Some(n) => n.titles.last().cloned().unwrap_or_else(|| s.clone()),
                None => s.clone(),
            }
        }
        StickyGo::Act(act) => match (&k.inherited, &k.explicit, &k.binding) {
            (Some(src), _, _) => app_key_desc(src),
            (None, Some(b), _) => default_desc(b, act),
            (None, None, Some(b)) => binding_desc(cfg, b),
            (None, None, None) => act.describe(),
        },
    }
}

/// Перечень строк подсказки в порядке конфига.
pub fn entries(cfg: &Config) -> Result<Vec<Entry>> {
    check_chains(cfg)?;
    let binds = collect(cfg)?;
    let nodes = sticky_nodes(cfg)?;
    let mut out = Vec::new();
    // Клавиши `[keys]`, приложений и workspace.
    for b in binds.iter().filter(|b| b.origin.is_none()) {
        // Цепочка с выходом: сочетание входа с подписью корня, затем пути
        // до состояний и клавиш (решение D10).
        if let Act::Sticky(name) = &b.act {
            let root = nodes.iter().find(|n| n.chain == *name && n.names.is_empty());
            out.push(Entry {
                chain: chain_compact(&b.chain),
                label: chain_label(&b.chain),
                desc: root.and_then(|n| n.titles.first().cloned()).unwrap_or_else(|| name.clone()),
                group: G_STICKY.to_string(),
                action: b.act.describe(),
                source: b.source.clone(),
            });
            for r in sticky_rows(&nodes, name) {
                let node = &nodes[r.node];
                let k = &node.keys[r.key];
                let desc = match &k.go {
                    StickyGo::State(_) => sticky_key_desc(cfg, &nodes, node, k),
                    StickyGo::Act(_) => {
                        let title = node.titles.last().cloned().unwrap_or_default();
                        format!("{title} · {}{}", sticky_key_desc(cfg, &nodes, node, k), if k.exit { " (выход)" } else { "" })
                    }
                };
                out.push(Entry { chain: chain_compact(&r.chain), label: chain_label(&r.chain), desc, group: G_STICKY.to_string(), action: r.action, source: r.source });
            }
            continue;
        }
        let (desc, group) = own_desc(b);
        out.push(Entry {
            chain: chain_compact(&b.chain),
            label: chain_label(&b.chain),
            desc,
            group: group.to_string(),
            action: b.act.describe(),
            source: b.source.clone(),
        });
    }
    // Записи `[[binds]]`: серия `range` — одной строкой с подписью «от…до».
    for (i, raw) in cfg.binds.iter().enumerate() {
        let b = match raw.range {
            Some((from, to)) => substitute(raw, &format!("{from}…{to}")),
            None => raw.clone(),
        };
        let act = bind_act(&b, false)?;
        let chain = parse_chain(&b.chain)?;
        out.push(Entry {
            chain: chain_compact(&chain),
            label: chain_label(&chain),
            desc: b.desc.clone().unwrap_or_else(|| default_desc(&b, &act)),
            group: b.group.clone().unwrap_or_else(|| default_group(&b, &act).to_string()),
            action: act.describe(),
            source: format!("binds[{i}]"),
        });
    }
    // Зарезервированные сочетания — карта XKB и намеренно свободные.
    for r in &cfg.keys.reserved {
        let combo = parse_combo(r)?;
        let desc = cfg
            .keys
            .reserved_desc
            .iter()
            .find(|(k, _)| parse_combo(k).is_ok_and(|c| c.cmp_key() == combo.cmp_key()))
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| "Зарезервировано".to_string());
        out.push(Entry {
            chain: combo.compact(),
            label: combo_label(&combo),
            desc,
            group: G_LAYOUT.to_string(),
            action: "xkb".to_string(),
            source: "keys.reserved".to_string(),
        });
    }
    Ok(out)
}

/// Строки, собранные по группам: сначала группы из `GROUPS` по порядку,
/// затем собственные группы конфига в порядке первого появления; пустые
/// группы не выводятся.
pub fn grouped(cfg: &Config) -> Result<Vec<(String, Vec<Entry>)>> {
    let list = entries(cfg)?;
    let mut names: Vec<String> = GROUPS.iter().map(|g| g.to_string()).collect();
    for e in &list {
        if !names.contains(&e.group) {
            names.push(e.group.clone());
        }
    }
    Ok(names
        .into_iter()
        .map(|g| {
            let rows: Vec<Entry> = list.iter().filter(|e| e.group == g).cloned().collect();
            (g, rows)
        })
        .filter(|(_, rows)| !rows.is_empty())
        .collect())
}

/// JSON для панели: `{ "groups": [ { "name", "keys": [ { chain, label, desc,
/// action, source } ] } ] }`.
pub fn to_json(cfg: &Config) -> Result<Value> {
    let groups: Vec<Value> = grouped(cfg)?
        .into_iter()
        .map(|(name, rows)| {
            let keys: Vec<Value> = rows
                .into_iter()
                .map(|e| json!({ "chain": e.chain, "label": e.label, "desc": e.desc, "action": e.action, "source": e.source }))
                .collect();
            json!({ "name": name, "keys": keys })
        })
        .collect();
    // Раздел для индикатора цепочки с выходом: состояние находится по имени
    // подкарты из события композитора без разбора строк (решение D10).
    // В нём и автоматические режимы многозвенных цепочек: путь — подписи
    // звеньев («Super+Tab»), клавиши — последние звенья с описаниями, как
    // в подсказке (изменение chains-sticky).
    let nodes = sticky_nodes(cfg)?;
    let sticky: Vec<Value> = nodes
        .iter()
        .map(|n| {
            let keys: Vec<Value> = n
                .keys
                .iter()
                .map(|k| json!({ "key": k.combo().compact(), "label": sticky_key_label(k), "desc": sticky_key_desc(cfg, &nodes, n, k), "exit": k.exit }))
                .collect();
            json!({ "submap": n.submap, "path": n.titles, "keys": keys })
        })
        .collect();
    Ok(json!({ "groups": groups, "sticky": sticky }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::parse(
            r#"
[keys]
save_session = "CTRL+SUPER+s CTRL+SUPER+s"
next_workspace = "SUPER+TAB Tab"
reserved = ["SUPER+space", "ALT+SUPER+space"]
reserved_desc = { "SUPER+space" = "Следующая раскладка" }
[templates.t]
main = "c"
cells = { c = { x = 0, y = 0, w = 10, h = 10 }, l = { x = 0, y = 0, w = 5, h = 10 } }
[apps.chrome]
cmd = "chrome"
chain = "SUPER+SHIFT+B"
[apps.herdr]
cmd = "wezterm"
chain = "SUPER+T"
[workspaces.surf]
template = "t"
chain = "SUPER+TAB s"
[workspaces.surf.apps]
chrome = { cell = "l", chain = "SUPER+B" }
herdr = "c"
[[binds]]
chain = "SUPER+SHIFT+slash"
exec = "qs -c panel ipc call keys toggle"
desc = "Подсказка клавиш"
[[binds]]
chain = "ALT+SUPER+left"
action = { half = "left" }
[[binds]]
chain = "CTRL+SUPER+$n"
range = [1, 8]
action = { move = "$n" }
[[binds]]
chain = "SUPER+$n"
range = [1, 8]
dispatch = "focus({ workspace = $n })"
[[binds]]
chain = "CTRL+Escape"
exec = "qs -c panel ipc call notifications history"
[[binds]]
chain = "XF86AudioMute"
exec = "~/bin/change-volume.sh 0"
[[binds]]
chain = "SHIFT+Print"
exec = "~/bin/screenshot-selection"
[[binds]]
chain = "ALT+Tab"
lua = "return"
group = "Окна"
desc = "Следующее окно"
[[binds]]
chain = "SUPER+Z"
dispatch = "window.move({ workspace = \"special:hidden\", follow = false })"
[[binds]]
chain = "SUPER+R"
exec = "rofi -show drun"
group = "Мои программы"
"#,
        )
        .unwrap()
    }

    fn find<'a>(list: &'a [Entry], chain: &str) -> &'a Entry {
        list.iter().find(|e| e.chain == chain).unwrap_or_else(|| panic!("нет строки {chain}: {list:#?}"))
    }

    #[test]
    fn labels_are_human() {
        let l = |s: &str| chain_label(&parse_chain(s).unwrap());
        assert_eq!(l("SUPER+SHIFT+slash"), "Shift+Super+/");
        assert_eq!(l("CTRL+SUPER+s CTRL+SUPER+w"), "Ctrl+Super+S Ctrl+Super+W");
        assert_eq!(l("SUPER+TAB w"), "Super+Tab w");
        assert_eq!(l("ALT+SUPER+Page_Up"), "Alt+Super+PageUp");
        assert_eq!(l("SUPER+CTRL+ALT+Return"), "Ctrl+Alt+Super+Enter");
        assert_eq!(l("XF86AudioMute"), "AudioMute");
        assert_eq!(l("SUPER+mouse:272"), "Super+ЛКМ");
    }

    #[test]
    fn groups_and_descriptions() {
        let list = entries(&cfg()).unwrap();
        let e = find(&list, "SUPER+SHIFT+slash");
        assert_eq!((e.label.as_str(), e.desc.as_str(), e.group.as_str()), ("Shift+Super+/", "Подсказка клавиш", "Сессия и прочее"));
        assert_eq!(find(&list, "SUPER+CTRL+s SUPER+CTRL+s").group, "Workspace");
        assert_eq!(find(&list, "SUPER+TAB s").desc, "Поднять workspace surf");
        let b = find(&list, "SUPER+B");
        assert_eq!((b.desc.as_str(), b.group.as_str()), ("chrome в workspace surf", "Приложения"));
        assert_eq!(find(&list, "SUPER+T").desc, "Приложение herdr");
        let h = find(&list, "SUPER+ALT+left");
        assert_eq!((h.desc.as_str(), h.group.as_str()), ("Окно в левую половину", "Окна"));
        // Серия range — одна строка с подписью «1…8».
        let m = find(&list, "SUPER+CTRL+1…8");
        assert_eq!((m.label.as_str(), m.desc.as_str(), m.group.as_str()), ("Ctrl+Super+1…8", "Перенести workspace на стол 1…8 и перейти туда", "Столы"));
        assert_eq!(find(&list, "SUPER+1…8").group, "Столы");
        assert_eq!(list.iter().filter(|e| e.source == "binds[3]").count(), 1);
        assert_eq!(find(&list, "CTRL+Escape").group, "Уведомления");
        assert_eq!(find(&list, "XF86AudioMute").group, "Звук и яркость");
        assert_eq!(find(&list, "SHIFT+Print").group, "Снимки экрана");
        assert_eq!(find(&list, "ALT+Tab").group, "Окна");
        assert_eq!(find(&list, "SUPER+Z").group, "Окна");
        // Без desc описание — действие как в keys --list.
        assert_eq!(find(&list, "SUPER+R").desc, "exec rofi -show drun");
        let r = find(&list, "SUPER+space");
        assert_eq!((r.desc.as_str(), r.group.as_str(), r.label.as_str()), ("Следующая раскладка", "Раскладка", "Super+Пробел"));
        assert_eq!(find(&list, "SUPER+ALT+space").desc, "Зарезервировано");
    }

    #[test]
    fn group_order() {
        let names: Vec<String> = grouped(&cfg()).unwrap().into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            ["Workspace", "Столы", "Приложения", "Окна", "Уведомления", "Звук и яркость", "Снимки экрана", "Раскладка", "Сессия и прочее", "Мои программы"]
        );
        let v = to_json(&cfg()).unwrap();
        assert_eq!(v["groups"][0]["name"], "Workspace");
        assert!(v["groups"][0]["keys"][0]["label"].is_string());
    }

    #[test]
    fn invalid_config_is_error() {
        let mut c = cfg();
        c.keys.reserved.push("SUPER+R".into());
        assert!(entries(&c).is_err());
    }

    #[test]
    fn chain_modes_in_sticky_section() {
        let mut c = cfg();
        c.keys.save_workspace = Some("CTRL+SUPER+s CTRL+SUPER+w".into());
        let text = r#"
[templates.t]
main = "c"
cells = { c = { x = 0, y = 0, w = 10, h = 10 } }
[workspaces.work]
template = "t"
chain = "SUPER+TAB w"
[[binds]]
chain = "SUPER+W 3 f"
exec = "true"
desc = "Проверка"
"#;
        let extra = Config::parse(text).unwrap();
        let v = to_json(&extra).unwrap();
        let sticky = v["sticky"].as_array().unwrap();
        let w = sticky.iter().find(|s| s["submap"] == "ws-sticky:SUPER+W").unwrap();
        assert_eq!(w["path"], json!(["Super+W"]));
        assert_eq!(w["keys"][0], json!({ "key": "3", "label": "3", "desc": "Далее: f", "exit": false }));
        let w3 = sticky.iter().find(|s| s["submap"] == "ws-sticky:SUPER+W/3").unwrap();
        assert_eq!(w3["path"], json!(["Super+W", "3"]));
        assert_eq!(w3["keys"][0], json!({ "key": "f", "label": "f", "desc": "Проверка", "exit": true }));
        let tab = sticky.iter().find(|s| s["submap"] == "ws-sticky:SUPER+TAB").unwrap();
        assert_eq!(tab["keys"][0], json!({ "key": "w", "label": "w", "desc": "Поднять workspace work", "exit": true }));

        // Сохранение: звенья с модификаторами — подписи с модификаторами.
        let v = to_json(&c).unwrap();
        let sticky = v["sticky"].as_array().unwrap();
        let save = sticky.iter().find(|s| s["submap"] == "ws-sticky:SUPER+CTRL+s").unwrap();
        assert_eq!(save["path"], json!(["Ctrl+Super+S"]));
        assert_eq!(save["keys"][0], json!({ "key": "SUPER+CTRL+s", "label": "Ctrl+Super+S", "desc": "Записать снимок сессии", "exit": true }));
        assert_eq!(save["keys"][1], json!({ "key": "SUPER+CTRL+w", "label": "Ctrl+Super+W", "desc": "Записать workspace в конфиг", "exit": true }));
        let tab = sticky.iter().find(|s| s["submap"] == "ws-sticky:SUPER+TAB").unwrap();
        let keys: Vec<(String, String)> = tab["keys"].as_array().unwrap().iter().map(|k| (k["label"].as_str().unwrap().to_string(), k["desc"].as_str().unwrap().to_string())).collect();
        assert_eq!(keys, [("Tab".to_string(), "Следующий workspace стола".to_string()), ("s".to_string(), "Поднять workspace surf".to_string())]);
        // Строки групп подсказки остаются прежними.
        assert_eq!(find(&entries(&c).unwrap(), "SUPER+CTRL+s SUPER+CTRL+w").group, "Workspace");
    }

    #[test]
    fn sticky_entries_and_section() {
        let text = r#"
[keys]
sessions = "SHIFT+SUPER+S"
[templates.t]
main = "c"
cells = { c = { x = 0, y = 0, w = 10, h = 10 }, r = { x = 5, y = 0, w = 5, h = 10 } }
[apps.neovide]
cmd = "neovide"
chain = "SUPER+E"
[apps.chrome-ai]
cmd = "chrome"
chain = "SUPER+SHIFT+V"
[workspaces.surf]
template = "t"
[workspaces.surf.apps]
chrome-ai = { cell = "r", chain = "SUPER+V" }
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
"#;
        let cfg = Config::parse(text).unwrap();
        let list = entries(&cfg).unwrap();
        let row = |chain: &str| {
            let e = find(&list, chain);
            (e.label.clone(), e.desc.clone(), e.group.clone())
        };
        let g = "Цепочки с выходом".to_string();
        assert_eq!(row("SUPER+S"), ("Super+S".into(), "Приложения".into(), g.clone()));
        assert_eq!(row("SUPER+S r"), ("Super+S r".into(), "Поднять".into(), g.clone()));
        assert_eq!(row("SUPER+S r v"), ("Super+S r v".into(), "Поднять · chrome-ai в workspace surf".into(), g.clone()));
        assert_eq!(row("SUPER+S s e"), ("Super+S s e".into(), "Новое окно · Приложение neovide (выход)".into(), g.clone()));
        assert_eq!(find(&list, "SUPER+S s v").action, "key --new SUPER+V");
        assert_eq!(find(&list, "SUPER+S r v").source, "sticky.apps.raise");
        let names: Vec<String> = grouped(&cfg).unwrap().into_iter().map(|(n, _)| n).collect();
        let at = |n: &str| names.iter().position(|x| x == n).unwrap();
        assert_eq!(at("Цепочки с выходом"), at("Приложения") + 1, "{names:?}");
        let v = to_json(&cfg).unwrap();
        let sticky = v["sticky"].as_array().unwrap();
        assert_eq!(sticky.len(), 3);
        let raise = sticky.iter().find(|s| s["submap"] == "ws-sticky:apps/raise").unwrap();
        assert_eq!(raise["path"], json!(["Приложения", "Поднять"]));
        assert_eq!(raise["keys"][0], json!({ "key": "e", "label": "e", "desc": "Приложение neovide", "exit": true }));
        let root = sticky.iter().find(|s| s["submap"] == "ws-sticky:apps").unwrap();
        assert_eq!(root["keys"][0], json!({ "key": "r", "label": "r", "desc": "Поднять", "exit": false }));
    }
}
