//! Сессии: снимок состояния, `default` по событию, именованные по команде,
//! восстановление при старте (лениво) и загрузка со сверкой (спецификация ws-sessions).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::daemon::{Daemon, ExpectedForeign, proc_info};
use crate::hypr;
use crate::state::{Desktop, Foreign, Session, SessionWindow, SessionWorkspace};

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
        if let Some(app) = c.app() {
            windows.push(SessionWindow { app: Some(app), workspace: None, desktop, rect: c.rect(), cmd: vec![], cwd: None });
        } else if c.pid > 0 {
            let (cmd, cwd) = match st.foreign.get(&c.address) {
                Some(f) => (f.cmd.clone(), f.cwd.clone()),
                None => proc_info(c.pid),
            };
            if cmd.is_empty() {
                continue;
            }
            windows.push(SessionWindow { app: None, workspace: None, desktop, rect: c.rect(), cmd, cwd });
        }
    }
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
        bail!("сессия default пишется демоном автоматически");
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
            adopt_live_foreign(d, &[]);
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
    adopt_live_foreign(d, &s.windows);
    spawn_missing_foreign(d, &s.windows)?;
    if s.active_desktop != d.current_desktop() {
        d.hypr().dispatch(&hypr::d_focus_desktop(s.active_desktop))?;
        d.set_current(s.active_desktop);
    }
    if let Some(ws) = s.desktops.get(&s.active_desktop.to_string()).and_then(|x| x.active.clone()) {
        d.raise(&ws, Some(s.active_desktop))?;
    }
    log::info!("сессия default восстановлена: активный стол {}", s.active_desktop);
    Ok(())
}

/// Списки столов, назначения ячеек и дополнительные приложения из снимка.
/// Workspace сверяются с файлом конфига, а не с эффективным: дополнительные
/// приложения прежнего состояния здесь как раз заменяются.
fn apply_lists(d: &mut Daemon, s: &Session) {
    let cfg_ws: Vec<String> = d.cfg_file().workspaces.keys().cloned().collect();
    let st = d.state_mut();
    st.desktops.clear();
    for (n, dsk) in &s.desktops {
        if let Ok(n) = n.parse::<u8>() {
            let workspaces: Vec<String> = dsk.workspaces.iter().filter(|w| cfg_ws.contains(w)).cloned().collect();
            let active = dsk.active.clone().filter(|a| workspaces.contains(a));
            st.desktops.insert(n, Desktop { workspaces, active });
        }
    }
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

/// Живые окна без тега сопоставляются с посторонними окнами снимка по команде и каталогу.
/// Сверка начинается с приложений, которых в эффективном конфиге уже нет:
/// ожидания их окон снимаются, а с их окон снимается тег, и такое окно попадает
/// в сопоставление наравне с прочими посторонними. Список клиентов читается
/// после освобождения, поэтому теги в нём уже сняты.
fn adopt_live_foreign(d: &mut Daemon, snapshot: &[SessionWindow]) {
    d.drop_stale_pending();
    d.free_stale_tagged();
    let Ok(clients) = d.clients() else { return };
    let mut used = vec![false; snapshot.len()];
    let st = d.state_mut();
    st.foreign.clear();
    for c in clients.iter().filter(|c| c.app().is_none() && c.pid > 0) {
        let (cmd, cwd) = proc_info(c.pid);
        let hit = snapshot.iter().enumerate().find(|(i, w)| !used[*i] && w.app.is_none() && w.cmd == cmd && w.cwd == cwd);
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
        let key = (w.cmd.clone(), w.cwd.clone());
        let live_count = live.iter().filter(|k| **k == key).count();
        let seen_count = seen.iter().filter(|k| **k == key).count();
        seen.push(key.clone());
        if seen_count < live_count {
            continue;
        }
        let desktop = w.desktop.parse::<u8>().ok();
        // Ожидание заводится по pid запущенного процесса: оно живёт до окна
        // или до выхода процесса, сроком не ограничено.
        match d.spawn_foreign(&w.cmd, w.cwd.as_deref()) {
            Ok(pid) => d.expect_foreign(ExpectedForeign { cmd: w.cmd.clone(), cwd: w.cwd.clone(), desktop, rect: w.rect, pid }),
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

/// Загрузка сессии посреди работы: сверка, закрытие лишнего, запуск недостающего.
pub fn load(d: &mut Daemon, name: &str) -> Result<()> {
    let s = read(name)?;
    save_default(d)?;
    let clients = d.clients()?;
    // Лишние окна закрываются штатно.
    let mut close = Vec::new();
    let mut foreign_used = vec![false; s.windows.len()];
    for c in &clients {
        match c.app() {
            Some(app) => {
                if !s.windows.iter().any(|w| w.app.as_deref() == Some(&app)) {
                    close.push(hypr::d_close(&c.address));
                }
            }
            None => {
                if c.pid <= 0 {
                    continue;
                }
                let (cmd, cwd) = proc_info(c.pid);
                match s.windows.iter().enumerate().find(|(i, w)| !foreign_used[*i] && w.app.is_none() && w.cmd == cmd && w.cwd == cwd) {
                    Some((i, _)) => foreign_used[i] = true,
                    None => close.push(hypr::d_close(&c.address)),
                }
            }
        }
    }
    d.hypr().dispatch_all(&close)?;
    apply_lists(d, &s);
    adopt_live_foreign(d, &s.windows);
    spawn_missing_foreign(d, &s.windows)?;
    // Окна приложений: недостающие запускаются в ячейку активного workspace или на парковку.
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
        d.spawn_for_session(&app, ws.as_deref(), desktop)?;
    }
    // Активные workspace поднимаются по столам, активный стол последним.
    let mut order: Vec<(u8, String)> = s.desktops.iter().filter_map(|(n, x)| Some((n.parse::<u8>().ok()?, x.active.clone()?))).collect();
    order.sort();
    if let Some(i) = order.iter().position(|(n, _)| *n == s.active_desktop) {
        let it = order.remove(i);
        order.push(it);
    }
    for (n, ws) in order {
        d.raise(&ws, Some(n))?;
    }
    if s.active_desktop != d.current_desktop() {
        d.hypr().dispatch(&hypr::d_focus_desktop(s.active_desktop))?;
        d.set_current(s.active_desktop);
    }
    d.broadcast();
    log::info!("сессия {name} загружена");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PxRect;

    fn win(app: Option<&str>, desktop: &str) -> SessionWindow {
        SessionWindow {
            app: app.map(String::from),
            workspace: None,
            desktop: desktop.to_string(),
            rect: PxRect { x: 0, y: 0, w: 10, h: 10 },
            cmd: vec![],
            cwd: None,
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
}
