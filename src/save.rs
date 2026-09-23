//! Две команды сохранения. «Сохранить сессию» (`save-session`) записывает
//! снимок сессии: проходит по всем столам с активным workspace, принимает
//! в них ещё не учтённые окна, снимает с экрана раскладку каждого такого
//! workspace и пишет `default.toml`; файла конфига команда не трогает.
//! «Сохранить workspace» (`save-workspace`) записывает раскладку активного
//! workspace в `config.toml` через toml_edit с сохранением комментариев.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, Value, value};

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

/// Запись места приложения в таблице `apps` workspace (изменение live-layout,
/// решение D11): имя ячейки шаблона, если прямоугольник места `rect` с ней
/// совпадает — сначала проверяется исходная ячейка места `base`, затем ячейки
/// шаблона `cells` по порядку, — иначе `{ rect = … }`. Без прямоугольника
/// пишется исходное место как есть.
pub fn place_item(cells: &BTreeMap<String, PxRect>, base: Option<&Place>, rect: Option<PxRect>) -> Option<Item> {
    let Some(r) = rect else {
        return match base? {
            Place::Cell(c) => Some(value(c.as_str())),
            Place::Rect { rect } => Some(Item::Value(rect_item(*rect))),
        };
    };
    if let Some(Place::Cell(c)) = base
        && cells.get(c) == Some(&r)
    {
        return Some(value(c.as_str()));
    }
    match cells.iter().find(|(_, x)| **x == r) {
        Some((name, _)) => Some(value(name.as_str())),
        None => Some(Item::Value(rect_item(r))),
    }
}

/// Записать место приложения `name` в таблицу `apps` раздела workspace
/// на месте (изменение workspace-overrides, решение D9). `short` — место
/// в короткой форме от `place_item`: имя ячейки строкой или `{ rect = … }`.
/// Строковая запись и запись `{ rect = … }` без других полей заменяются
/// короткой формой; у записи-таблицы с `chain` или `mode` меняется только
/// поле места (`cell` или `rect`) на прежней позиции, остальные поля
/// остаются. Украшения записи (пробелы, комментарий в конце строки)
/// сохраняются. Запись, которой в таблице не было, добавляется в конец;
/// тогда возвращается `true`.
fn set_place(apps: &mut dyn TableLike, name: &str, short: Item) -> bool {
    let (key, place): (&str, Value) = match short.as_value() {
        Some(Value::InlineTable(t)) => match t.get("rect") {
            Some(r) => ("rect", r.clone()),
            None => return false,
        },
        Some(v) => ("cell", v.clone()),
        None => return false,
    };
    let is_place = |k: &str| k == "cell" || k == "rect";
    let Some(item) = apps.get_mut(name) else {
        apps.insert(name, short);
        return true;
    };
    match item {
        Item::Value(Value::InlineTable(t)) if t.iter().any(|(k, _)| !is_place(k)) => {
            let mut out = InlineTable::new();
            let mut placed = false;
            for (k, v) in t.iter() {
                if !is_place(k) {
                    out.insert(k, v.clone());
                } else if !placed {
                    out.insert(key, place.clone());
                    placed = true;
                }
            }
            if !placed {
                out.insert(key, place);
            }
            *out.decor_mut() = t.decor().clone();
            *t = out;
        }
        Item::Table(t) => {
            // Запись отдельной таблицей `[workspaces.<ws>.apps.<имя>]`:
            // её форма сохраняется, меняется только поле места.
            t.remove("cell");
            t.remove("rect");
            t.insert(key, Item::Value(place));
        }
        Item::Value(v) => {
            let decor = v.decor().clone();
            if let Ok(mut nv) = short.into_value() {
                *nv.decor_mut() = decor;
                *v = nv;
            }
        }
        other => *other = short,
    }
    false
}

/// Записать места приложений в таблицу `apps` раздела workspace `ws`
/// по порядку `places` (решение D9).
fn write_places(doc: &mut DocumentMut, ws: &str, places: Vec<(String, Item)>) -> Result<()> {
    let wtab = doc["workspaces"][ws].as_table_mut().context("workspace не таблица")?;
    let apps = wtab
        .entry("apps")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(false);
            Item::Table(t)
        })
        .as_table_like_mut()
        .context("apps workspace не таблица")?;
    let mut added = false;
    for (name, short) in places {
        added |= set_place(apps, &name, short);
    }
    // Таблица apps в одну строку после добавления записи форматируется
    // заново: иначе пробел, стоявший перед закрывающей скобкой, оказался бы
    // перед запятой. Комментариев внутри такой таблицы не бывает.
    if added && let Some(Item::Value(Value::InlineTable(t))) = wtab.get_mut("apps") {
        t.fmt();
    }
    Ok(())
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

/// Записать снимок сессии (спецификация ws-sessions, «Файлы сессий»).
/// Команда проходит по всем столам с активным workspace, принимает в них
/// ещё не учтённые окна по общим правилам, снимает с экрана раскладку каждого
/// такого workspace (изменение live-layout, решение D13) и пишет
/// `default.toml`. Сдвинутое окно даёт изменённое место своего приложения;
/// прямоугольник записи дополнительного приложения — место, с которым оно
/// принято, — не переписывается. Файл конфига не читается и не пишется.
/// Возвращает число workspace, окон в них и принятых окон.
pub fn save_session(d: &mut Daemon) -> Result<(usize, usize, usize)> {
    // Сохранение — явное действие пользователя: окна, которых ждёт
    // восстановление из прежнего снимка, больше не ждутся, а записи
    // workspace, ещё не поднятых после старта, переходят в новый снимок
    // (изменение session-instances, решения D10, D16).
    d.end_restore_wait(None, false, "сохранение сессии");
    let desks: Vec<(u8, String)> = d.state().desktops.iter().filter_map(|(n, x)| Some((*n, x.active.clone()?))).collect();
    let mut adopted = 0;
    for (n, ws) in &desks {
        adopted += accept_desktop(d, *n, ws)?;
    }
    let clients = d.clients()?;
    for (n, ws) in &desks {
        d.absorb(ws, *n, &clients);
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
    // Раскладка снимается с экрана перед записью (решение D11): сдвинутое
    // окно даёт место своего приложения. Общее окно, ушедшее за пользователем
    // на другой стол, даёт место, снятое, когда оно стояло здесь.
    d.absorb(&ws, n, &clients);
    let cells = d.state_mut().cells_of(&cfg, &ws, mon).clone();
    let main_app = d.state_mut().main_app(&cfg, &ws, mon);
    let template: BTreeMap<String, PxRect> = cfg
        .workspaces
        .get(&ws)
        .and_then(|w| cfg.templates.get(&w.template))
        .map(|t| t.cells.iter().map(|(k, r)| (k.clone(), r.resolve(mon.0, mon.1))).collect())
        .unwrap_or_default();

    // Дополнительные приложения сессии переходят в конфиг: их описание идёт
    // в [apps], а место — вместе с остальными приложениями workspace, потому
    // что они уже в его раскладке.
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
    // Места приложений по раскладке в памяти: прямоугольник места,
    // совпадающий с ячейкой шаблона, — именем ячейки, иначе rect. У приложения
    // с несколькими окнами место снято с первого экземпляра, и отдельных
    // записей для остальных окон не появляется. Записи идут в порядке раздела
    // эффективного конфига: сначала записи файла, затем дополнительные
    // приложения сессии; в файле они правятся на месте (решение D9
    // изменения workspace-overrides), и новые встают в конец.
    let mut order: Vec<String> = cfg.workspaces.get(&ws).map(|w| w.apps.keys().cloned().collect()).unwrap_or_default();
    order.extend(cells.keys().filter(|a| !order.contains(a)).cloned().collect::<Vec<_>>());
    let mut places: Vec<(String, Item)> = Vec::new();
    for app in &order {
        let Some(place) = cells.get(app) else { continue };
        let rect = d.state_mut().rect_for(&cfg, &ws, app, mon);
        if let Some(item) = place_item(&template, Some(place), rect) {
            places.push((app.clone(), item));
        }
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
        places.push((name.clone(), Item::Value(rect_item(c.rect()))));
        adopted.push(c.address.clone());
        log::info!("сохранение: окно {} ({}) → приложение {name} workspace {ws}", c.address, c.class);
    }
    write_places(&mut doc, &ws, places)?;

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
    // Раскладка workspace собирается заново из только что записанного
    // конфига: изменённые места стали местами из описания, а новые
    // приложения иначе не попали бы в неё до перезапуска демона.
    // Дополнительные приложения сессии этого workspace перешли в конфиг.
    let st = d.state_mut();
    st.cells.remove(&ws);
    st.moved.remove(&ws);
    st.main.remove(&ws);
    st.extra.remove(&ws);
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
    use crate::state::State;

    fn placed(mut c: Client, r: PxRect) -> Client {
        c.at = (r.x, r.y);
        c.size = (r.w, r.h);
        c
    }

    /// Шаблон `thirds` из CFG в пикселях: ячейки по порядку имён.
    fn thirds(cfg: &Config) -> BTreeMap<String, PxRect> {
        cfg.templates["thirds"].cells.iter().map(|(k, r)| (k.clone(), r.resolve(3840, 2160))).collect()
    }

    /// Места приложений workspace `ws` для записи — как в `save_workspace`:
    /// раскладка снимается со стола 1, места берутся из неё.
    fn layout_items(cfg: &Config, st: &mut State, ws: &str, clients: &[Client]) -> Vec<(String, String)> {
        let mon = (3840, 2160);
        daemon::absorb_into(cfg, st, ws, 1, clients, mon);
        let cells = st.cells_of(cfg, ws, mon).clone();
        cells
            .iter()
            .filter_map(|(app, place)| {
                let rect = st.rect_for(cfg, ws, app, mon);
                place_item(&thirds(cfg), Some(place), rect).map(|i| (app.clone(), i.to_string().trim().to_string()))
            })
            .collect()
    }

    fn work_on_one() -> State {
        let mut st = State::default();
        st.desktops.insert(1, crate::state::Desktop { workspaces: vec!["work".into()], active: Some("work".into()) });
        st
    }

    #[test]
    fn save_workspace_writes_shared_window() {
        let file = Config::parse(&format!("{CFG}\n[apps.chrome-ai]\ncmd = \"google-chrome\"\nclass = \"^google-chrome-ai$\"\n")).unwrap();
        let mut st = work_on_one();
        let record = PxRect { x: 960, y: 540, w: 1920, h: 1080 };
        st.extra.entry("work".into()).or_default().insert("chrome-ai".into(), ExtraApp { rect: record, ..ExtraApp::default() });
        let (cfg, _) = daemon::merge_extra(&file, &st.extra);
        let moved = PxRect { x: 800, y: 300, w: 1920, h: 1080 };
        let left = PxRect { x: -805, y: 10, w: 1920, h: 2140 };
        let clients = vec![
            // Общее окно, перетащенное в work и сдвинутое там.
            placed(test_client("0x1", "google-chrome-ai", "ИИ", "1", &["app:chrome-ai#1", "ws:surf", "ws:work"]), moved),
            // Первый экземпляр chromium убран из work и остался в dev-front.
            placed(test_client("0x2", "chromium", "Новости", "3", &["app:chromium#1", "ws:dev-front"]), PxRect { x: 5, y: 5, w: 100, h: 100 }),
            placed(test_client("0x3", "chromium", "Почта", "1", &["app:chromium#2", "ws:work"]), left),
        ];
        let items = layout_items(&cfg, &mut st, "work", &clients);
        let item = |app: &str| items.iter().find(|(a, _)| a == app).map(|(_, i)| i.clone()).unwrap();
        // Приложение общего окна записывается под своим именем с местом окна
        // в этом workspace.
        assert_eq!(item("chrome-ai"), rect_item(moved).to_string().trim());
        // Отделённое окно места не даёт: считается второй экземпляр, и ячейка
        // chromium остаётся left.
        assert_eq!(item("chromium"), "\"left\"");
        // Окно ушло за пользователем на стол 2 и стоит там в другом месте:
        // место в work — снятое, когда окно стояло здесь, а не нынешнее.
        let mut away = clients.clone();
        away[0].workspace.name = "2".into();
        away[0].at = (1910, 10);
        let items = layout_items(&cfg, &mut st, "work", &away);
        assert_eq!(items.iter().find(|(a, _)| a == "chrome-ai").unwrap().1, rect_item(moved).to_string().trim());
    }

    #[test]
    fn save_writes_layout_places() {
        let file = Config::parse(&format!("{CFG}\n[apps.chrome-ai]\ncmd = \"google-chrome\"\nclass = \"^google-chrome-ai$\"\n[apps.neovide]\ncmd = \"neovide\"\nclass = \"^neovide$\"\n").replace("apps = { herdr = \"center\", chromium = \"left\" }", "apps = { herdr = \"center\", chromium = \"left\", neovide = \"right\" }").replace("center = { x = 1125, y = 10, w = 1920, h = 2140 }\n", "center = { x = 1125, y = 10, w = 1920, h = 2140 }\nright = { x = 3055, y = 10, w = 1920, h = 2140 }\n")).unwrap();
        let mut st = work_on_one();
        let mon = (3840, 2160);
        let record = PxRect { x: 960, y: 540, w: 1920, h: 1080 };
        st.extra.entry("work".into()).or_default().insert("chrome-ai".into(), ExtraApp { rect: record, ..ExtraApp::default() });
        let (cfg, _) = daemon::merge_extra(&file, &st.extra);
        let center = PxRect { x: 1125, y: 10, w: 1920, h: 2140 };
        let left = PxRect { x: -805, y: 10, w: 1920, h: 2140 };
        let r = PxRect { x: 400, y: 300, w: 1400, h: 1000 };
        // Пример пользователя после обмена: chrome-ai на главном месте, herdr
        // в прямоугольнике, куда сдвигали chrome-ai; neovide сдвинут точно
        // в ячейку left, chromium — в другое место.
        st.cells_of(&cfg, "work", mon);
        daemon::swap_places(&mut st, "work", Some("herdr"), "chrome-ai", "center", &|_| true);
        let clients = vec![
            placed(test_client("0x1", "google-chrome-ai", "ИИ", "1", &["app:chrome-ai#1", "ws:work"]), center),
            placed(test_client("0x2", "wezterm-herdr", "herdr · dev-lab", "1", &["app:herdr#1", "ws:work"]), r),
            placed(test_client("0x3", "neovide", "[Scratch]", "1", &["app:neovide#1", "ws:work"]), left),
            placed(test_client("0x4", "chromium", "Новости", "1", &["app:chromium#1", "ws:work"]), PxRect { x: 100, y: 200, w: 800, h: 600 }),
        ];
        let items = layout_items(&cfg, &mut st, "work", &clients);
        let item = |app: &str| items.iter().find(|(a, _)| a == app).map(|(_, i)| i.clone()).unwrap();
        assert_eq!(st.main_app(&cfg, "work", mon).as_deref(), Some("chrome-ai"));
        assert_eq!(item("chrome-ai"), "\"center\"");
        assert_eq!(item("herdr"), rect_item(r).to_string().trim());
        // Прямоугольник, равный ячейке left, пишется именем ячейки.
        assert_eq!(item("neovide"), "\"left\"");
        assert_eq!(item("chromium"), rect_item(PxRect { x: 100, y: 200, w: 800, h: 600 }).to_string().trim());
        // Без прямоугольника место пишется как есть.
        assert_eq!(place_item(&thirds(&cfg), Some(&Place::Cell("right".into())), None).unwrap().to_string().trim(), "\"right\"");
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

    const SURF: &str = r#"# Браузеры
[workspaces.surf]
template = "overlay"
mode = "stack"
# Две клавиши у каждого браузера.
[workspaces.surf.apps]
# Chrome профиля Default
chrome = { cell = "left", chain = "SUPER+B" }
yandex-browser = "center"  # главный
chrome-ai = { cell = "right", chain = "SUPER+V", mode = "stack" }

[startup]
workspace = "surf"
"#;

    #[test]
    fn save_keeps_overrides() {
        let mut doc: DocumentMut = SURF.parse().unwrap();
        let moved = PxRect { x: 700, y: 200, w: 1920, h: 1080 };
        let places = vec![
            ("chrome".to_string(), value("left")),
            ("yandex-browser".to_string(), Item::Value(rect_item(moved))),
            ("chrome-ai".to_string(), Item::Value(rect_item(moved))),
        ];
        write_places(&mut doc, "surf", places).unwrap();
        let out = doc.to_string();
        // Запись-таблица сохраняет chain и mode, поле cell заменено на rect
        // на той же позиции; строковая запись стала { rect = … } и сохранила
        // комментарий в конце строки.
        assert!(out.contains("chrome = { cell = \"left\", chain = \"SUPER+B\" }"), "{out}");
        assert!(out.contains("yandex-browser = { rect = { x = 700, y = 200, w = 1920, h = 1080 } }  # главный"), "{out}");
        assert!(out.contains("chrome-ai = { rect = { x = 700, y = 200, w = 1920, h = 1080 }, chain = \"SUPER+V\", mode = \"stack\" }"), "{out}");
        assert!(out.contains("# Chrome профиля Default\nchrome ="), "{out}");
        assert!(out.contains("# Две клавиши у каждого браузера.\n[workspaces.surf.apps]"), "{out}");
        // Записанный файл читается, место и переопределения на месте.
        let text = format!("[templates.overlay]\nmain = \"center\"\ncells = {{ left = {{ x = 0, y = 0, w = 1, h = 1 }}, center = {{ x = 0, y = 0, w = 1, h = 1 }}, right = {{ x = 0, y = 0, w = 1, h = 1 }} }}\n[apps.chrome]\ncmd = \"c\"\n[apps.chrome-ai]\ncmd = \"c\"\n[apps.yandex-browser]\ncmd = \"y\"\n{out}");
        let cfg = Config::parse(&text).unwrap();
        let ai = &cfg.workspaces["surf"].apps["chrome-ai"];
        assert_eq!(ai.place, crate::config::Placement::Rect { rect: moved.to_rect() });
        assert_eq!((ai.chain.as_deref(), ai.mode.as_deref()), (Some("SUPER+V"), Some("stack")));
        // Обратно в ячейку: запись-таблица получает cell, { rect } без других
        // полей становится строкой.
        let mut doc: DocumentMut = out.parse().unwrap();
        write_places(&mut doc, "surf", vec![("yandex-browser".to_string(), value("center")), ("chrome-ai".to_string(), value("right"))]).unwrap();
        let out = doc.to_string();
        assert!(out.contains("yandex-browser = \"center\"  # главный"), "{out}");
        assert!(out.contains("chrome-ai = { cell = \"right\", chain = \"SUPER+V\", mode = \"stack\" }"), "{out}");
    }

    #[test]
    fn save_keeps_entry_order() {
        let mut doc: DocumentMut = SURF.parse().unwrap();
        // Порядок places — порядок раздела эффективного конфига: записи файла,
        // затем дополнительные приложения сессии.
        let calc = PxRect { x: 2600, y: 1500, w: 600, h: 400 };
        let places = vec![
            ("chrome".to_string(), value("left")),
            ("yandex-browser".to_string(), value("center")),
            ("chrome-ai".to_string(), value("right")),
            ("galculator".to_string(), Item::Value(rect_item(calc))),
        ];
        write_places(&mut doc, "surf", places).unwrap();
        let out = doc.to_string();
        let pos = |k: &str| out.find(&format!("\n{k} =")).unwrap_or_else(|| panic!("{k}: {out}"));
        assert!(pos("chrome") < pos("yandex-browser") && pos("yandex-browser") < pos("chrome-ai") && pos("chrome-ai") < pos("galculator"), "{out}");
        assert!(out.contains("galculator = { rect = { x = 2600, y = 1500, w = 600, h = 400 } }\n\n[startup]"), "{out}");
        // Без правки мест файл не меняется ни на символ.
        let mut doc: DocumentMut = SURF.parse().unwrap();
        write_places(&mut doc, "surf", vec![("chrome".to_string(), value("left")), ("yandex-browser".to_string(), value("center")), ("chrome-ai".to_string(), value("right"))]).unwrap();
        assert_eq!(doc.to_string(), SURF);
        // Раздел с таблицей apps в одну строку правится так же.
        let mut doc: DocumentMut = "[workspaces.w]\napps = { a = \"left\", b = { cell = \"right\", chain = \"SUPER+X\" } }\n".parse().unwrap();
        write_places(&mut doc, "w", vec![("a".to_string(), value("left")), ("b".to_string(), Item::Value(rect_item(calc))), ("c".to_string(), value("left"))]).unwrap();
        assert_eq!(doc.to_string(), "[workspaces.w]\napps = { a = \"left\", b = { rect = { x = 2600, y = 1500, w = 600, h = 400 }, chain = \"SUPER+X\" }, c = \"left\" }\n");
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
        let mut clients = clients;
        for c in clients.iter_mut() {
            c.tags.push("ws:work".into());
        }
        let mut st = work_on_one();
        let items = layout_items(&cfg, &mut st, "work", &clients);
        // В файле одна запись о приложении, и это место первого экземпляра.
        let chromium: Vec<&(String, String)> = items.iter().filter(|(a, _)| a == "chromium").collect();
        assert_eq!(chromium.len(), 1);
        assert_eq!(chromium[0].1, rect_item(moved).to_string().trim());

        // Окно первого экземпляра в своей ячейке — записывается имя ячейки.
        let clients = vec![at("0x1", "app:chromium#1", cell), at("0x2", "app:chromium#2", moved)].into_iter().map(|mut c| {
            c.tags.push("ws:work".into());
            c
        });
        let clients: Vec<Client> = clients.collect();
        let mut st = work_on_one();
        let items = layout_items(&cfg, &mut st, "work", &clients);
        assert_eq!(items.iter().find(|(a, _)| a == "chromium").unwrap().1, "\"left\"");
    }
}
