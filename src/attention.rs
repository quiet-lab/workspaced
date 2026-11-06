//! Сторож внимания (изменение ws-attention): когда агент в herdr ждёт ответа
//! (панель в статусе `blocked`), а пользователь занят другим окном, демон
//! ставит на паузу играющие плееры MPRIS, останавливает процесс полноэкранного
//! окна (игру) и переводит фокус на окно herdr. Когда на вопросы ответили,
//! остановленные процессы продолжают работу, плееры возобновляются, а фокус
//! возвращается на прежнее окно, если пользователь сам из herdr не ушёл.
//!
//! Источник событий — сервер herdr: подписка `events.subscribe` на статус
//! каждой панели и на появление и закрытие панелей в одном соединении.
//! Сервер недоступен — поток ждёт появления сокета по inotify; таймеров
//! и опроса нет (запись `0007-no-timers.md` репозитория chezmoi).
//!
//! Решения о действиях принимает [`Watch`] без Hyprland и herdr: действия
//! выполняет реализация [`Effects`], в тестах — подставная.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::{Value, json};

use crate::config::Attention;

/// Окно с фокусом в момент вопроса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Win {
    pub addr: String,
    pub class: String,
    pub pid: i32,
    /// Полноэкранный режим композитора `fullscreen` (не maximized).
    pub fullscreen: bool,
    /// Окно приложения сторожа (herdr).
    pub herdr: bool,
}

/// Входные события сторожа.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Панели в статусе `blocked` по ответу `pane.list`. `fresh` — первый
    /// снимок после подключения: он задаёт состояние и сигнала не даёт
    /// (вопрос, который уже висел до старта демона или до подмены сервера,
    /// фокус не забирает). Снимок переподписки внутри соединения сравнивается
    /// с известным состоянием, и новые панели в `blocked` дают сигнал.
    Snapshot { blocked: BTreeSet<String>, fresh: bool },
    /// Статус панели изменился: `blocked` — панель ждёт ответа.
    Status { pane: String, blocked: bool },
    /// Связь с сервером herdr потеряна: состояние панелей неизвестно.
    Disconnected,
    /// Фокус перешёл на окно `addr` (событие композитора).
    Focus { addr: String },
}

/// Действия сторожа на станции.
pub trait Effects {
    /// Окно с фокусом; `None` — фокуса нет.
    fn active(&mut self) -> Option<Win>;
    /// Плееры MPRIS в состоянии `Playing`.
    fn playing(&mut self) -> Vec<String>;
    fn pause(&mut self, players: &[String]);
    fn play(&mut self, players: &[String]);
    /// Остановить процесс `pid` и его потомков; возвращает остановленные pid.
    /// Список записывается в файл состояния до первого `SIGSTOP`.
    fn freeze(&mut self, pid: i32) -> Vec<i32>;
    /// `SIGCONT` перечисленным процессам и очистка файла состояния.
    fn thaw(&mut self, pids: &[i32]);
    /// Фокус на окно приложения сторожа (как по его клавише).
    fn focus_app(&mut self) -> Result<()>;
    fn window_exists(&mut self, addr: &str) -> bool;
    fn focus(&mut self, addr: &str) -> Result<()>;
}

/// Что сделано по сигналу внимания и что отменяется при отбое.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Engaged {
    /// Панели, вошедшие в `blocked` с момента сигнала: отбой — когда их
    /// не осталось. Панель, давно забытая в `blocked` до сигнала, отбой
    /// не задерживает, иначе игра оставалась бы остановленной без срока.
    panes: BTreeSet<String>,
    prev: Option<Win>,
    players: Vec<String>,
    frozen: Vec<i32>,
}

/// Состояние сторожа: панели в `blocked` и действия последнего сигнала.
#[derive(Debug, Default)]
pub struct Watch {
    blocked: BTreeSet<String>,
    engaged: Option<Engaged>,
}

impl Watch {
    pub fn feed(&mut self, input: Input, cfg: &Attention, fx: &mut dyn Effects) {
        match input {
            Input::Snapshot { blocked, fresh } => {
                let entered: Vec<String> = if fresh { Vec::new() } else { blocked.difference(&self.blocked).cloned().collect() };
                self.blocked = blocked;
                if let Some(e) = &mut self.engaged {
                    e.panes.retain(|p| self.blocked.contains(p));
                }
                self.settle(cfg, fx);
                for p in entered {
                    self.entered(p, cfg, fx);
                }
            }
            Input::Status { pane, blocked } => {
                if blocked {
                    if self.blocked.insert(pane.clone()) {
                        self.entered(pane, cfg, fx);
                    }
                } else {
                    self.blocked.remove(&pane);
                    if let Some(e) = &mut self.engaged {
                        e.panes.remove(&pane);
                    }
                    self.settle(cfg, fx);
                }
            }
            Input::Disconnected => {
                self.blocked.clear();
                if let Some(e) = &mut self.engaged {
                    e.panes.clear();
                }
                self.settle(cfg, fx);
            }
            Input::Focus { addr } => {
                // Пользователь сам вернулся в остановленное окно: процесс
                // продолжает работу сразу, не дожидаясь отбоя.
                if let Some(e) = &mut self.engaged
                    && !e.frozen.is_empty()
                    && e.prev.as_ref().is_some_and(|p| p.addr == addr)
                {
                    log::info!("внимание: фокус вернулся в остановленное окно {addr} — SIGCONT {:?}", e.frozen);
                    fx.thaw(&e.frozen);
                    e.frozen.clear();
                }
            }
        }
    }

    /// Панель вошла в `blocked`.
    fn entered(&mut self, pane: String, cfg: &Attention, fx: &mut dyn Effects) {
        if let Some(e) = &mut self.engaged {
            e.panes.insert(pane);
            return;
        }
        if !cfg.enabled {
            return;
        }
        let active = fx.active();
        if active.as_ref().is_some_and(|w| w.herdr) {
            log::info!("внимание: панель {pane} ждёт ответа, окно herdr уже с фокусом");
            return;
        }
        let players = if cfg.media_pause {
            let p = fx.playing();
            if !p.is_empty() {
                fx.pause(&p);
            }
            p
        } else {
            Vec::new()
        };
        let frozen = match &active {
            Some(w) if cfg.freeze_fullscreen && w.fullscreen && w.pid > 1 && !cfg.freeze_excluded(&w.class) => fx.freeze(w.pid),
            _ => Vec::new(),
        };
        log::info!("внимание: панель {pane} ждёт ответа; прежнее окно {:?}, пауза {players:?}, остановлены {frozen:?}", active.as_ref().map(|w| (&w.addr, &w.class)));
        // Даже без фокуса на herdr состояние запоминается: отбой обязан
        // вернуть остановленные процессы и плееры.
        self.engaged = Some(Engaged { panes: BTreeSet::from([pane]), prev: active, players, frozen });
        if let Err(e) = fx.focus_app() {
            log::warn!("внимание: фокус на herdr: {e:#}");
        }
    }

    /// Отбой, если панелей сигнала в `blocked` не осталось.
    fn settle(&mut self, _cfg: &Attention, fx: &mut dyn Effects) {
        if self.engaged.as_ref().is_none_or(|e| !e.panes.is_empty()) {
            return;
        }
        let e = self.engaged.take().unwrap();
        log::info!("внимание: вопросов нет — отбой: SIGCONT {:?}, play {:?}", e.frozen, e.players);
        // Сначала процессы: плеер в остановленном процессе не ответил бы по D-Bus.
        if !e.frozen.is_empty() {
            fx.thaw(&e.frozen);
        }
        if !e.players.is_empty() {
            fx.play(&e.players);
        }
        let Some(prev) = e.prev else { return };
        // Фокус возвращается, только если пользователь остался в herdr.
        if !fx.active().is_some_and(|w| w.herdr) {
            log::info!("внимание: фокус не в herdr — прежнее окно {} не возвращается", prev.addr);
            return;
        }
        if !fx.window_exists(&prev.addr) {
            log::info!("внимание: прежнего окна {} больше нет", prev.addr);
            return;
        }
        if let Err(err) = fx.focus(&prev.addr) {
            log::warn!("внимание: возврат фокуса на {}: {err:#}", prev.addr);
        }
    }
}

// ---- Остановка процессов -------------------------------------------------------

/// Процессы, которые не останавливаются никогда, по имени `comm` (не длиннее
/// 15 знаков): терминал и сервер herdr, композитор, демон, панель.
const NEVER_FREEZE: &[&str] = &["herdr", "workspaced", "qs", "quickshell", "Hyprland", "systemd", "dbus-daemon", "dbus-broker", "pipewire", "wireplumber", "Xwayland"];

fn never_freeze(comm: &str) -> bool {
    NEVER_FREEZE.contains(&comm) || comm.starts_with("wezterm") || comm.starts_with("herdr")
}

/// Процесс из `/proc`: pid, родитель, имя, владелец.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub uid: u32,
}

/// Кого останавливать: `root` и его потомки, кроме процессов из
/// [`NEVER_FREEZE`], чужих процессов, pid 1 и самого демона (`me`). Если сам
/// `root` из запретного списка, не останавливается никто.
pub fn freeze_set(root: i32, procs: &[Proc], me: i32, uid: u32) -> Vec<i32> {
    let ok = |p: &Proc| p.pid > 1 && p.pid != me && p.uid == uid && !never_freeze(&p.comm);
    let Some(r) = procs.iter().find(|p| p.pid == root) else { return Vec::new() };
    if !ok(r) {
        return Vec::new();
    }
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let parent = out[i];
        for p in procs.iter().filter(|p| p.ppid == parent && ok(p)) {
            if !out.contains(&p.pid) {
                out.push(p.pid);
            }
        }
        i += 1;
    }
    out
}

/// Разбор `/proc/<pid>/stat`: (родитель, имя). Имя в скобках может содержать
/// пробелы и скобки, поэтому поля после него ищутся от последней `)`.
pub fn parse_stat(stat: &str) -> Option<(i32, String)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_string();
    let mut rest = stat.get(close + 1..)?.split_whitespace();
    let _state = rest.next()?;
    let ppid = rest.next()?.parse().ok()?;
    Some((ppid, comm))
}

pub fn read_procs() -> Vec<Proc> {
    use std::os::unix::fs::MetadataExt;
    let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
    dir.flatten()
        .filter_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let uid = e.metadata().ok()?.uid();
            let (ppid, comm) = parse_stat(&std::fs::read_to_string(e.path().join("stat")).ok()?)?;
            Some(Proc { pid, ppid, comm, uid })
        })
        .collect()
}

/// Файл со списком остановленных pid: по нему процессы продолжают работу
/// после падения демона (при следующем старте) и при его завершении.
pub fn frozen_file() -> PathBuf {
    dirs::state_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/state")).join("workspaced").join("attention-frozen")
}

fn write_frozen(path: &Path, pids: &[i32]) -> Result<()> {
    if pids.is_empty() {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => return Ok(()),
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text: String = pids.iter().map(|p| format!("{p}\n")).collect();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_frozen(path: &Path) -> Vec<i32> {
    std::fs::read_to_string(path).map(|t| t.lines().filter_map(|l| l.trim().parse().ok()).filter(|p: &i32| *p > 1).collect()).unwrap_or_default()
}

/// Остановить процесс окна с потомками. Список пишется в файл до `SIGSTOP`;
/// не записался — не останавливается никто.
pub fn freeze_tree(root: i32) -> Vec<i32> {
    let me = std::process::id() as i32;
    let uid = match std::fs::metadata("/proc/self") {
        Ok(m) => std::os::unix::fs::MetadataExt::uid(&m),
        Err(e) => {
            log::warn!("внимание: /proc/self: {e} — процессы не останавливаются");
            return Vec::new();
        }
    };
    let pids = freeze_set(root, &read_procs(), me, uid);
    if pids.is_empty() {
        return pids;
    }
    if let Err(e) = write_frozen(&frozen_file(), &pids) {
        log::warn!("внимание: файл остановленных процессов не записан ({e:#}) — процессы не останавливаются");
        return Vec::new();
    }
    let mut done = Vec::new();
    for &p in &pids {
        match kill(Pid::from_raw(p), Signal::SIGSTOP) {
            Ok(()) => done.push(p),
            Err(e) => log::warn!("внимание: SIGSTOP {p}: {e}"),
        }
    }
    done
}

/// `SIGCONT` процессам и очистка файла.
pub fn thaw(pids: &[i32]) {
    for &p in pids {
        if let Err(e) = kill(Pid::from_raw(p), Signal::SIGCONT) {
            log::warn!("внимание: SIGCONT {p}: {e}");
        }
    }
    if let Err(e) = write_frozen(&frozen_file(), &[]) {
        log::warn!("внимание: файл остановленных процессов не очищен: {e:#}");
    }
}

/// `SIGCONT` всем процессам из файла: при старте демона (остались после
/// падения), при завершении по сигналу и при панике.
pub fn thaw_persisted() {
    let path = frozen_file();
    let pids = read_frozen(&path);
    if !pids.is_empty() {
        log::info!("внимание: продолжают работу процессы из {}: {pids:?}", path.display());
        thaw(&pids);
    }
}

static TERM_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_terminate(_: nix::libc::c_int) {
    let fd = TERM_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        // write — безопасная для обработчика сигнала функция.
        unsafe {
            nix::libc::write(fd, b"x".as_ptr().cast(), 1);
        }
    }
}

/// Гарантия `SIGCONT` при завершении демона: SIGTERM и SIGINT будят поток,
/// который возвращает остановленные процессы и завершает демон; паника
/// тоже их возвращает. Снимок сессии при этом не пишется (README, «Что стоит
/// знать до правки»). Обработчик, а не маска сигналов: маску унаследовали бы
/// запускаемые демоном приложения.
pub fn install_guards() -> Result<()> {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, sigaction};
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        thaw_persisted();
        prev(info);
    }));
    let (rd, wr) = nix::unistd::pipe().context("канал сигналов")?;
    TERM_PIPE.store(std::os::fd::IntoRawFd::into_raw_fd(wr), Ordering::Relaxed);
    let act = SigAction::new(SigHandler::Handler(on_terminate), SaFlags::SA_RESTART, SigSet::empty());
    for s in [Signal::SIGTERM, Signal::SIGINT] {
        unsafe { sigaction(s, &act) }.with_context(|| format!("обработчик {s}"))?;
    }
    std::thread::spawn(move || {
        let mut f = std::fs::File::from(rd);
        let mut b = [0u8; 1];
        let _ = f.read(&mut b);
        log::info!("демон завершается по сигналу");
        thaw_persisted();
        std::process::exit(0);
    });
    Ok(())
}

// ---- Плееры MPRIS --------------------------------------------------------------

fn playerctl(args: &[&str]) -> Option<String> {
    match std::process::Command::new("playerctl").args(args).stdin(std::process::Stdio::null()).output() {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(_) => None,
        Err(e) => {
            log::warn!("внимание: playerctl: {e}");
            None
        }
    }
}

pub fn playing_players() -> Vec<String> {
    let Some(list) = playerctl(&["-l"]) else { return Vec::new() };
    list.lines().map(str::trim).filter(|p| !p.is_empty()).filter(|p| playerctl(&["-p", p, "status"]).is_some_and(|s| s.trim() == "Playing")).map(String::from).collect()
}

pub fn player_cmd(players: &[String], cmd: &str) {
    for p in players {
        if playerctl(&["-p", p, cmd]).is_none() {
            log::warn!("внимание: playerctl -p {p} {cmd} не выполнен");
        }
    }
}

// ---- Сервер herdr ----------------------------------------------------------------

/// Сокет сервера herdr: `HERDR_SOCKET_PATH` или путь по умолчанию.
pub fn herdr_socket() -> PathBuf {
    match std::env::var("HERDR_SOCKET_PATH") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => dirs::config_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config")).join("herdr").join("herdr.sock"),
    }
}

/// События, после которых состав панелей другой и подписка обновляется.
const LAYOUT_EVENTS: &[&str] = &["pane.created", "pane.closed", "pane.moved", "tab.closed", "workspace.closed"];

fn rpc(path: &Path, method: &str, params: Value) -> Result<Value> {
    let mut s = UnixStream::connect(path).with_context(|| format!("сокет herdr {}", path.display()))?;
    writeln!(s, "{}", json!({ "id": "workspaced", "method": method, "params": params }))?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    response(&line)
}

fn response(line: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(line).context("ответ herdr не разобран")?;
    if let Some(e) = v.get("error") {
        bail!("herdr: {}", e.get("message").and_then(Value::as_str).unwrap_or("ошибка"));
    }
    v.get("result").cloned().context("в ответе herdr нет result")
}

/// Панели из ответа `pane.list`: (идентификатор, статус агента).
pub fn parse_panes(result: &Value) -> Vec<(String, String)> {
    result
        .get("panes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|p| Some((p.get("pane_id")?.as_str()?.to_string(), p.get("agent_status").and_then(Value::as_str).unwrap_or("unknown").to_string()))).collect())
        .unwrap_or_default()
}

/// Строка потока подписки.
#[derive(Debug, PartialEq, Eq)]
pub enum StreamLine {
    Status { pane: String, blocked: bool },
    Layout,
    Other,
}

pub fn parse_line(line: &str) -> StreamLine {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return StreamLine::Other };
    match v.get("event").and_then(Value::as_str) {
        Some("pane.agent_status_changed") => {
            let d = v.get("data");
            match (d.and_then(|d| d.get("pane_id")).and_then(Value::as_str), d.and_then(|d| d.get("agent_status")).and_then(Value::as_str)) {
                (Some(p), Some(s)) => StreamLine::Status { pane: p.to_string(), blocked: s == "blocked" },
                _ => StreamLine::Other,
            }
        }
        // Имя события состава сервер пишет через подчёркивание (`pane_created`),
        // хотя подписка называет его через точку (`pane.created`).
        Some(e) if LAYOUT_EVENTS.iter().any(|l| l.replace('.', "_") == e.replace('.', "_")) => StreamLine::Layout,
        _ => StreamLine::Other,
    }
}

fn blocked_of(panes: &[(String, String)]) -> BTreeSet<String> {
    panes.iter().filter(|(_, s)| s == "blocked").map(|(p, _)| p.clone()).collect()
}

/// Одно соединение с сервером: подписка, снимок, события. Изменение состава
/// панелей — новая подписка с новым снимком. Возвращается при обрыве.
fn session(path: &Path, send: &dyn Fn(Input) -> bool) -> Result<()> {
    let mut fresh = true;
    loop {
        let panes = parse_panes(&rpc(path, "pane.list", json!({}))?);
        let mut subs: Vec<Value> = LAYOUT_EVENTS.iter().map(|t| json!({ "type": t })).collect();
        subs.extend(panes.iter().map(|(p, _)| json!({ "type": "pane.agent_status_changed", "pane_id": p })));
        let mut s = UnixStream::connect(path).with_context(|| format!("сокет herdr {}", path.display()))?;
        writeln!(s, "{}", json!({ "id": "workspaced-sub", "method": "events.subscribe", "params": { "subscriptions": subs } }))?;
        let mut reader = BufReader::new(s);
        let mut ack = String::new();
        reader.read_line(&mut ack)?;
        response(&ack).context("подписка herdr")?;
        // Снимок после установки подписки: изменения позже снимка придут событиями.
        let now = parse_panes(&rpc(path, "pane.list", json!({}))?);
        if !send(Input::Snapshot { blocked: blocked_of(&now), fresh }) {
            return Ok(());
        }
        fresh = false;
        // Панель появилась между двумя снимками — подписки на неё нет.
        if now.iter().any(|(p, _)| !panes.iter().any(|(q, _)| q == p)) {
            continue;
        }
        let mut relayout = false;
        for line in reader.by_ref().lines() {
            match parse_line(&line?) {
                StreamLine::Status { pane, blocked } => {
                    if !send(Input::Status { pane, blocked }) {
                        return Ok(());
                    }
                }
                StreamLine::Layout => {
                    relayout = true;
                    break;
                }
                StreamLine::Other => {}
            }
        }
        if !relayout {
            bail!("сервер herdr закрыл соединение");
        }
    }
}

/// Поток сторожа: соединение с сервером herdr и переподключение по появлению
/// сокета (inotify на каталог: сервер и подмена `live-handoff` создают сокет
/// заново). `send` возвращает `false`, когда демон больше не принимает события.
pub fn watch_herdr(send: impl Fn(Input) -> bool) {
    use notify::{RecursiveMode, Watcher};
    let path = herdr_socket();
    let (Some(dir), Some(name)) = (path.parent().map(Path::to_path_buf), path.file_name().map(|n| n.to_os_string())) else {
        log::error!("внимание: путь сокета herdr {} без каталога", path.display());
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::error!("внимание: каталог {}: {e}", dir.display());
        return;
    }
    let (wtx, wrx) = std::sync::mpsc::channel();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = wtx.send(ev);
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            log::error!("внимание: наблюдатель сокета herdr: {e}");
            return;
        }
    };
    if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
        log::error!("внимание: наблюдатель {}: {e}", dir.display());
        return;
    }
    let touches = |ev: &notify::Event| ev.paths.iter().any(|p| p.file_name() == Some(name.as_os_str())) && matches!(ev.kind, notify::EventKind::Create(_) | notify::EventKind::Modify(notify::event::ModifyKind::Name(_)));
    loop {
        // События до попытки устарели: сокет проверяется самой попыткой.
        while wrx.try_recv().is_ok() {}
        match session(&path, &send) {
            Ok(()) => return,
            Err(e) => log::info!("внимание: herdr недоступен: {e:#}; жду сокет {}", path.display()),
        }
        if !send(Input::Disconnected) {
            return;
        }
        loop {
            match wrx.recv() {
                Ok(ev) if touches(&ev) => break,
                Ok(_) => {}
                Err(_) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        active: Option<Win>,
        playing: Vec<String>,
        exists: bool,
        log: Vec<String>,
        frozen_by: Vec<i32>,
    }

    impl Effects for Fake {
        fn active(&mut self) -> Option<Win> {
            self.active.clone()
        }
        fn playing(&mut self) -> Vec<String> {
            self.playing.clone()
        }
        fn pause(&mut self, p: &[String]) {
            self.log.push(format!("pause {}", p.join(",")));
        }
        fn play(&mut self, p: &[String]) {
            self.log.push(format!("play {}", p.join(",")));
        }
        fn freeze(&mut self, pid: i32) -> Vec<i32> {
            self.log.push(format!("freeze {pid}"));
            let mut v = vec![pid];
            v.extend(&self.frozen_by);
            v
        }
        fn thaw(&mut self, pids: &[i32]) {
            self.log.push(format!("thaw {pids:?}"));
        }
        fn focus_app(&mut self) -> Result<()> {
            self.log.push("focus herdr".into());
            if let Some(w) = &mut self.active {
                *w = herdr();
            }
            Ok(())
        }
        fn window_exists(&mut self, _: &str) -> bool {
            self.exists
        }
        fn focus(&mut self, addr: &str) -> Result<()> {
            self.log.push(format!("focus {addr}"));
            Ok(())
        }
    }

    fn cfg() -> Attention {
        Attention { enabled: true, freeze_exclude_classes: vec!["(?i)google-chrome.*".into(), "wezterm.*".into()], ..Attention::default() }
    }

    fn herdr() -> Win {
        Win { addr: "0xh".into(), class: "wezterm-herdr".into(), pid: 10, fullscreen: false, herdr: true }
    }

    fn browser() -> Win {
        Win { addr: "0xb".into(), class: "google-chrome".into(), pid: 20, fullscreen: true, herdr: false }
    }

    fn game() -> Win {
        Win { addr: "0xg".into(), class: "steam_app_1".into(), pid: 30, fullscreen: true, herdr: false }
    }

    fn st(p: &str, blocked: bool) -> Input {
        Input::Status { pane: p.into(), blocked }
    }

    #[test]
    fn video_in_browser_pauses_and_focuses_herdr() {
        let mut fx = Fake { active: Some(browser()), playing: vec!["chromium.instance1".into()], exists: true, ..Fake::default() };
        let mut w = Watch::default();
        w.feed(st("w1:p1", true), &cfg(), &mut fx);
        // Браузер полноэкранный, но в исключениях — не останавливается.
        assert_eq!(fx.log, vec!["pause chromium.instance1", "focus herdr"]);
        fx.log.clear();
        w.feed(st("w1:p1", false), &cfg(), &mut fx);
        assert_eq!(fx.log, vec!["play chromium.instance1", "focus 0xb"]);
    }

    #[test]
    fn fullscreen_game_is_frozen_and_thawed() {
        let mut fx = Fake { active: Some(game()), exists: true, frozen_by: vec![31], ..Fake::default() };
        let mut w = Watch::default();
        w.feed(st("w1:p1", true), &cfg(), &mut fx);
        assert_eq!(fx.log, vec!["freeze 30", "focus herdr"]);
        fx.log.clear();
        // Второй вопрос во время сигнала — та же очередь, отбой после обоих.
        w.feed(st("w2:p1", true), &cfg(), &mut fx);
        w.feed(st("w1:p1", false), &cfg(), &mut fx);
        assert!(fx.log.is_empty());
        w.feed(st("w2:p1", false), &cfg(), &mut fx);
        assert_eq!(fx.log, vec!["thaw [30, 31]", "focus 0xg"]);
    }

    #[test]
    fn herdr_active_does_nothing() {
        let mut fx = Fake { active: Some(herdr()), playing: vec!["p".into()], ..Fake::default() };
        let mut w = Watch::default();
        w.feed(st("w1:p1", true), &cfg(), &mut fx);
        w.feed(st("w1:p1", false), &cfg(), &mut fx);
        assert!(fx.log.is_empty());
    }

    #[test]
    fn user_left_herdr_before_release() {
        let mut fx = Fake { active: Some(game()), playing: vec!["p".into()], exists: true, ..Fake::default() };
        let mut w = Watch::default();
        w.feed(st("w1:p1", true), &cfg(), &mut fx);
        fx.log.clear();
        let mut other = browser();
        other.addr = "0xo".into();
        fx.active = Some(other);
        w.feed(st("w1:p1", false), &cfg(), &mut fx);
        assert_eq!(fx.log, vec!["thaw [30]", "play p"]);
    }

    #[test]
    fn returning_to_frozen_window_thaws_at_once() {
        let mut fx = Fake { active: Some(game()), exists: true, ..Fake::default() };
        let mut w = Watch::default();
        w.feed(st("w1:p1", true), &cfg(), &mut fx);
        fx.log.clear();
        w.feed(Input::Focus { addr: "0xg".into() }, &cfg(), &mut fx);
        assert_eq!(fx.log, vec!["thaw [30]"]);
        fx.log.clear();
        fx.active = Some(game());
        w.feed(st("w1:p1", false), &cfg(), &mut fx);
        assert!(fx.log.is_empty());
    }

    #[test]
    fn fresh_snapshot_gives_no_signal_but_resubscribe_does() {
        let mut fx = Fake { active: Some(browser()), exists: true, ..Fake::default() };
        let mut w = Watch::default();
        w.feed(Input::Snapshot { blocked: BTreeSet::from(["old".to_string()]), fresh: true }, &cfg(), &mut fx);
        assert!(fx.log.is_empty());
        w.feed(Input::Snapshot { blocked: BTreeSet::from(["old".to_string(), "new".to_string()]), fresh: false }, &cfg(), &mut fx);
        assert_eq!(fx.log, vec!["focus herdr"]);
        fx.log.clear();
        // Давно забытая панель отбой не задерживает.
        w.feed(Input::Snapshot { blocked: BTreeSet::from(["old".to_string()]), fresh: false }, &cfg(), &mut fx);
        assert_eq!(fx.log, vec!["focus 0xb"]);
    }

    #[test]
    fn disconnect_releases() {
        let mut fx = Fake { active: Some(game()), exists: false, ..Fake::default() };
        let mut w = Watch::default();
        w.feed(st("w1:p1", true), &cfg(), &mut fx);
        fx.log.clear();
        w.feed(Input::Disconnected, &cfg(), &mut fx);
        assert_eq!(fx.log, vec!["thaw [30]"]);
    }

    #[test]
    fn disabled_does_nothing() {
        let mut fx = Fake { active: Some(game()), ..Fake::default() };
        let mut w = Watch::default();
        w.feed(st("w1:p1", true), &Attention::default(), &mut fx);
        assert!(fx.log.is_empty());
    }

    #[test]
    fn freeze_set_skips_protected_and_foreign() {
        let p = |pid, ppid, comm: &str, uid| Proc { pid, ppid, comm: comm.into(), uid };
        let procs = vec![p(30, 1, "game", 1000), p(31, 30, "game-helper", 1000), p(32, 31, "wezterm-gui", 1000), p(33, 31, "root-thing", 0), p(34, 32, "child", 1000), p(99, 31, "workspaced", 1000)];
        assert_eq!(freeze_set(30, &procs, 99, 1000), vec![30, 31]);
        assert!(freeze_set(32, &procs, 99, 1000).is_empty());
        assert!(freeze_set(1, &procs, 99, 1000).is_empty());
        assert!(freeze_set(77, &procs, 99, 1000).is_empty());
    }

    #[test]
    fn stat_with_spaces_in_name() {
        assert_eq!(parse_stat("123 (Web Content (x)) S 45 123 0"), Some((45, "Web Content (x)".to_string())));
    }

    #[test]
    fn frozen_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("workspaced-attention-test-{}", std::process::id()));
        let f = dir.join("frozen");
        write_frozen(&f, &[30, 31]).unwrap();
        assert_eq!(read_frozen(&f), vec![30, 31]);
        write_frozen(&f, &[]).unwrap();
        assert!(!f.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stream_lines() {
        assert_eq!(parse_line(r#"{"data":{"agent":"claude","agent_status":"blocked","pane_id":"w2C:p1","workspace_id":"w2C"},"event":"pane.agent_status_changed"}"#), StreamLine::Status { pane: "w2C:p1".into(), blocked: true });
        assert_eq!(parse_line(r#"{"data":{"agent_status":"idle","pane_id":"w1:p1"},"event":"pane.agent_status_changed"}"#), StreamLine::Status { pane: "w1:p1".into(), blocked: false });
        assert_eq!(parse_line(r#"{"data":{"type":"pane_created","pane":{}},"event":"pane_created"}"#), StreamLine::Layout);
        assert_eq!(parse_line(r#"{"data":{"tab_id":"w2E:t9","type":"tab_closed","workspace_id":"w2E"},"event":"tab_closed"}"#), StreamLine::Layout);
        assert_eq!(parse_line(r#"{"data":{"type":"tab_created"},"event":"tab_created"}"#), StreamLine::Other);
        assert_eq!(parse_line(r#"{"id":"s","result":{"type":"subscription_started"}}"#), StreamLine::Other);
        let r: Value = serde_json::from_str(r#"{"panes":[{"pane_id":"a","agent_status":"blocked"},{"pane_id":"b","agent_status":"idle"}]}"#).unwrap();
        assert_eq!(blocked_of(&parse_panes(&r)), BTreeSet::from(["a".to_string()]));
    }
}

