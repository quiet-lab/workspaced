//! Перечень клавиш для окна подсказки панели (`workspaced keys --json`).
//!
//! Каждая привязка получает человекочитаемую подпись цепочки («Shift+Super+/»),
//! описание и группу по назначению. Описание и группу записи `[[binds]]` можно
//! задать полями `desc` и `group`; без них они выводятся из действия. Серия
//! `range` показывается одной строкой с подписью «1…8». Зарезервированные
//! сочетания `[keys]` попадают в группу «Раскладка» с описаниями из
//! `keys.reserved_desc`.

use anyhow::Result;
use serde_json::{Value, json};

use crate::config::{Action, Bind, Config, TargetAction};
use crate::keys::{Act, Chain, Combo, bind_act, chain_compact, check_chains, collect, parse_chain, parse_combo, substitute};

/// Группы в порядке показа. Группа из поля `group`, которой здесь нет,
/// идёт после них в порядке первого появления.
pub const GROUPS: [&str; 9] = ["Workspace", "Столы", "Приложения", "Окна", "Уведомления", "Звук и яркость", "Снимки экрана", "Раскладка", "Сессия и прочее"];

const G_WORKSPACE: &str = GROUPS[0];
const G_DESKTOPS: &str = GROUPS[1];
const G_APPS: &str = GROUPS[2];
const G_WINDOWS: &str = GROUPS[3];
const G_NOTIFICATIONS: &str = GROUPS[4];
const G_SOUND: &str = GROUPS[5];
const G_SCREENSHOTS: &str = GROUPS[6];
const G_LAYOUT: &str = GROUPS[7];
const G_OTHER: &str = GROUPS[8];

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

/// Перечень строк подсказки в порядке конфига.
pub fn entries(cfg: &Config) -> Result<Vec<Entry>> {
    check_chains(cfg)?;
    let binds = collect(cfg)?;
    let mut out = Vec::new();
    // Клавиши `[keys]`, приложений и workspace.
    for b in binds.iter().filter(|b| b.origin.is_none()) {
        let cmd = match &b.act {
            Act::Daemon(c) => c.strip_prefix("workspaced ").unwrap_or(c).to_string(),
            _ => String::new(),
        };
        let (desc, group) = if cmd.starts_with("key ") {
            (app_key_desc(&b.source), G_APPS)
        } else if let Some(w) = cmd.strip_prefix("raise ") {
            (format!("Поднять workspace {w}"), G_WORKSPACE)
        } else {
            (daemon_desc(&cmd).unwrap_or_else(|| b.act.describe()), G_WORKSPACE)
        };
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
    Ok(json!({ "groups": groups }))
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
}
