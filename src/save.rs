//! Две команды сохранения. «Сохранить сессию» (`save-session`) записывает
//! снимок сессии: проходит по всем столам с активным workspace, принимает
//! в них ещё не учтённые окна, обновляет места дополнительных приложений
//! по нынешнему положению окон и пишет `default.toml`; файла конфига команда
//! не трогает. «Сохранить workspace» (`save-workspace`) записывает состояние
//! активного workspace в `config.toml` через toml_edit с сохранением
//! комментариев.

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

/// Нынешнее место приложения `app` в workspace `ws` для записи в конфиг:
/// прямоугольник первого нескрытого экземпляра, входящего в `ws` по тегу
/// состава (изменение shared-windows, решение D12). Окно, убранное
/// из workspace командой отделения, места не даёт.
pub fn live_rect(cfg: &Config, clients: &[Client], ws: &str, app: &str) -> Option<PxRect> {
    daemon::app_windows(cfg, clients, app).into_iter().find(|c| c.in_ws(ws) && !c.on_hidden()).map(|c| c.rect())
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

/// Что сделать с окном при сохранении сессии.
#[derive(Debug, Clone, PartialEq)]
pub struct Adopted {
    pub addr: String,
    pub app: String,
    /// Запись для сессии; `None` — приложение уже входит в workspace,
    /// и окно становится просто ещё одним его экземпляром.
    pub extra: Option<ExtraApp>,
}

/// План принятия окон в workspace: решение по каждому окну принимает
/// `join_window` — те же правила, что у окна, появившегося по ходу работы.
/// Место записывается нынешнее: окно, уже поставленное пользователем куда
/// нужно, команда не двигает. Окно, которое принимать не следует (диалог,
/// окно без класса, класс из `ignore_classes`, окно без командной строки),
/// остаётся свободным со строкой в журнале.
fn session_plan(cfg: &Config, file: &Config, ws: &str, taken: &mut Vec<String>, windows: &[(&Client, Option<&Foreign>)]) -> Vec<Adopted> {
    let mut out = Vec::new();
    for (c, f) in windows {
        let cmd: Vec<String> = f.map(|f| f.cmd.clone()).unwrap_or_default();
        match daemon::join_window(cfg, file, Some(ws), c, &cmd, taken) {
            daemon::Join::App(app) => out.push(Adopted { addr: c.address.clone(), app, extra: None }),
            daemon::Join::AppPlace(app) => {
                out.push(Adopted { addr: c.address.clone(), app, extra: Some(ExtraApp { rect: c.rect(), ..ExtraApp::default() }) });
            }
            daemon::Join::Extra(name) => {
                if !taken.contains(&name) {
                    taken.push(name.clone());
                }
                let extra = ExtraApp { class: Some(c.class.clone()), cmd, cwd: f.and_then(|f| f.cwd.clone()), rect: c.rect() };
                out.push(Adopted { addr: c.address.clone(), app: name, extra: Some(extra) });
            }
            daemon::Join::Free(reason) => {
                log::info!("сохранение сессии: окно {} ({}) осталось свободным: {}", c.address, c.class, reason.text());
            }
        }
    }
    out
}

/// Принять в workspace `ws` стола `n` окна, которые в нём ещё не учтены.
/// Возвращает число принятых окон.
fn accept_desktop(d: &mut Daemon, n: u8, ws: &str) -> Result<usize> {
    let clients = d.clients()?;
    let cfg = d.cfg().clone();
    let file = d.cfg_file().clone();
    let foreign = d.state().foreign.clone();
    let windows: Vec<(&Client, Option<&Foreign>)> = clients.iter().filter(|c| c.desktop() == Some(n) && c.app().is_none()).map(|c| (c, foreign.get(&c.address))).collect();
    let mut taken = daemon::taken_names(&cfg, &d.state().extra);
    let plan = session_plan(&cfg, &file, ws, &mut taken, &windows);
    if plan.is_empty() {
        return Ok(0);
    }
    let mut used: Vec<(String, u32)> = clients.iter().filter_map(|c| c.app_instance()).collect();
    let mut ex = Vec::new();
    for a in &plan {
        let num = next_instance(&mut used, &a.app);
        ex.push(hypr::d_tag(&a.addr, &format!("app:{}#{num}", a.app)));
        ex.push(hypr::d_tag(&a.addr, &hypr::ws_tag(ws)));
        log::info!("сохранение сессии: окно {} → приложение {} (экземпляр {num}) workspace {ws}", a.addr, a.app);
    }
    d.hypr().dispatch_all(&ex)?;
    let st = d.state_mut();
    for a in &plan {
        st.foreign.remove(&a.addr);
        if let Some(e) = &a.extra {
            st.extra.entry(ws.to_string()).or_default().insert(a.app.clone(), e.clone());
        }
    }
    d.rebuild_cfg();
    Ok(plan.len())
}

/// Место записи дополнительного приложения `name` workspace `ws` по его
/// окнам (спецификация ws-sessions, «Дополнительные приложения сессии»;
/// изменение shared-windows, решение D12). Берётся первый экземпляр,
/// входящий в `ws`. Окно на столе своего workspace (`desk` — стол, где `ws`
/// активен) даёт нынешний прямоугольник; общее окно, ушедшее за
/// пользователем на другой стол, — прямоугольник, запомненный для `ws`
/// в режиме `stack` (`kept`), а без него `None`: нынешнее положение окна
/// описывает место в другом workspace, и запись не меняется.
pub fn extra_rect(cfg: &Config, clients: &[Client], ws: &str, desk: Option<u8>, kept: &dyn Fn(&str) -> Option<PxRect>, name: &str) -> Option<PxRect> {
    let c = daemon::app_windows(cfg, clients, name).into_iter().find(|c| c.in_ws(ws) && !c.on_hidden())?;
    if desk.is_some() && c.desktop() == desk {
        return Some(c.rect());
    }
    kept(&c.address)
}

/// Обновить места дополнительных приложений сессии по нынешнему положению
/// их окон: команда сохраняет то, что на экране (`extra_rect`). Возвращает
/// число изменённых записей.
fn update_extra_rects(d: &mut Daemon, ws: &str, clients: &[Client]) -> usize {
    let cfg = d.cfg().clone();
    let names: Vec<String> = d.state().extra.get(ws).map(|m| m.keys().cloned().collect()).unwrap_or_default();
    let desk = d.state().desktops.iter().find(|(_, x)| x.active.as_deref() == Some(ws)).map(|(n, _)| *n);
    let stack = cfg.workspaces.get(ws).map(|w| w.mode()) == Some(Mode::Stack);
    let geom = d.state().geom.get(ws).cloned().unwrap_or_default();
    let kept = |a: &str| if stack { geom.get(a).copied() } else { None };
    let mut changed = 0;
    for name in names {
        let Some(rect) = extra_rect(&cfg, clients, ws, desk, &kept, &name) else { continue };
        let st = d.state_mut();
        let Some(e) = st.extra.get_mut(ws).and_then(|m| m.get_mut(&name)) else { continue };
        if e.rect == rect {
            continue;
        }
        e.rect = rect;
        // Назначение места в workspace собрано раньше, поэтому новое место
        // кладётся и туда: иначе поднятие вернуло бы окно на прежнее.
        if let Some(cells) = st.cells.get_mut(ws) {
            cells.insert(name.clone(), Place::Rect { rect });
        }
        changed += 1;
        log::info!("сохранение сессии: место {name} в workspace {ws} — {},{} {}×{}", rect.x, rect.y, rect.w, rect.h);
    }
    changed
}

/// Записать снимок сессии (спецификация ws-sessions, «Файлы сессий»).
/// Команда проходит по всем столам с активным workspace, принимает в них
/// ещё не учтённые окна по общим правилам, обновляет места дополнительных
/// приложений и пишет `default.toml`. Файл конфига не читается и не пишется;
/// места приложений, описанных в конфиге, не меняются. Возвращает число
/// workspace, окон в них и принятых окон.
pub fn save_session(d: &mut Daemon) -> Result<(usize, usize, usize)> {
    let desks: Vec<(u8, String)> = d.state().desktops.iter().filter_map(|(n, x)| Some((*n, x.active.clone()?))).collect();
    let mut adopted = 0;
    for (n, ws) in &desks {
        adopted += accept_desktop(d, *n, ws)?;
    }
    let clients = d.clients()?;
    for (_, ws) in &desks {
        update_extra_rects(d, ws, &clients);
    }
    let windows: usize = desks.iter().map(|(_, ws)| daemon::ws_windows(&clients, ws).len()).sum();
    d.save_session_file();
    d.broadcast();
    log::info!("снимок сессии записан: workspace — {}, окон в них — {windows}, принято окон — {adopted}", desks.len());
    Ok((desks.len(), windows, adopted))
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
        // экземпляра, входящего в workspace: стопка стоит в одном месте,
        // и отдельных записей для остальных окон в файле не появляется.
        // Окно, убранное из workspace командой отделения, места не даёт,
        // а общее окно даёт место, которое занимает здесь (решение D12).
        let live = live_rect(&cfg, &clients, &ws, app);
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
        // Окна workspace определяются по тегам состава; окно варианта
        // считается окном своего семейства, и если workspace описывает
        // семейство, второй записи о варианте не появляется.
        if c.in_ws(&ws) {
            continue;
        }
        let Some(f) = d.state().foreign.get(&c.address).cloned() else {
            continue;
        };
        // Окно без класса остаётся свободным всегда (изменение
        // classless-windows-stay-free, решение D1).
        if daemon::classless(c) {
            log::info!("сохранение: окно {} («{}») осталось свободным: у окна нет класса", c.address, c.title);
            continue;
        }
        let tagged = c.app().filter(|a| cfg.apps.contains_key(a));
        let name = match tagged.clone().or_else(|| daemon::app_for_window(&cfg, c)) {
            Some(a) => a,
            None => {
                if f.cmd.is_empty() {
                    continue;
                }
                let name = daemon::app_name(&c.class, &taken);
                taken.push(name.clone());
                let apps_root = root.entry("apps").or_insert(Item::Table(Table::new())).as_table_mut().context("[apps] не таблица")?;
                apps_root.insert(&name, Item::Table(app_table(&f.cmd, f.cwd.as_deref(), Some(&c.class))));
                name
            }
        };
        if tagged.is_none() {
            ex.push(hypr::d_tag(&c.address, &format!("app:{name}#{}", next_instance(&mut used, &name))));
        }
        ex.push(hypr::d_tag(&c.address, &hypr::ws_tag(&ws)));
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

    fn placed(mut c: Client, r: PxRect) -> Client {
        c.at = (r.x, r.y);
        c.size = (r.w, r.h);
        c
    }

    #[test]
    fn save_workspace_writes_shared_window() {
        let cfg = Config::parse(&format!("{CFG}\n[apps.chrome-ai]\ncmd = \"google-chrome\"\nclass = \"^google-chrome-ai$\"\n")).unwrap();
        let moved = PxRect { x: 800, y: 300, w: 1920, h: 1080 };
        let left = PxRect { x: -805, y: 10, w: 1920, h: 2140 };
        let clients = vec![
            // Общее окно, перетащенное в work и сдвинутое там.
            placed(test_client("0x1", "google-chrome-ai", "ИИ", "1", &["app:chrome-ai#1", "ws:surf", "ws:work"]), moved),
            // Первый экземпляр chromium убран из work и остался в dev-front.
            placed(test_client("0x2", "chromium", "Новости", "3", &["app:chromium#1", "ws:dev-front"]), PxRect { x: 5, y: 5, w: 100, h: 100 }),
            placed(test_client("0x3", "chromium", "Почта", "1", &["app:chromium#2", "ws:work"]), left),
        ];
        // Приложение общего окна записывается под своим именем с местом окна
        // в этом workspace.
        let r = live_rect(&cfg, &clients, "work", "chrome-ai");
        assert_eq!(r, Some(moved));
        let item = app_item(&Place::Rect { rect: PxRect::default() }, r, None);
        assert_eq!(item.to_string(), rect_item(moved).to_string());
        // Отделённое окно места не даёт: считается второй экземпляр, и ячейка
        // chromium остаётся left.
        let r = live_rect(&cfg, &clients, "work", "chromium");
        assert_eq!(r, Some(left));
        assert_eq!(app_item(&Place::Cell("left".into()), r, Some(left)).as_str(), Some("left"));
    }

    #[test]
    fn extra_rect_of_shared_window_elsewhere() {
        let cfg = Config::parse(&format!("{CFG}\n[apps.chrome-ai]\ncmd = \"google-chrome\"\nclass = \"^google-chrome-ai$\"\n")).unwrap();
        let on_two = PxRect { x: 1910, y: 10, w: 1920, h: 2140 };
        let kept = PxRect { x: 700, y: 200, w: 1920, h: 1080 };
        // Окно chrome-ai входит в work (стол 1) и surf, пользователь на столе 2,
        // окно стоит там.
        let clients = vec![placed(test_client("0x1", "google-chrome-ai", "ИИ", "2", &["app:chrome-ai#1", "ws:surf", "ws:work"]), on_two)];
        let none = |_: &str| None;
        // work в режиме обмена ячеек: запись не меняется.
        assert_eq!(extra_rect(&cfg, &clients, "work", Some(1), &none, "chrome-ai"), None);
        // Режим stack с запомненным прямоугольником: берётся он.
        let remembered = |a: &str| (a == "0x1").then_some(kept);
        assert_eq!(extra_rect(&cfg, &clients, "work", Some(1), &remembered, "chrome-ai"), Some(kept));
        // Окно на столе своего workspace даёт нынешний прямоугольник.
        assert_eq!(extra_rect(&cfg, &clients, "surf", Some(2), &remembered, "chrome-ai"), Some(on_two));
        // Приложение без окон в workspace записи не меняет.
        assert_eq!(extra_rect(&cfg, &clients, "dev-front", Some(3), &none, "chrome-ai"), None);
    }

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

[workspaces.solo]
template = "thirds"
main = "chromium"
apps = { chromium = "left" }
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
        // В `solo` описан только chromium: окно herdr даёт запись о месте.
        let ws = "solo";
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
        let mut taken: Vec<String> = cfg.apps.keys().cloned().collect();
        let plan = session_plan(&cfg, &cfg, ws, &mut taken, &windows);

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
        let mut taken: Vec<String> = cfg.apps.keys().cloned().collect();
        let plan = session_plan(&cfg, &cfg, "solo", &mut taken, &windows);
        assert_eq!(plan.iter().map(|a| a.app.as_str()).collect::<Vec<_>>(), vec!["galculator", "galculator-2"]);
    }

    #[test]
    fn session_plan_leaves_classless_windows_free() {
        // Диспетчер задач браузера: окно без класса с командной строкой браузера.
        let cfg = Config::parse(CFG).unwrap();
        let tm = test_client("0x1", "", "Диспетчер задач – Chromium", "1", &[]);
        let f = foreign(&["/usr/lib/chromium/chromium", "--ozone-platform=wayland"]);
        let windows: Vec<(&crate::hypr::Client, Option<&Foreign>)> = vec![(&tm, Some(&f))];
        let mut taken: Vec<String> = cfg.apps.keys().cloned().collect();
        assert!(session_plan(&cfg, &cfg, "solo", &mut taken, &windows).is_empty());
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
