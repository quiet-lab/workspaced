//! Сессии: снимок состояния — `default` по команде сохранения, именованные
//! по команде `session save`, — восстановление при старте (лениво, экземпляры
//! по плану восстановления) и загрузка со сверкой по экземплярам
//! (спецификация ws-sessions).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::{Config, Mode, PxRect};
use crate::daemon::{self, Daemon, ExpectedForeign, launchable, proc_info};
use crate::hypr::{self, Client};
use crate::state::{Desktop, Foreign, RestoreEntry, RestoreState, Session, SessionWindow, SessionWorkspace, State};

pub fn sessions_dir() -> PathBuf {
    dirs::state_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/state")).join("workspaced").join("sessions")
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) && name != "." && name != ".."
}

fn path_of(name: &str) -> Result<PathBuf> {
    if !valid_name(name) {
        bail!("недопустимое имя сессии {name:?}");
    }
    Ok(sessions_dir().join(format!("{name}.toml")))
}

/// Снимок текущего состояния.
pub fn snapshot(d: &Daemon) -> Result<Session> {
    let clients = d.clients()?;
    let st = d.state();
    let mut windows = Vec::new();
    for c in &clients {
        let desktop = match c.desktop() {
            Some(n) => n.to_string(),
            None if c.on_pool() => "pool".to_string(),
            None if c.on_hidden() => "hidden".to_string(),
            None => continue,
        };
        if c.app().is_some() {
            windows.push(instance_record(c, &clients, st, d.cfg(), &proc_info, desktop));
        } else if c.pid > 0 {
            let (cmd, cwd) = match st.foreign.get(&c.address) {
                Some(f) => (f.cmd.clone(), f.cwd.clone()),
                None => proc_info(c.pid),
            };
            if cmd.is_empty() {
                continue;
            }
            windows.push(SessionWindow { desktop, rect: c.rect(), cmd, cwd, ..SessionWindow::default() });
        }
    }
    windows.extend(carried_entries(st, &clients));
    // Назначения ячеек и дополнительные приложения сессии — сведения об одном
    // и том же workspace, поэтому в снимке они лежат рядом.
    let mut workspaces: BTreeMap<String, SessionWorkspace> = BTreeMap::new();
    for (w, cells) in &st.cells {
        workspaces.entry(w.clone()).or_default().cells = cells.clone();
    }
    for (w, apps) in &st.extra {
        workspaces.entry(w.clone()).or_default().extra_apps = apps.clone();
    }
    Ok(Session {
        saved: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        active_desktop: d.current_desktop(),
        desktops: st.desktops.iter().map(|(n, d)| (n.to_string(), d.clone())).collect(),
        workspaces,
        windows,
    })
}

/// Запись окна приложения в снимке — экземпляром (изменение session-instances,
/// решения D1, D2): номер из тега, workspace по тегам состава, прямоугольники
/// для workspace режима `stack` — нынешний, если окно стоит на столе, где
/// этот workspace активен, иначе запомненный для него. Командная строка
/// и каталог (`proc` — `proc_info` или замена в тесте) пишутся только у
/// экземпляра с собственным процессом, который не первый у своего приложения:
/// первый запускается командой приложения, остальные окна процесса
/// открывает сам процесс.
pub fn instance_record(c: &Client, clients: &[Client], st: &State, cfg: &Config, proc: &dyn Fn(i32) -> (Vec<String>, Option<String>), desktop: String) -> SessionWindow {
    let (app, num) = c.app_instance().unwrap_or_default();
    let workspaces = c.workspaces();
    let act = daemon::actives(st);
    let mut rects = BTreeMap::new();
    for w in &workspaces {
        if cfg.workspaces.get(w).map(|x| x.mode()) != Some(Mode::Stack) {
            continue;
        }
        let here = c.desktop().is_some_and(|n| act.get(&n) == Some(w));
        let r = if here { Some(c.rect()) } else { st.geom.get(w).and_then(|g| g.get(&c.address)).copied() };
        if let Some(r) = r {
            rects.insert(w.clone(), r);
        }
    }
    let own_process = c.pid > 0 && !clients.iter().any(|x| x.address != c.address && x.pid == c.pid);
    let first = clients.iter().filter_map(|x| x.app_instance()).filter(|(a, _)| *a == app).map(|(_, n)| n).min() == Some(num);
    let (cmd, cwd) = if own_process && !first { proc(c.pid) } else { (Vec::new(), None) };
    let cwd = if cmd.is_empty() { None } else { cwd };
    SessionWindow { app: Some(app), instance: Some(num), workspaces, desktop, rect: c.rect(), rects, cmd, cwd, ..SessionWindow::default() }
}

/// Записи плана восстановления, которые ещё ждут поднятия своего workspace
/// и окна которых пока нет: снимок переносит их как есть, иначе сохранение
/// до первого перехода на стол с ленивым поднятием потеряло бы экземпляры
/// этого workspace (решение D16).
pub fn carried_entries(st: &State, clients: &[Client]) -> Vec<SessionWindow> {
    st.restore
        .iter()
        .filter(|e| e.state == RestoreState::Waiting)
        .filter(|e| !clients.iter().any(|c| c.app_instance().is_some_and(|(a, n)| a == e.app && n == e.instance)))
        .map(|e| SessionWindow {
            app: Some(e.app.clone()),
            instance: Some(e.instance),
            workspaces: e.workspaces.clone(),
            desktop: e.desktop.clone(),
            rect: e.rect,
            rects: e.rects.clone(),
            cmd: e.cmd.clone(),
            cwd: e.cwd.clone(),
            ..SessionWindow::default()
        })
        .collect()
}

fn write(name: &str, s: &Session) -> Result<()> {
    let path = path_of(name)?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let text = toml::to_string_pretty(s).context("сериализация сессии")?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn read(name: &str) -> Result<Session> {
    let path = path_of(name)?;
    let text = std::fs::read_to_string(&path).with_context(|| format!("сессия {name}: нет файла {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("сессия {name}: ошибка разбора"))
}

pub fn save_default(d: &Daemon) -> Result<()> {
    write("default", &snapshot(d)?)
}

pub fn save_named(d: &Daemon, name: &str) -> Result<()> {
    if name == "default" {
        bail!("сессию default записывает команда сохранения сессии (save-session)");
    }
    write(name, &snapshot(d)?)
}

/// Список сессий с картой столов (для окна выбора и `session list --json`).
pub fn list(_d: &Daemon) -> Result<Vec<Value>> {
    let dir = sessions_dir();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else { return Ok(out) };
    let mut names: Vec<String> = rd.flatten().filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned())).filter(|n| valid_name(n) && !n.ends_with(".toml")).collect();
    names.sort();
    if let Some(i) = names.iter().position(|n| n == "default") {
        let n = names.remove(i);
        names.insert(0, n);
    }
    for name in names {
        let Ok(s) = read(&name) else { continue };
        let mut desktops = serde_json::Map::new();
        for (n, dsk) in &s.desktops {
            desktops.insert(n.clone(), json!({ "workspaces": dsk.workspaces, "active": dsk.active }));
        }
        let foreign = s.windows.iter().filter(|w| w.app.is_none()).count();
        out.push(json!({ "name": name, "saved": s.saved, "active_desktop": s.active_desktop, "desktops": desktops, "foreign_windows": foreign }));
    }
    Ok(out)
}

/// Восстановление при старте демона: списки целиком, окна лениво.
pub fn restore_default(d: &mut Daemon) -> Result<()> {
    let s = match read("default") {
        Ok(s) => s,
        Err(e) => {
            log::info!("сессия default не загружена, пустое состояние: {e:#}");
            adopt_live_foreign(d, &[], true);
            return Ok(());
        }
    };
    apply_lists(d, &s);
    // Ленивое поднятие: активные workspace остальных столов.
    let mut lazy = BTreeMap::new();
    for (n, dsk) in &s.desktops {
        if let (Ok(n), Some(ws)) = (n.parse::<u8>(), &dsk.active)
            && n != s.active_desktop
        {
            lazy.insert(n, ws.clone());
        }
    }
    crate::daemon::set_lazy(d, lazy);
    adopt_live_foreign(d, &s.windows, true);
    // План восстановления экземпляров (решения D8, D12): записи с живым окном
    // того же тега исполнены, остальные ждут поднятия своего workspace.
    let clients = d.clients()?;
    let plan = restore_plan(&s.windows, &clients, d.cfg());
    for why in &plan.dropped {
        log::info!("восстановление: {why}");
    }
    let st = d.state_mut();
    for (w, addr, r) in &plan.geom {
        st.geom.entry(w.clone()).or_default().insert(addr.clone(), *r);
    }
    let open = plan.entries.iter().filter(|e| e.open()).count();
    if !plan.entries.is_empty() {
        log::info!("восстановление: записей экземпляров {}, ждут восстановления {open}", plan.entries.len());
    }
    st.restore = plan.entries;
    st.restore_apps.clear();
    spawn_missing_foreign(d, &s.windows)?;
    if s.active_desktop != d.current_desktop() {
        d.hypr().dispatch(&hypr::d_focus_desktop(s.active_desktop))?;
        d.set_current(s.active_desktop);
    }
    if let Some(ws) = s.desktops.get(&s.active_desktop.to_string()).and_then(|x| x.active.clone()) {
        d.raise(&ws, Some(s.active_desktop))?;
    }
    d.spawn_free_entries();
    log::info!("сессия default восстановлена: активный стол {}", s.active_desktop);
    Ok(())
}

/// План восстановления при старте (решения D8, D12).
#[derive(Debug, Default)]
pub struct RestorePlan {
    pub entries: Vec<RestoreEntry>,
    /// workspace, адрес окна, прямоугольник: запомнить для режима `stack`.
    pub geom: Vec<(String, String, PxRect)>,
    /// Снятые записи — строки для журнала.
    pub dropped: Vec<String>,
}

/// План восстановления экземпляров по окнам снимка. Снимок прежнего формата
/// (без номеров экземпляров) плана не даёт (решение D12). Запись, для которой
/// живое окно с тем же тегом уже есть (перезапуск демона без перезагрузки),
/// исполнена: её прямоугольники переходят в память режима `stack` для тех
/// workspace, в которые окно входит сейчас, а состав окна не трогается
/// (решение D8). Запись без командной строки, окна которой нет, при живых
/// окнах приложения снимается: процесс приложения уже работает и своих окон
/// заново не откроет. Записи приложений, которых в эффективном конфиге нет,
/// в план не входят. Исполненные записи остаются в плане: по ним видно,
/// какой экземпляр в снимке первый.
pub fn restore_plan(windows: &[SessionWindow], clients: &[Client], cfg: &Config) -> RestorePlan {
    let mut plan = RestorePlan::default();
    for w in windows {
        let Some(mut e) = RestoreEntry::from_window(w) else { continue };
        if !cfg.apps.contains_key(&e.app) {
            plan.dropped.push(format!("{}#{}: приложения в конфиге нет, запись пропущена", e.app, e.instance));
            continue;
        }
        let live = clients.iter().find(|c| c.app_instance().is_some_and(|(a, n)| a == e.app && n == e.instance));
        if let Some(c) = live {
            for (ws, r) in &e.rects {
                if c.in_ws(ws) {
                    plan.geom.push((ws.clone(), c.address.clone(), *r));
                }
            }
            e.state = RestoreState::Done;
        } else if e.cmd.is_empty() && clients.iter().any(|c| c.app().as_deref() == Some(e.app.as_str())) {
            plan.dropped.push(format!("{}#{}: окна нет, а процесс приложения уже работает и окна заново не откроет — запись снята", e.app, e.instance));
            e.state = RestoreState::Done;
        }
        plan.entries.push(e);
    }
    plan
}

/// Списки столов, назначения ячеек и дополнительные приложения из снимка.
/// Workspace сверяются с файлом конфига, а не с эффективным: дополнительные
/// приложения прежнего состояния здесь как раз заменяются.
fn apply_lists(d: &mut Daemon, s: &Session) {
    let cfg_ws: Vec<String> = d.cfg_file().workspaces.keys().cloned().collect();
    let mut desktops: BTreeMap<u8, Desktop> = BTreeMap::new();
    for (n, dsk) in &s.desktops {
        if let Ok(n) = n.parse::<u8>() {
            let workspaces: Vec<String> = dsk.workspaces.iter().filter(|w| cfg_ws.contains(w)).cloned().collect();
            let active = dsk.active.clone().filter(|a| workspaces.contains(a));
            desktops.insert(n, Desktop { workspaces, active });
        }
    }
    dedup_desktops(&mut desktops);
    let st = d.state_mut();
    st.desktops = desktops;
    st.cells.clear();
    st.extra.clear();
    for (w, sw) in &s.workspaces {
        if !cfg_ws.contains(w) {
            continue;
        }
        // Пустая таблица ячеек в снимке ничего не значит: назначения тогда
        // собираются из конфига при первом обращении.
        if !sw.cells.is_empty() {
            st.cells.insert(w.clone(), sw.cells.clone());
        }
        if !sw.extra_apps.is_empty() {
            st.extra.insert(w.clone(), sw.extra_apps.clone());
        }
    }
    st.lazy.clear();
    d.rebuild_cfg();
}

/// Списки столов из снимка приводятся к правилу «workspace числится ровно
/// на одном столе» (решение D16): снимки прежних версий накопили повторы,
/// потому что перенос workspace на другой стол не убирал его из списка
/// прежнего. Столом workspace становится тот, где он активен, а если он
/// не активен нигде — первый по номеру стол, в списке которого он встретился.
/// Порядок оставшихся workspace в списке не меняется; стол, потерявший свой
/// активный workspace, остаётся без активного.
fn dedup_desktops(desktops: &mut BTreeMap<u8, Desktop>) {
    let mut owner: BTreeMap<String, u8> = BTreeMap::new();
    for (n, d) in desktops.iter() {
        if let Some(a) = &d.active {
            owner.entry(a.clone()).or_insert(*n);
        }
    }
    for (n, d) in desktops.iter() {
        for w in &d.workspaces {
            owner.entry(w.clone()).or_insert(*n);
        }
    }
    for (n, d) in desktops.iter_mut() {
        d.workspaces.retain(|w| owner.get(w) == Some(n));
        if d.active.as_deref().is_some_and(|a| !d.workspaces.iter().any(|w| w == a)) {
            d.active = None;
        }
    }
}

/// Постороннее окно снимка `w` запущено командой `cmd` в каталоге `cwd`.
/// Команда живого окна уже приведена к запускаемому виду (`proc_info`),
/// а снимок прежней версии демона мог хранить её как есть, поэтому команда
/// снимка сравнивается и в исходном, и в приведённом виде.
fn same_window(w: &SessionWindow, cmd: &[String], cwd: Option<&str>) -> bool {
    w.cwd.as_deref() == cwd && (w.cmd == cmd || launchable(&w.cmd, w.cwd.as_deref(), None).as_deref() == Some(cmd))
}

/// Живые окна без тега сопоставляются с посторонними окнами снимка по команде и каталогу.
/// Перед этим теги состава сверяются с эффективным конфигом, собранным
/// из снимка (`Daemon::sync_membership`).
/// Сверка начинается с приложений, которых в эффективном конфиге уже нет:
/// ожидания их окон снимаются, а с их окон снимается тег, и такое окно попадает
/// в сопоставление наравне с прочими посторонними. Список клиентов читается
/// после освобождения, поэтому теги в нём уже сняты.
/// `derive` — выдавать состав окнам без тегов состава (решение D2 шага 4);
/// загрузка снимка нового формата его не выдаёт: состав переиспользованным
/// окнам уже дал снимок (решение D9).
fn adopt_live_foreign(d: &mut Daemon, snapshot: &[SessionWindow], derive: bool) {
    d.drop_stale_pending();
    d.free_stale_tagged();
    // Состав workspace сверяется с записями сессии, а окна прежней версии
    // демона получают его по спискам столов (изменение shared-windows,
    // решение D2).
    d.sync_membership(derive);
    let Ok(clients) = d.clients() else { return };
    let mut used = vec![false; snapshot.len()];
    let st = d.state_mut();
    st.foreign.clear();
    for c in clients.iter().filter(|c| c.app().is_none() && c.pid > 0) {
        let (cmd, cwd) = proc_info(c.pid);
        let hit = snapshot.iter().enumerate().find(|(i, w)| !used[*i] && w.app.is_none() && same_window(w, &cmd, cwd.as_deref()));
        match hit {
            Some((i, w)) => {
                used[i] = true;
                st.foreign.insert(c.address.clone(), Foreign { rect: w.rect, cmd, cwd });
            }
            None => {
                st.foreign.insert(c.address.clone(), Foreign { rect: c.rect(), cmd, cwd });
            }
        }
    }
}

/// Посторонние окна снимка, которых нет в системе, запускаются сразу на свои места.
fn spawn_missing_foreign(d: &mut Daemon, snapshot: &[SessionWindow]) -> Result<()> {
    let live: Vec<(Vec<String>, Option<String>)> = d.state().foreign.values().map(|f| (f.cmd.clone(), f.cwd.clone())).collect();
    let mut seen: Vec<(Vec<String>, Option<String>)> = Vec::new();
    for w in snapshot.iter().filter(|w| w.app.is_none() && w.desktop != "hidden") {
        // Снимок, записанный прежней версией демона, может хранить команду,
        // которую не запустить (`./steamwebhelper`, строку Chromium целиком):
        // она приводится к запускаемому виду так же, как при сохранении,
        // и сверяется с живыми окнами уже в этом виде.
        let Some(cmd) = launchable(&w.cmd, w.cwd.as_deref(), None) else {
            log::warn!("восстановление окна {:?}: исполняемый файл не найден, окно пропущено", w.cmd);
            continue;
        };
        let key = (cmd.clone(), w.cwd.clone());
        let live_count = live.iter().filter(|k| **k == key).count();
        let seen_count = seen.iter().filter(|k| **k == key).count();
        seen.push(key.clone());
        if seen_count < live_count {
            continue;
        }
        let desktop = w.desktop.parse::<u8>().ok();
        // Ожидание заводится по pid запущенного процесса: оно живёт до окна
        // или до выхода процесса, сроком не ограничено.
        match d.spawn_foreign(&cmd, w.cwd.as_deref()) {
            Ok(pid) => d.expect_foreign(ExpectedForeign { cmd, cwd: w.cwd.clone(), desktop, rect: w.rect, pid }),
            Err(e) => log::warn!("восстановление окна {:?}: {e:#}", w.cmd),
        }
    }
    Ok(())
}

/// Окна снимка, для которых нужно запустить приложение: по одному запуску
/// на приложение, даже если в снимке у него несколько окон. Приложение
/// с живым окном (`live`) пропускается; из снимка возвращается один экземпляр,
/// остальные окна открывает сам пользователь.
fn apps_to_start<'a>(windows: &'a [SessionWindow], live: &dyn Fn(&str) -> bool) -> Vec<&'a SessionWindow> {
    let mut started: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for w in windows {
        let Some(app) = w.app.as_deref() else { continue };
        if started.contains(&app) || live(app) {
            continue;
        }
        started.push(app);
        out.push(w);
    }
    out
}

/// Сверка окон приложения с его записями в загружаемом снимке (решение D9):
/// живые окна по возрастанию номера против записей по возрастанию номера.
/// `live` — адрес и номер экземпляра живых окон, `entries` — номер записи
/// и её индекс в снимке. Возвращает пары «адрес — индекс записи», адреса
/// окон сверх числа записей и индексы записей, которым окна не хватило.
#[allow(clippy::type_complexity)]
pub fn load_pairs(live: &[(String, u32)], entries: &[(u32, usize)]) -> (Vec<(String, usize)>, Vec<String>, Vec<usize>) {
    let mut live = live.to_vec();
    live.sort_by_key(|(_, n)| *n);
    let mut entries = entries.to_vec();
    entries.sort_by_key(|(n, _)| *n);
    let pairs: Vec<(String, usize)> = live.iter().zip(entries.iter()).map(|((a, _), (_, i))| (a.clone(), *i)).collect();
    let extra: Vec<String> = live.iter().skip(entries.len()).map(|(a, _)| a.clone()).collect();
    let missing: Vec<usize> = entries.iter().skip(live.len()).map(|(_, i)| *i).collect();
    (pairs, extra, missing)
}

/// Загрузка сессии посреди работы: сверка, закрытие лишнего, запуск недостающего.
pub fn load(d: &mut Daemon, name: &str) -> Result<()> {
    let s = read(name)?;
    save_default(d)?;
    // Ожидания прежнего восстановления снимаются до сверки (решение D10).
    d.end_restore_wait(None, true, "загрузка сессии");
    let new_format = s.windows.iter().any(|w| w.instance.is_some());
    let clients = d.clients()?;
    // Лишние окна закрываются штатно.
    let mut close = Vec::new();
    let mut foreign_used = vec![false; s.windows.len()];
    // Окна приложений снимка нового формата сверяются по экземплярам.
    let mut reuse: Vec<(String, usize)> = Vec::new();
    let mut missing: Vec<usize> = Vec::new();
    if new_format {
        let mut live: BTreeMap<String, Vec<(String, u32)>> = BTreeMap::new();
        for c in &clients {
            if let Some((app, n)) = c.app_instance() {
                live.entry(app).or_default().push((c.address.clone(), n));
            }
        }
        let mut recs: BTreeMap<String, Vec<(u32, usize)>> = BTreeMap::new();
        for (i, w) in s.windows.iter().enumerate() {
            if let (Some(app), Some(n)) = (&w.app, w.instance) {
                recs.entry(app.clone()).or_default().push((n, i));
            }
        }
        let mut apps: Vec<String> = live.keys().chain(recs.keys()).cloned().collect();
        apps.sort();
        apps.dedup();
        for app in apps {
            let (pairs, extra, lack) = load_pairs(live.get(&app).map(Vec::as_slice).unwrap_or(&[]), recs.get(&app).map(Vec::as_slice).unwrap_or(&[]));
            reuse.extend(pairs);
            missing.extend(lack);
            for a in extra {
                log::info!("загрузка сессии {name}: окно {a} приложения {app} лишнее, закрывается");
                close.push(hypr::d_close(&a));
            }
        }
    }
    for c in &clients {
        match c.app() {
            Some(app) => {
                if !new_format && !s.windows.iter().any(|w| w.app.as_deref() == Some(&app)) {
                    close.push(hypr::d_close(&c.address));
                }
            }
            None => {
                if c.pid <= 0 {
                    continue;
                }
                let (cmd, cwd) = proc_info(c.pid);
                match s.windows.iter().enumerate().find(|(i, w)| !foreign_used[*i] && w.app.is_none() && same_window(w, &cmd, cwd.as_deref())) {
                    Some((i, _)) => foreign_used[i] = true,
                    None => close.push(hypr::d_close(&c.address)),
                }
            }
        }
    }
    d.hypr().dispatch_all(&close)?;
    apply_lists(d, &s);
    if new_format {
        // Переиспользованные окна получают состав и место из снимка до сверки
        // тегов с эффективным конфигом.
        let mut done: Vec<RestoreEntry> = Vec::new();
        for (addr, i) in &reuse {
            let Some(e) = RestoreEntry::from_window(&s.windows[*i]) else { continue };
            if let Some(c) = clients.iter().find(|c| c.address == *addr) {
                d.reuse_window(c, &e);
            }
            done.push(RestoreEntry { state: RestoreState::Done, ..e });
        }
        let mut plan: Vec<RestoreEntry> = missing.iter().filter_map(|i| RestoreEntry::from_window(&s.windows[*i])).filter(|e| d.cfg().apps.contains_key(&e.app)).collect();
        // Запись без командной строки при живых окнах приложения не ждёт:
        // процесс уже работает и своих окон заново не откроет (как при старте).
        for e in plan.iter_mut() {
            if e.cmd.is_empty() && done.iter().any(|x| x.app == e.app) {
                log::info!("загрузка сессии {name}: {}#{} — окна нет, процесс приложения уже работает, запись снята", e.app, e.instance);
                e.state = RestoreState::Done;
            }
        }
        plan.extend(done);
        d.state_mut().restore = plan;
    }
    adopt_live_foreign(d, &s.windows, !new_format);
    spawn_missing_foreign(d, &s.windows)?;
    if new_format {
        // Первый экземпляр приложения без живых окон запускается командой
        // приложения, как прежде; остальные записи ждут своих workspace.
        let firsts = first_to_start(&d.state().restore);
        for e in firsts {
            let (ws, desktop) = entry_target(&s, &e);
            d.state_mut().restore_apps.insert(e.app.clone());
            if let Err(err) = d.spawn_for_session(&e.app, ws.as_deref(), desktop) {
                log::warn!("загрузка сессии {name}: {err:#}; загрузка продолжается без этого приложения");
            }
        }
    } else {
        // Снимок прежнего формата: по одному запуску на приложение без живых окон.
        let starts: Vec<(String, String)> = apps_to_start(&s.windows, &|app| !crate::daemon::app_windows(d.cfg(), &clients, app).is_empty())
            .into_iter()
            .map(|w| (w.app.clone().unwrap_or_default(), w.desktop.clone()))
            .collect();
        for (app, desk) in starts {
            let desktop = desk.parse::<u8>().ok();
            let ws = desktop
                .and_then(|n| s.desktops.get(&n.to_string()))
                .and_then(|x| x.active.clone())
                .filter(|ws| d.cfg().workspaces.get(ws).is_some_and(|x| x.apps.contains_key(&app)))
                .or_else(|| d.cfg().workspaces.iter().find(|(_, x)| x.apps.contains_key(&app)).map(|(n, _)| n.clone()));
            // Сбой запуска одного приложения загрузку не обрывает (изменение
            // classless-windows-stay-free, решение D2).
            if let Err(e) = d.spawn_for_session(&app, ws.as_deref(), desktop) {
                log::warn!("загрузка сессии {name}: {e:#}; загрузка продолжается без этого приложения");
            }
        }
    }
    // Активные workspace поднимаются по столам, активный стол последним.
    let mut order: Vec<(u8, String)> = s.desktops.iter().filter_map(|(n, x)| Some((n.parse::<u8>().ok()?, x.active.clone()?))).collect();
    order.sort();
    if let Some(i) = order.iter().position(|(n, _)| *n == s.active_desktop) {
        let it = order.remove(i);
        order.push(it);
    }
    for (n, ws) in order {
        if let Err(e) = d.raise(&ws, Some(n)) {
            log::warn!("загрузка сессии {name}: workspace {ws} на столе {n} не поднят: {e:#}");
        }
    }
    if s.active_desktop != d.current_desktop() {
        d.hypr().dispatch(&hypr::d_focus_desktop(s.active_desktop))?;
        d.set_current(s.active_desktop);
    }
    d.spawn_free_entries();
    d.broadcast();
    log::info!("сессия {name} загружена");
    Ok(())
}

/// Первые экземпляры приложений, которые пора запустить командой
/// приложения: запись с наименьшим номером среди записей приложения ждёт
/// запуска, у неё нет командной строки и есть workspace, а исполненных
/// записей (живых окон) у приложения нет.
pub fn first_to_start(entries: &[RestoreEntry]) -> Vec<RestoreEntry> {
    let mut out: Vec<RestoreEntry> = Vec::new();
    for e in entries {
        let first = entries.iter().filter(|x| x.app == e.app).map(|x| x.instance).min() == Some(e.instance);
        let live = entries.iter().any(|x| x.app == e.app && x.state == RestoreState::Done);
        if first && !live && e.state == RestoreState::Waiting && e.cmd.is_empty() && !e.workspaces.is_empty() && !out.iter().any(|x| x.app == e.app) {
            out.push(e.clone());
        }
    }
    out
}

/// Workspace и стол, для которых запускается приложение записи при загрузке:
/// первый workspace записи, активный на каком-либо столе снимка, и этот стол;
/// иначе первый workspace записи и парковка.
fn entry_target(s: &Session, e: &RestoreEntry) -> (Option<String>, Option<u8>) {
    for w in &e.workspaces {
        if let Some((n, _)) = s.desktops.iter().find(|(_, x)| x.active.as_deref() == Some(w.as_str())) {
            return (Some(w.clone()), n.parse::<u8>().ok());
        }
    }
    (e.workspaces.first().cloned(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PxRect;

    fn win(app: Option<&str>, desktop: &str) -> SessionWindow {
        SessionWindow {
            app: app.map(String::from),
            desktop: desktop.to_string(),
            rect: PxRect { x: 0, y: 0, w: 10, h: 10 },
            ..SessionWindow::default()
        }
    }

    #[test]
    fn snapshot_with_two_windows_starts_one_instance() {
        // У chromium в снимке два окна, живых окон нет: запускается один
        // экземпляр. У neovide окно живо, запускать нечего.
        let windows = vec![win(Some("chromium"), "1"), win(Some("chromium"), "1"), win(Some("neovide"), "1"), win(None, "3")];
        let picked = apps_to_start(&windows, &|app| app == "neovide");
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].app.as_deref(), Some("chromium"));
    }

    fn desk(ws: &[&str], active: Option<&str>) -> Desktop {
        Desktop { workspaces: ws.iter().map(|w| w.to_string()).collect(), active: active.map(String::from) }
    }

    #[test]
    fn restored_lists_keep_workspace_on_one_desktop() {
        // Снимок, накопленный прежним демоном: surf числится на четырёх
        // столах, work — на трёх.
        let mut d: BTreeMap<u8, Desktop> = BTreeMap::new();
        d.insert(1, desk(&["work", "surf"], Some("work")));
        d.insert(2, desk(&["surf", "work"], None));
        d.insert(3, desk(&["work", "surf"], None));
        d.insert(6, desk(&["surf"], Some("surf")));
        dedup_desktops(&mut d);
        // Каждый workspace остаётся на столе, где он активен: work на первом,
        // surf на шестом, хотя в списке первого стола он стоял раньше.
        assert_eq!(d[&1].workspaces, vec!["work".to_string()]);
        assert_eq!(d[&1].active.as_deref(), Some("work"));
        assert!(d[&2].workspaces.is_empty());
        assert!(d[&3].workspaces.is_empty());
        assert_eq!(d[&6].workspaces, vec!["surf".to_string()]);
        assert_eq!(d[&6].active.as_deref(), Some("surf"));

        // Workspace не активен нигде: он остаётся на первом по номеру столе,
        // в списке которого встретился, а порядок списка не меняется.
        let mut d: BTreeMap<u8, Desktop> = BTreeMap::new();
        d.insert(1, desk(&["chat", "work"], Some("work")));
        d.insert(2, desk(&["chat"], None));
        // Снимок, где один workspace записан активным на двух столах: он
        // достаётся первому по номеру, а второй остаётся без активного.
        d.insert(3, desk(&["work"], Some("work")));
        dedup_desktops(&mut d);
        assert_eq!(d[&1].workspaces, vec!["chat".to_string(), "work".to_string()]);
        assert_eq!(d[&1].active.as_deref(), Some("work"));
        assert!(d[&2].workspaces.is_empty());
        assert!(d[&3].workspaces.is_empty());
        assert_eq!(d[&3].active, None);
    }

    const CFG: &str = r#"
[templates.thirds]
main = "center"
[templates.thirds.cells]
left   = { x = -805, y = 10, w = 1920, h = 2140 }
center = { x = 1125, y = 10, w = 1920, h = 2140 }
right  = { x = 3055, y = 10, w = 1920, h = 2140 }

[apps.chromium]
cmd = "chromium"
class = "(?i)^chromium$"

[apps.neovide]
cmd = "neovide"
class = "^neovide$"

[apps.chrome-ai]
cmd = "google-chrome"
class = "^google-chrome-ai$"

[workspaces.work]
template = "thirds"
apps = { chromium = "left", neovide = "right", chrome-ai = "center" }

[workspaces.surf]
template = "thirds"
mode = "stack"
apps = { chrome-ai = "right" }
"#;

    fn client(addr: &str, class: &str, ws: &str, tags: &[&str], pid: i32, r: PxRect) -> Client {
        let mut c = hypr::test_client(addr, class, "", ws, tags);
        c.pid = pid;
        c.at = (r.x, r.y);
        c.size = (r.w, r.h);
        c
    }

    const R1: PxRect = PxRect { x: -805, y: 10, w: 1920, h: 2140 };
    const R2: PxRect = PxRect { x: 3055, y: 10, w: 1920, h: 2140 };
    const RS: PxRect = PxRect { x: 1700, y: 60, w: 1920, h: 2140 };

    #[test]
    fn snapshot_records_instances() {
        let cfg = Config::parse(CFG).unwrap();
        let mut st = State::default();
        st.desktops.insert(1, desk(&["work"], Some("work")));
        st.desktops.insert(2, desk(&["surf"], Some("surf")));
        // Для surf запомнен прямоугольник окна chrome-ai, стоящего сейчас на столе 1.
        st.geom.entry("surf".into()).or_default().insert("0xai".into(), RS);
        let clients = vec![
            client("0xc1", "chromium", "1", &["app:chromium#1", "ws:work"], 100, R1),
            client("0xc2", "chromium", "1", &["app:chromium#2", "ws:work", "ws:surf"], 100, R1),
            client("0xn1", "neovide", "1", &["app:neovide#1", "ws:work"], 200, R2),
            client("0xn2", "neovide", "1", &["app:neovide#2", "ws:work"], 300, R2),
            client("0xai", "google-chrome-ai", "1", &["app:chrome-ai#1", "ws:surf", "ws:work"], 400, R2),
            client("0xa2", "google-chrome-ai", "2", &["app:chrome-ai#2", "ws:surf"], 401, RS),
        ];
        let proc = |pid: i32| -> (Vec<String>, Option<String>) {
            match pid {
                300 => (vec!["neovide".into(), "notes.md".into()], Some("/home/mne/notes".into())),
                _ => (vec!["x".into()], Some("/".into())),
            }
        };
        let rec: Vec<SessionWindow> = clients.iter().map(|c| instance_record(c, &clients, &st, &cfg, &proc, c.desktop().unwrap().to_string())).collect();
        // Два окна одного процесса: номера и состав есть, команды нет.
        assert_eq!((rec[0].instance, rec[0].workspaces.clone(), rec[0].cmd.is_empty()), (Some(1), vec!["work".to_string()], true));
        assert_eq!((rec[1].instance, rec[1].workspaces.clone(), rec[1].cmd.is_empty()), (Some(2), vec!["surf".to_string(), "work".to_string()], true));
        // Первый экземпляр neovide без команды, второй своего процесса — с командой и каталогом.
        assert!(rec[2].cmd.is_empty() && rec[2].cwd.is_none());
        assert_eq!(rec[3].cmd, vec!["neovide".to_string(), "notes.md".to_string()]);
        assert_eq!(rec[3].cwd.as_deref(), Some("/home/mne/notes"));
        // rects только у workspace режима stack: surf — запомненный, work нет.
        assert_eq!(rec[4].rects, BTreeMap::from([("surf".to_string(), RS)]));
        assert_eq!(rec[4].rect, R2);
        assert!(rec[0].rects.is_empty() && rec[1].rects.is_empty());
        // Окно на столе, где surf активен, — нынешний прямоугольник.
        assert_eq!(rec[5].rects, BTreeMap::from([("surf".to_string(), RS)]));
        assert_eq!(rec[5].cmd, vec!["x".to_string()]);
    }

    #[test]
    fn restore_plan_skips_live_instances() {
        let cfg = Config::parse(CFG).unwrap();
        let w = |app: &str, n: u32, ws: &[&str], cmd: &[&str]| SessionWindow {
            app: Some(app.into()),
            instance: Some(n),
            workspaces: ws.iter().map(|x| x.to_string()).collect(),
            desktop: "1".into(),
            rect: R1,
            cmd: cmd.iter().map(|x| x.to_string()).collect(),
            ..SessionWindow::default()
        };
        let mut ai = w("chrome-ai", 1, &["surf", "work"], &[]);
        ai.rects.insert("surf".into(), RS);
        ai.rects.insert("gone".into(), RS);
        let windows = vec![w("chromium", 1, &["work"], &[]), w("chromium", 2, &["work"], &[]), w("neovide", 1, &["work"], &[]), w("neovide", 2, &["work"], &["neovide", "notes.md"]), ai, w("nosuch", 1, &["work"], &[])];
        // Перезапуск демона: chromium#1, neovide#1 и chrome-ai живы, второго
        // окна chromium и второго neovide нет.
        let clients = vec![
            client("0xc1", "chromium", "1", &["app:chromium#1", "ws:work"], 100, R1),
            client("0xn1", "neovide", "1", &["app:neovide#1", "ws:work"], 200, R2),
            client("0xai", "google-chrome-ai", "1", &["app:chrome-ai#1", "ws:surf"], 400, R2),
        ];
        let plan = restore_plan(&windows, &clients, &cfg);
        let st: Vec<(String, u32, RestoreState)> = plan.entries.iter().map(|e| (e.app.clone(), e.instance, e.state)).collect();
        assert_eq!(
            st,
            vec![
                ("chromium".into(), 1, RestoreState::Done),
                // Без команды при живых окнах приложения — снята.
                ("chromium".into(), 2, RestoreState::Done),
                ("neovide".into(), 1, RestoreState::Done),
                // С командой — ждёт запуска при поднятии work.
                ("neovide".into(), 2, RestoreState::Waiting),
                ("chrome-ai".into(), 1, RestoreState::Done),
            ]
        );
        // Прямоугольник surf переходит в память stack: окно входит в surf.
        assert_eq!(plan.geom, vec![("surf".to_string(), "0xai".to_string(), RS)]);
        assert_eq!(plan.dropped.len(), 2, "{:?}", plan.dropped);
        // После перезагрузки живых окон нет: все записи ждут.
        let plan = restore_plan(&windows, &[], &cfg);
        assert!(plan.entries.iter().all(|e| e.state == RestoreState::Waiting));
        // Первые экземпляры для запуска при загрузке.
        let firsts: Vec<String> = first_to_start(&plan.entries).iter().map(|e| format!("{}#{}", e.app, e.instance)).collect();
        assert_eq!(firsts, vec!["chromium#1", "neovide#1", "chrome-ai#1"]);
    }

    #[test]
    fn restore_plan_old_format_is_empty() {
        let cfg = Config::parse(CFG).unwrap();
        let windows = vec![win(Some("chromium"), "1"), win(Some("chromium"), "1"), win(Some("neovide"), "1"), win(None, "3")];
        let plan = restore_plan(&windows, &[], &cfg);
        assert!(plan.entries.is_empty() && plan.geom.is_empty());
    }

    #[test]
    fn load_pairs_by_instance_order() {
        let live = vec![("0xc3".to_string(), 3), ("0xc1".to_string(), 1), ("0xc2".to_string(), 2)];
        // Три живых окна против двух записей: 1 и 2 переиспользуются, 3 лишнее.
        let (pairs, extra, missing) = load_pairs(&live, &[(2, 11), (1, 10)]);
        assert_eq!(pairs, vec![("0xc1".to_string(), 10), ("0xc2".to_string(), 11)]);
        assert_eq!(extra, vec!["0xc3".to_string()]);
        assert!(missing.is_empty());
        // Одно живое против двух: запись номер 2 восстанавливается.
        let (pairs, extra, missing) = load_pairs(&[("0xn1".to_string(), 1)], &[(1, 5), (2, 6)]);
        assert_eq!(pairs, vec![("0xn1".to_string(), 5)]);
        assert!(extra.is_empty());
        assert_eq!(missing, vec![6]);
        // Приложения нет в снимке: все окна лишние.
        let (_, extra, _) = load_pairs(&[("0xt".to_string(), 1)], &[]);
        assert_eq!(extra, vec!["0xt".to_string()]);
    }

    #[test]
    fn waiting_entries_carry_into_snapshot() {
        let mut st = State::default();
        let e = |app: &str, n: u32, state: RestoreState| RestoreEntry { app: app.into(), instance: n, workspaces: vec!["surf".into()], desktop: "pool".into(), rect: RS, rects: BTreeMap::from([("surf".to_string(), RS)]), cmd: vec![], cwd: None, state };
        st.restore = vec![e("chrome-ai", 1, RestoreState::Waiting), e("chromium", 1, RestoreState::Done), e("neovide", 2, RestoreState::Launched(5)), e("chrome", 1, RestoreState::Waiting)];
        // Окно chrome#1 уже живо: его пишет сам снимок, запись не переносится.
        let clients = vec![client("0x1", "google-chrome", "2", &["app:chrome#1", "ws:surf"], 9, RS)];
        let carried = carried_entries(&st, &clients);
        assert_eq!(carried.len(), 1);
        assert_eq!((carried[0].app.as_deref(), carried[0].instance, carried[0].rects.get("surf")), (Some("chrome-ai"), Some(1), Some(&RS)));
    }
}
