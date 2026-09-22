//! Две команды сохранения. «Сохранить сессию» (`save-session`) принимает
//! посторонние окна текущего стола в активный workspace как дополнительные
//! приложения сессии и файла конфига не трогает. «Сохранить workspace»
//! (`save-workspace`) записывает состояние активного workspace в `config.toml`
//! через toml_edit с сохранением комментариев.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value, value};

use crate::config::{Config, Mode, PxRect};
use crate::daemon::{self, Daemon};
use crate::hypr::{self, Client};
use crate::state::{ExtraApp, Foreign, Place};

fn rect_table(r: PxRect) -> InlineTable {
    let mut t = InlineTable::new();
    t.insert("x", (r.x as i64).into());
    t.insert("y", (r.y as i64).into());
    t.insert("w", (r.w as i64).into());
    t.insert("h", (r.h as i64).into());
    t
}

fn rect_item(r: PxRect) -> Value {
    let mut t = InlineTable::new();
    t.insert("rect", Value::InlineTable(rect_table(r)));
    Value::InlineTable(t)
}

/// Запись приложения в таблице `apps` workspace: имя ячейки, если окно стоит
/// в ней, иначе прямоугольник. Место берётся у первого экземпляра, остальные
/// окна приложения отдельными записями не появляются.
fn app_item(place: &Place, live: Option<PxRect>, expected: Option<PxRect>) -> Item {
    match (place, live, expected) {
        (Place::Cell(c), Some(l), Some(e)) if l == e => value(c.as_str()),
        (Place::Cell(c), None, _) => value(c.as_str()),
        (Place::Cell(_), Some(l), _) => Item::Value(rect_item(l)),
        (Place::Rect { rect }, live, _) => Item::Value(rect_item(live.unwrap_or(*rect))),
    }
}

/// Имя для постороннего окна по классу, уникальное среди приложений.
fn app_name(class: &str, taken: &[String]) -> String {
    let base: String = class.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect::<String>().trim_matches('-').to_string();
    let base = if base.is_empty() { "app".to_string() } else { base };
    if !taken.contains(&base) {
        return base;
    }
    (2..).map(|i| format!("{base}-{i}")).find(|n| !taken.contains(n)).unwrap()
}

/// Описание приложения для раздела `[apps]`: команда, аргументы, каталог
/// и класс окна как точное совпадение.
fn app_table(cmd: &[String], cwd: Option<&str>, class: Option<&str>) -> Table {
    let mut t = Table::new();
    t["cmd"] = value(cmd[0].as_str());
    if cmd.len() > 1 {
        let mut arr = toml_edit::Array::new();
        for a in &cmd[1..] {
            arr.push(a.as_str());
        }
        t["args"] = value(arr);
    }
    if let Some(cwd) = cwd {
        t["cwd"] = value(cwd);
    }
    if let Some(class) = class {
        t["class"] = value(format!("^{}$", regex::escape(class)));
    }
    t
}

/// Наименьший свободный номер экземпляра с учётом номеров, розданных
/// в этом же вызове.
fn next_instance(used: &mut Vec<(String, u32)>, app: &str) -> u32 {
    let nums: Vec<u32> = used.iter().filter(|(a, _)| a == app).map(|(_, n)| *n).collect();
    let num = daemon::free_number(&nums);
    used.push((app.to_string(), num));
    num
}

// ---- Сохранение сессии ------------------------------------------------------

/// Что сделать с посторонним окном при сохранении сессии.
#[derive(Debug, Clone, PartialEq)]
pub struct Adopted {
    pub addr: String,
    pub app: String,
    /// Запись для сессии; `None` — приложение уже входит в workspace,
    /// и окно становится просто ещё одним его экземпляром.
    pub extra: Option<ExtraApp>,
}

/// План принятия посторонних окон в workspace: окно, подходящее приложению
/// конфига, получает его имя (и запись только о месте, если в workspace этого
/// приложения ещё нет); прочие окна становятся новыми дополнительными
/// приложениями с именем по классу. Окно без командной строки пропускается:
/// восстановить его было бы нечем.
fn session_plan(cfg: &Config, ws_apps: &[String], windows: &[(&Client, Option<&Foreign>)]) -> Vec<Adopted> {
    let mut taken: Vec<String> = cfg.apps.keys().cloned().collect();
    let mut out = Vec::new();
    for (c, f) in windows {
        if let Some(app) = daemon::app_for_window(cfg, c) {
            let inside = ws_apps.iter().any(|x| cfg.app_is(&app, x));
            let extra = (!inside).then(|| ExtraApp { rect: c.rect(), ..ExtraApp::default() });
            out.push(Adopted { addr: c.address.clone(), app, extra });
            continue;
        }
        let Some(f) = f.filter(|f| !f.cmd.is_empty()) else {
            log::info!("сохранение сессии: у окна {} ({}) нет командной строки, окно осталось свободным", c.address, c.class);
            continue;
        };
        let app = app_name(&c.class, &taken);
        taken.push(app.clone());
        let extra = ExtraApp { class: Some(c.class.clone()), cmd: f.cmd.clone(), cwd: f.cwd.clone(), rect: c.rect() };
        out.push(Adopted { addr: c.address.clone(), app, extra: Some(extra) });
    }
    out
}

/// Принять посторонние окна текущего стола в активный workspace этого стола.
/// Файл конфига не читается и не пишется; места приложений, уже входящих
/// в workspace, не меняются. Возвращает имя workspace и число принятых окон.
pub fn save_session(d: &mut Daemon) -> Result<(String, usize)> {
    let n = d.current_desktop();
    let Some(ws) = d.state().desktops.get(&n).and_then(|x| x.active.clone()) else { bail!("на столе {n} нет активного workspace") };
    let clients = d.clients()?;
    let cfg = d.cfg().clone();
    let ws_apps: Vec<String> = cfg.workspaces.get(&ws).map(|w| w.apps.keys().cloned().collect()).unwrap_or_default();
    let foreign = d.state().foreign.clone();
    let windows: Vec<(&Client, Option<&Foreign>)> = clients.iter().filter(|c| c.desktop() == Some(n) && c.app().is_none()).map(|c| (c, foreign.get(&c.address))).collect();
    let plan = session_plan(&cfg, &ws_apps, &windows);
    if plan.is_empty() {
        log::info!("сохранение сессии: посторонних окон на столе {n} нет, workspace {ws} не изменён");
        return Ok((ws, 0));
    }
    let mut used: Vec<(String, u32)> = clients.iter().filter_map(|c| c.app_instance()).collect();
    let mut ex = Vec::new();
    for a in &plan {
        let num = next_instance(&mut used, &a.app);
        ex.push(hypr::d_tag(&a.addr, &format!("app:{}#{num}", a.app)));
        log::info!("сохранение сессии: окно {} → приложение {} (экземпляр {num}) workspace {ws}", a.addr, a.app);
    }
    d.hypr().dispatch_all(&ex)?;
    let st = d.state_mut();
    for a in &plan {
        st.foreign.remove(&a.addr);
        if let Some(e) = &a.extra {
            st.extra.entry(ws.clone()).or_default().insert(a.app.clone(), e.clone());
        }
    }
    d.rebuild_cfg();
    d.broadcast();
    log::info!("workspace {ws}: принято окон — {}", plan.len());
    Ok((ws, plan.len()))
}

// ---- Запись workspace в конфиг ----------------------------------------------

pub fn save_workspace(d: &mut Daemon) -> Result<String> {
    let n = d.current_desktop();
    let Some(ws) = d.state().desktops.get(&n).and_then(|x| x.active.clone()) else { bail!("на столе {n} нет активного workspace") };
    let text = std::fs::read_to_string(d.cfg_path()).context("чтение конфига")?;
    if text != d.cfg_text() {
        d.reload_config();
        bail!("конфиг изменился с последнего чтения; перечитан, повторите команду");
    }
    let mut doc: DocumentMut = text.parse().context("разбор конфига для записи")?;
    let clients = d.clients()?;
    let mon = d.mon();
    let cfg = d.cfg().clone();
    let cells = d.state_mut().cells_of(&cfg, &ws, mon).clone();
    let main_app = d.state_mut().main_app(&cfg, &ws, mon);

    // Дополнительные приложения сессии переходят в конфиг: их описание идёт
    // в [apps], а место — вместе с остальными приложениями workspace, потому
    // что они уже в его назначениях ячеек.
    let extra: BTreeMap<String, ExtraApp> = d.state().extra.get(&ws).cloned().unwrap_or_default();

    let root = doc.as_table_mut();
    for (name, e) in &extra {
        if e.cmd.is_empty() {
            continue;
        }
        let apps_root = root.entry("apps").or_insert(Item::Table(Table::new())).as_table_mut().context("[apps] не таблица")?;
        apps_root.insert(name, Item::Table(app_table(&e.cmd, e.cwd.as_deref(), e.class.as_deref())));
    }
    let workspaces = root.entry("workspaces").or_insert(Item::Table(Table::new())).as_table_mut().context("[workspaces] не таблица")?;
    let wtab = workspaces.entry(&ws).or_insert(Item::Table(Table::new())).as_table_mut().context("workspace не таблица")?;
    if let Some(m) = &main_app {
        wtab["main"] = value(m.as_str());
    }
    // Режим задаёт только пользователь, командой он не меняется; запись нужна,
    // чтобы раздел workspace в файле описывал его целиком.
    if cfg.workspaces.get(&ws).map(|w| w.mode()) == Some(Mode::Stack) {
        wtab["mode"] = value("stack");
    }
    // Приложения по ячейкам; окно не в своей ячейке — rect.
    let mut apps_tab = Table::new();
    apps_tab.set_implicit(false);
    for (app, place) in &cells {
        let expected = d.state_mut().rect_for(&cfg, &ws, app, mon);
        // У приложения с несколькими окнами записывается место первого
        // экземпляра: стопка стоит в одном месте, и отдельных записей
        // для остальных окон в файле не появляется.
        let live = daemon::app_windows(&cfg, &clients, app).first().map(|c| c.rect());
        apps_tab.insert(app, app_item(place, live, expected));
    }
    // Посторонние окна на столе workspace становятся его приложениями: окно,
    // подходящее описанному в конфиге приложению по class и title, — под именем
    // этого приложения, остальные — новой записью в [apps] по команде процесса.
    // Окно получает тег приложения и перестаёт быть посторонним; свободные окна
    // на других столах не трогаются.
    let mut taken: Vec<String> = cfg.apps.keys().cloned().collect();
    let mut used: Vec<(String, u32)> = clients.iter().filter_map(|c| c.app_instance()).collect();
    let root = doc.as_table_mut();
    let mut ex = Vec::new();
    let mut adopted = Vec::new();
    for c in clients.iter().filter(|c| c.desktop() == Some(n)) {
        // Окно варианта считается окном своего семейства: если workspace
        // описывает семейство, второй записи о варианте не появляется.
        if c.app().is_some_and(|a| cells.keys().any(|x| cfg.app_is(&a, x))) {
            continue;
        }
        let Some(f) = d.state().foreign.get(&c.address).cloned() else {
            continue;
        };
        let tagged = c.app().filter(|a| cfg.apps.contains_key(a));
        let name = match tagged.clone().or_else(|| daemon::app_for_window(&cfg, c)) {
            Some(a) => a,
            None => {
                if f.cmd.is_empty() {
                    continue;
                }
                let name = app_name(&c.class, &taken);
                taken.push(name.clone());
                let apps_root = root.entry("apps").or_insert(Item::Table(Table::new())).as_table_mut().context("[apps] не таблица")?;
                apps_root.insert(&name, Item::Table(app_table(&f.cmd, f.cwd.as_deref(), Some(&c.class))));
                name
            }
        };
        if tagged.is_none() {
            ex.push(hypr::d_tag(&c.address, &format!("app:{name}#{}", next_instance(&mut used, &name))));
        }
        apps_tab.insert(&name, Item::Value(rect_item(c.rect())));
        adopted.push(c.address.clone());
        log::info!("сохранение: окно {} ({}) → приложение {name} workspace {ws}", c.address, c.class);
    }
    let wtab = doc["workspaces"][&ws].as_table_mut().unwrap();
    wtab.insert("apps", Item::Table(apps_tab));

    let path = d.cfg_path().to_path_buf();
    let out = doc.to_string();
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, &path)?;
    d.set_cfg_text(out);
    if !ex.is_empty() {
        d.hypr().dispatch_all(&ex)?;
    }
    for a in &adopted {
        d.state_mut().foreign.remove(a);
    }
    // Назначения ячеек workspace пересобираются из только что записанного конфига,
    // иначе новые приложения в них не попадут до перезапуска демона.
    // Дополнительные приложения сессии этого workspace перешли в конфиг.
    d.state_mut().cells.remove(&ws);
    d.state_mut().extra.remove(&ws);
    // Перечитывать конфиг здесь не нужно: запись во временный файл
    // с переименованием — завершённая запись, и наблюдатель каталога
    // присылает демону событие сам. Явный вызов давал второе перечитывание
    // и второй `hyprctl reload config-only` на одно сохранение.
    log::info!("workspace {ws} записан в конфиг");
    Ok(ws)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::daemon;
    use crate::hypr::test_client;

    const CFG: &str = r#"
[templates.thirds]
main = "center"
[templates.thirds.cells]
left   = { x = -805, y = 10, w = 1920, h = 2140 }
center = { x = 1125, y = 10, w = 1920, h = 2140 }

[apps.chromium]
cmd = "chromium"
class = "(?i)^chromium(-browser)?$"

[apps.herdr]
cmd = "wezterm-gui"
class = "^wezterm-herdr$"

[apps.chromium-mail]
family = "chromium"
cmd = "chromium"
class = "(?i)^chromium(-browser)?$"
title = "^Gmail"

[workspaces.work]
template = "thirds"
main = "herdr"
apps = { herdr = "center", chromium = "left" }
"#;

    fn at(addr: &str, tag: &str, r: PxRect) -> crate::hypr::Client {
        let mut c = test_client(addr, "chromium", "Новости", "1", &[tag]);
        c.at = (r.x, r.y);
        c.size = (r.w, r.h);
        c
    }

    fn foreign(cmd: &[&str]) -> Foreign {
        Foreign { rect: PxRect { x: 0, y: 0, w: 10, h: 10 }, cmd: cmd.iter().map(|s| s.to_string()).collect(), cwd: Some("/home/mne".into()) }
    }

    #[test]
    fn session_plan_names_windows() {
        let cfg = Config::parse(CFG).unwrap();
        let ws_apps = ["chromium".to_string()];
        let mut alien = test_client("0x1", "Galculator", "Калькулятор", "1", &[]);
        alien.at = (100, 200);
        alien.size = (800, 600);
        let known = test_client("0x2", "chromium", "Новости", "1", &[]);
        let outside = test_client("0x3", "wezterm-herdr", "herdr · dev-lab", "1", &[]);
        let mute = test_client("0x4", "Keepassxc", "Пароли", "1", &[]);

        let f_alien = foreign(&["galculator"]);
        let f_known = foreign(&["chromium"]);
        let f_outside = foreign(&["wezterm-gui"]);
        let f_mute = foreign(&[]);
        let windows: Vec<(&crate::hypr::Client, Option<&Foreign>)> =
            vec![(&alien, Some(&f_alien)), (&known, Some(&f_known)), (&outside, Some(&f_outside)), (&mute, Some(&f_mute))];
        let plan = session_plan(&cfg, &ws_apps, &windows);

        // Окно без подходящего приложения конфига даёт новую запись с классом,
        // командой, каталогом и своим прямоугольником.
        assert_eq!(plan[0].app, "galculator");
        let e = plan[0].extra.clone().unwrap();
        assert_eq!(e.class.as_deref(), Some("Galculator"));
        assert_eq!(e.cmd, vec!["galculator".to_string()]);
        assert_eq!(e.rect, PxRect { x: 100, y: 200, w: 800, h: 600 });
        // Окно приложения, которое уже входит в workspace: только тег,
        // места приложений не меняются.
        assert_eq!(plan[1], Adopted { addr: "0x2".into(), app: "chromium".into(), extra: None });
        // Окно приложения конфига, которого в workspace нет: запись только
        // о месте, нового приложения по классу окна не появляется.
        assert_eq!(plan[2].app, "herdr");
        assert_eq!(plan[2].extra.clone().unwrap().cmd, Vec::<String>::new());
        // Окно без командной строки пропускается: восстановить его нечем.
        assert_eq!(plan.len(), 3);
    }

    #[test]
    fn session_plan_uniquifies_names() {
        let cfg = Config::parse(CFG).unwrap();
        let a = test_client("0x1", "Galculator", "Первый", "1", &[]);
        let b = test_client("0x2", "Galculator", "Второй", "1", &[]);
        let f = foreign(&["galculator"]);
        let windows: Vec<(&crate::hypr::Client, Option<&Foreign>)> = vec![(&a, Some(&f)), (&b, Some(&f))];
        let plan = session_plan(&cfg, &[], &windows);
        assert_eq!(plan.iter().map(|a| a.app.as_str()).collect::<Vec<_>>(), vec!["galculator", "galculator-2"]);
    }

    #[test]
    fn instance_numbers_do_not_repeat_in_one_call() {
        let mut used = vec![("galculator".to_string(), 1)];
        assert_eq!(next_instance(&mut used, "galculator"), 2);
        assert_eq!(next_instance(&mut used, "galculator"), 3);
        assert_eq!(next_instance(&mut used, "chromium"), 1);
    }

    #[test]
    fn app_table_writes_command_and_class() {
        let t = app_table(&["galculator".to_string(), "--mode=paper".to_string()], Some("/home/mne"), Some("Galculator"));
        assert_eq!(t["cmd"].as_str(), Some("galculator"));
        assert_eq!(t["args"].as_array().unwrap().get(0).unwrap().as_str(), Some("--mode=paper"));
        assert_eq!(t["cwd"].as_str(), Some("/home/mne"));
        assert_eq!(t["class"].as_str(), Some("^Galculator$"));
    }

    #[test]
    fn rect_of_first_instance_only() {
        let cfg = Config::parse(CFG).unwrap();
        let cell = PxRect { x: -805, y: 10, w: 1920, h: 2140 };
        let moved = PxRect { x: 100, y: 200, w: 800, h: 600 };
        // Первый экземпляр сдвинут пользователем, второй стоит в ячейке.
        let clients = vec![at("0x2", "app:chromium#2", cell), at("0x1", "app:chromium#1", moved)];
        let wins = daemon::app_windows(&cfg, &clients, "chromium");
        assert_eq!(wins.len(), 2);
        let live = wins.first().map(|c| c.rect());
        assert_eq!(live, Some(moved));

        let mut tab = Table::new();
        tab.insert("chromium", app_item(&Place::Cell("left".into()), live, Some(cell)));
        // В файле одна запись о приложении, и это место первого экземпляра.
        assert_eq!(tab.len(), 1);
        let written = tab["chromium"].as_inline_table().unwrap()["rect"].as_inline_table().unwrap().clone();
        assert_eq!(written.get("x").unwrap().as_integer(), Some(100));
        assert_eq!(written.get("w").unwrap().as_integer(), Some(800));

        // Окно первого экземпляра в своей ячейке — записывается имя ячейки.
        let clients = vec![at("0x1", "app:chromium#1", cell), at("0x2", "app:chromium#2", moved)];
        let live = daemon::app_windows(&cfg, &clients, "chromium").first().map(|c| c.rect());
        assert_eq!(app_item(&Place::Cell("left".into()), live, Some(cell)).as_str(), Some("left"));
    }
}
