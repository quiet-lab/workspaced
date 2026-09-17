//! Демон: владелец модели. Слушает события Hyprland, выполняет команды клиентов
//! и панели, запускает приложения, расставляет окна, пишет сессию `default`.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::{Config, PxRect, config_path};
use crate::hypr::{self, Client, Event, Hypr};
use crate::session;
use crate::state::{Foreign, Place, State};

/// Куда поставить окно после появления.
#[derive(Debug, Clone)]
enum Target {
    /// Прямоугольник ячейки или `rect` приложения на столе.
    Place(PxRect),
    /// Центр экрана поверх остальных (приложение без workspace).
    Center,
    /// Парковка (загрузка сессии для неактивного workspace).
    Pool,
}

/// Запущенное приложение, чьё окно ещё не появилось.
struct Pending {
    app: String,
    workspace: Option<String>,
    desktop: u8,
    child: Child,
    /// Исполняемый файл: `which` команды, затем `/proc/<pid>/exe`, пока процесс жив
    /// (обёртка-скрипт вроде `/usr/bin/firefox` выполняет настоящий бинарник).
    exe: Option<PathBuf>,
    /// Имя команды для мягкого сопоставления с классом окна.
    cmd_base: String,
    started: Instant,
    /// Процесс завершился без окна: ещё немного ждём новое окно у чужого процесса.
    exited_at: Option<Instant>,
    target: Target,
    focus: bool,
}

impl Pending {
    /// Похоже ли окно на результат этого запуска.
    fn matches(&self, c: &Client, ancestors: &[i32]) -> bool {
        if ancestors.contains(&(self.child.id() as i32)) {
            return true;
        }
        if let Some(e) = &self.exe
            && proc_exe(c.pid).as_ref() == Some(e)
        {
            return true;
        }
        class_matches(&c.class, &self.cmd_base)
    }
}

/// Класс окна содержит имя команды или наоборот (без учёта регистра).
fn class_matches(class: &str, cmd_base: &str) -> bool {
    let c = class.to_ascii_lowercase();
    let b = cmd_base.to_ascii_lowercase();
    !b.is_empty() && (c.contains(&b) || b.contains(&c))
}

/// Постороннее окно из снимка сессии, которое запущено и ждёт появления.
#[derive(Debug, Clone)]
pub struct ExpectedForeign {
    pub cmd: Vec<String>,
    pub cwd: Option<String>,
    pub desktop: Option<u8>,
    pub rect: PxRect,
}

enum Msg {
    Event(Event),
    Request { line: String, reply: Sender<String> },
    Subscribe(UnixStream),
    ConfigChanged,
    Tick,
}

pub struct Daemon {
    cfg: Config,
    cfg_text: String,
    cfg_path: PathBuf,
    expected: Vec<(ExpectedForeign, Instant)>,
    hypr: Hypr,
    st: State,
    mon: (i32, i32),
    current: u8,
    pending: Vec<Pending>,
    subs: Vec<UnixStream>,
    dirty: Option<Instant>,
    /// Геометрия окон до развёртывания (ключ — адрес окна), только в памяти.
    maximized: HashMap<String, PxRect>,
}

pub fn socket_path() -> Result<PathBuf> {
    let rt = std::env::var("XDG_RUNTIME_DIR").context("нет XDG_RUNTIME_DIR")?;
    Ok(PathBuf::from(rt).join("workspaced").join("sock"))
}

pub fn run() -> Result<()> {
    let cfg_path = config_path();
    let cfg = Config::load(&cfg_path)?;
    let cfg_text = std::fs::read_to_string(&cfg_path)?;
    let hypr = Hypr::new()?;
    let mon = hypr.monitor_size()?;
    let current = hypr.active_workspace()?.parse::<u8>().unwrap_or(1);
    let (tx, rx) = mpsc::channel::<Msg>();

    // События композитора.
    {
        let h = hypr.clone();
        let tx = tx.clone();
        std::thread::spawn(move || {
            let (etx, erx) = mpsc::channel();
            let h2 = h.clone();
            std::thread::spawn(move || {
                if let Err(e) = h2.events(etx) {
                    log::error!("события Hyprland: {e:#}");
                }
            });
            for ev in erx {
                if tx.send(Msg::Event(ev)).is_err() {
                    break;
                }
            }
            log::error!("поток событий Hyprland завершился, выход");
            std::process::exit(1);
        });
    }
    // Сокет клиентов и панели.
    {
        let path = socket_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).with_context(|| format!("не удалось открыть {}", path.display()))?;
        let tx = tx.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let tx = tx.clone();
                std::thread::spawn(move || serve_conn(conn, tx));
            }
        });
    }
    // Слежение за конфигом.
    {
        let tx = tx.clone();
        let path = cfg_path.clone();
        std::thread::spawn(move || watch_config(path, tx));
    }
    // Таймер.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                if tx.send(Msg::Tick).is_err() {
                    break;
                }
            }
        });
    }

    let mut d = Daemon { cfg, cfg_text, cfg_path, expected: Vec::new(), hypr, st: State::default(), mon, current, pending: Vec::new(), subs: Vec::new(), dirty: None, maximized: HashMap::new() };
    d.startup()?;
    log::info!("демон запущен, стол {}, монитор {}×{}", d.current, d.mon.0, d.mon.1);
    for msg in rx {
        match msg {
            Msg::Event(ev) => {
                if let Err(e) = d.on_event(ev) {
                    log::warn!("обработка события: {e:#}");
                }
            }
            Msg::Request { line, reply } => {
                let resp = d.handle_request(&line);
                let _ = reply.send(resp.to_string());
            }
            Msg::Subscribe(mut s) => {
                let state = d.state_json();
                if writeln!(s, "{state}").is_ok() {
                    d.subs.push(s);
                }
            }
            Msg::ConfigChanged => d.reload_config(),
            Msg::Tick => d.tick(),
        }
    }
    Ok(())
}

fn serve_conn(conn: UnixStream, tx: Sender<Msg>) {
    let reader = BufReader::new(match conn.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    });
    let mut writer = conn;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let is_sub = serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("cmd").and_then(|c| c.as_str()).map(|c| c == "subscribe")).unwrap_or(false);
        if is_sub {
            if let Ok(c) = writer.try_clone() {
                let _ = tx.send(Msg::Subscribe(c));
            }
            continue;
        }
        let (rtx, rrx) = mpsc::channel();
        if tx.send(Msg::Request { line, reply: rtx }).is_err() {
            break;
        }
        let Ok(resp) = rrx.recv() else { break };
        if writeln!(writer, "{resp}").is_err() {
            break;
        }
    }
}

fn watch_config(path: PathBuf, tx: Sender<Msg>) {
    use notify::{RecursiveMode, Watcher};
    let (wtx, wrx) = mpsc::channel();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = wtx.send(ev);
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            log::error!("наблюдатель конфига: {e}");
            return;
        }
    };
    // Следим за каталогом: редакторы пишут через временный файл и переименование.
    if let Some(dir) = path.parent()
        && let Err(e) = watcher.watch(dir, RecursiveMode::NonRecursive)
    {
        log::error!("наблюдатель конфига: {e}");
        return;
    }
    let mut last = Instant::now() - Duration::from_secs(10);
    for ev in wrx {
        let touches = ev.paths.iter().any(|p| p.file_name() == path.file_name());
        if touches && last.elapsed() > Duration::from_millis(300) {
            last = Instant::now();
            std::thread::sleep(Duration::from_millis(200));
            let _ = tx.send(Msg::ConfigChanged);
        }
    }
}

fn err_json(e: impl std::fmt::Display) -> Value {
    json!({ "ok": false, "error": e.to_string() })
}

impl Daemon {
    // ---- Запуск и конфиг ------------------------------------------------------

    /// При старте: теги окон дают приложения, сессия `default` — списки и ленивое восстановление.
    fn startup(&mut self) -> Result<()> {
        session::restore_default(self)?;
        // Стартовый workspace из конфига поднимается после сессии: если сессия
        // уже подняла его, повторное поднятие только расставляет окна.
        if let Some(st) = self.cfg.startup.clone() {
            log::info!("стартовый workspace {} на столе {}", st.workspace, st.desktop);
            if let Err(e) = self.raise(&st.workspace, Some(st.desktop)) {
                log::warn!("стартовый workspace {}: {e:#}", st.workspace);
            }
        }
        self.mark_dirty();
        Ok(())
    }

    pub fn reload_config(&mut self) {
        match Config::load(&self.cfg_path) {
            Ok(cfg) => {
                self.cfg = cfg;
                self.cfg_text = std::fs::read_to_string(&self.cfg_path).unwrap_or_default();
                // Назначения ячеек пересобираются из конфига для workspace, которых ещё не касались.
                self.st.cells.retain(|ws, _| self.cfg.workspaces.contains_key(ws));
                log::info!("конфиг перечитан: {} workspace, {} приложений", self.cfg.workspaces.len(), self.cfg.apps.len());
                match Command::new("hyprctl").arg("reload").arg("config-only").output() {
                    Ok(o) if o.status.success() => {}
                    Ok(o) => log::warn!("hyprctl reload: {}", String::from_utf8_lossy(&o.stderr)),
                    Err(e) => log::warn!("hyprctl reload: {e}"),
                }
                self.broadcast();
            }
            Err(e) => log::error!("конфиг не перечитан, действует прежний: {e:#}"),
        }
    }

    pub fn cfg(&self) -> &Config {
        &self.cfg
    }
    pub fn cfg_path(&self) -> &Path {
        &self.cfg_path
    }
    pub fn cfg_text(&self) -> &str {
        &self.cfg_text
    }
    pub fn set_cfg_text(&mut self, t: String) {
        self.cfg_text = t;
    }
    pub fn state(&self) -> &State {
        &self.st
    }
    pub fn expect_foreign(&mut self, e: ExpectedForeign) {
        self.expected.push((e, Instant::now()));
    }
    pub fn state_mut(&mut self) -> &mut State {
        &mut self.st
    }
    pub fn hypr(&self) -> &Hypr {
        &self.hypr
    }
    pub fn current_desktop(&self) -> u8 {
        self.current
    }
    pub fn set_current(&mut self, n: u8) {
        self.current = n;
    }
    pub fn mon(&self) -> (i32, i32) {
        self.mon
    }

    fn mark_dirty(&mut self) {
        self.dirty = Some(Instant::now());
    }

    fn tick(&mut self) {
        if let Some(t) = self.dirty
            && t.elapsed() >= Duration::from_millis(500)
        {
            self.dirty = None;
            if let Err(e) = session::save_default(self) {
                log::warn!("запись сессии default: {e:#}");
            }
        }
        self.check_pending();
        self.expected.retain(|(_, t)| t.elapsed() < Duration::from_secs(30));
    }

    // ---- События ----------------------------------------------------------------

    fn on_event(&mut self, ev: Event) -> Result<()> {
        match ev {
            Event::OpenWindow { addr, .. } => self.on_open(&addr)?,
            Event::CloseWindow { addr } => {
                self.st.foreign.remove(&addr);
                self.mark_dirty();
                self.broadcast();
            }
            Event::MoveWindow { .. } => {
                self.mark_dirty();
                self.broadcast();
            }
            Event::Workspace { name } => {
                if let Ok(n) = name.parse::<u8>()
                    && (1..=8).contains(&n)
                {
                    self.current = n;
                    if let Some(ws) = self.st.lazy.remove(&n) {
                        log::info!("стол {n}: ленивое поднятие {ws}");
                        if let Err(e) = self.raise(&ws, Some(n)) {
                            log::warn!("ленивое поднятие {ws}: {e:#}");
                        }
                    }
                    self.mark_dirty();
                    self.broadcast();
                }
            }
            Event::ActiveWindow { .. } | Event::Other(_) => {}
        }
        Ok(())
    }

    /// Новое окно: либо ожидаемое приложение, либо постороннее.
    fn on_open(&mut self, addr: &str) -> Result<()> {
        let clients = self.hypr.clients()?;
        let Some(c) = clients.iter().find(|c| c.address == *addr).cloned() else { return Ok(()) };
        if c.app().is_some() {
            self.mark_dirty();
            self.broadcast();
            return Ok(());
        }
        let ancestors = ancestors(c.pid);
        let hit = self
            .pending
            .iter()
            .position(|p| ancestors.contains(&(p.child.id() as i32)))
            .or_else(|| self.pending.iter().position(|p| p.matches(&c, &ancestors)));
        if let Some(i) = hit {
            let p = self.pending.remove(i);
            self.adopt(&c, p)?;
        } else {
            let (cmd, cwd) = proc_info(c.pid);
            if let Some(i) = self.expected.iter().position(|(e, _)| e.cmd == cmd && e.cwd == cwd) {
                // Постороннее окно из снимка: на своё место.
                let (e, _) = self.expected.remove(i);
                let mut ex = Vec::new();
                match e.desktop {
                    Some(n) => {
                        if c.desktop() != Some(n) {
                            ex.push(hypr::d_move_to(&c.address, &n.to_string()));
                        }
                        ex.extend(hypr::d_place(&c.address, e.rect));
                    }
                    None => ex.push(hypr::d_move_to(&c.address, "special:pool")),
                }
                self.st.foreign.insert(c.address.clone(), Foreign { rect: e.rect, cmd, cwd });
                self.hypr.dispatch_all(&ex)?;
            } else {
                // Постороннее окно остаётся свободным: частью workspace его делает
                // только команда сохранения.
                self.st.foreign.insert(c.address.clone(), Foreign { rect: c.rect(), cmd, cwd });
                log::info!("постороннее окно {} ({}) свободно", c.address, c.class);
            }
        }
        self.mark_dirty();
        self.broadcast();
        Ok(())
    }

    /// Окно ожидаемого приложения: тег, стол, место, фокус.
    fn adopt(&mut self, c: &Client, p: Pending) -> Result<()> {
        let mut ex = vec![hypr::d_tag(&c.address, &format!("app:{}", p.app))];
        match &p.target {
            Target::Pool => ex.push(hypr::d_move_to(&c.address, "special:pool")),
            Target::Place(r) => {
                if c.desktop() != Some(p.desktop) {
                    ex.push(hypr::d_move_to(&c.address, &p.desktop.to_string()));
                }
                ex.extend(hypr::d_place(&c.address, *r));
            }
            Target::Center => {
                if c.desktop() != Some(p.desktop) {
                    ex.push(hypr::d_move_to(&c.address, &p.desktop.to_string()));
                }
                let r = PxRect { x: (self.mon.0 - c.size.0) / 2, y: (self.mon.1 - c.size.1) / 2, w: c.size.0, h: c.size.1 };
                ex.extend(hypr::d_place(&c.address, r));
                let (cmd, cwd) = proc_info(c.pid);
                self.st.foreign.insert(c.address.clone(), Foreign { rect: r, cmd, cwd });
            }
        }
        if p.focus {
            ex.push(hypr::d_focus_window(&c.address));
            ex.push(hypr::d_bring_to_top());
        } else if let Some(ws) = &p.workspace
            && p.desktop == self.current
        {
            // Новое окно забирает фокус у композитора; возвращаем его главному окну workspace.
            let main = self.cfg.workspaces.get(ws).and_then(|w| w.main.clone()).or_else(|| self.st.main_app(&self.cfg, ws, self.mon));
            if let Some(m) = main
                && m != p.app
                && let Ok(clients) = self.hypr.clients()
                && let Some(mw) = Self::find_app_window(&clients, &m)
                && mw.desktop() == Some(p.desktop)
            {
                ex.push(hypr::d_focus_window(&mw.address));
                ex.push(hypr::d_bring_to_top());
            }
        }
        log::info!("окно {} → приложение {}", c.address, p.app);
        self.hypr.dispatch_all(&ex)
    }

    /// Просроченные запуски и одноэкземплярные приложения (процесс вышел без окна).
    fn check_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let mut i = 0;
        while i < self.pending.len() {
            let p = &mut self.pending[i];
            if p.exited_at.is_none() {
                // Пока процесс жив, уточняем исполняемый файл (после exec обёртки).
                if let Some(e) = proc_exe(p.child.id() as i32) {
                    p.exe = Some(e);
                }
                if matches!(p.child.try_wait(), Ok(Some(_))) {
                    p.exited_at = Some(Instant::now());
                }
            }
            let waited_after_exit = p.exited_at.is_some_and(|t| t.elapsed() > Duration::from_secs(2));
            let expired = p.started.elapsed() > Duration::from_secs(30);
            if waited_after_exit {
                // Одноэкземплярное приложение: окно осталось у уже работающего процесса.
                let p = self.pending.remove(i);
                let found = self.hypr.clients().ok().and_then(|cs| cs.into_iter().find(|c| c.app().is_none() && p.matches(c, &[])));
                match found {
                    Some(c) => {
                        log::info!("{}: процесс завершился без окна, принято окно {} ({})", p.app, c.address, c.class);
                        self.st.foreign.remove(&c.address);
                        if let Err(e) = self.adopt(&c, p) {
                            log::warn!("принятие окна: {e:#}");
                        }
                        self.mark_dirty();
                        self.broadcast();
                    }
                    None => log::warn!("{}: процесс завершился, окна нет", p.app),
                }
                continue;
            }
            if expired {
                let p = self.pending.remove(i);
                log::warn!("{}: окно не появилось за 30 с, ожидание снято", p.app);
                continue;
            }
            i += 1;
        }
    }

    // ---- Запуск -----------------------------------------------------------------

    fn spawn(&mut self, app: &str, workspace: Option<&str>, desktop: u8, target: Target, focus: bool) -> Result<()> {
        if self.pending.iter().any(|p| p.app == app) {
            return Ok(());
        }
        let Some(a) = self.cfg.apps.get(app) else { bail!("приложение {app} не описано") };
        let ws = workspace.and_then(|w| self.cfg.workspaces.get(w));
        let (cmd, args, cwd, env) = self.cfg.app_command(ws, a);
        let mut command = Command::new(&cmd);
        command.args(&args).envs(&env).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        if let Some(dir) = &cwd {
            if Path::new(dir).is_dir() {
                command.current_dir(dir);
            } else {
                log::warn!("{app}: каталога {dir} нет, запуск без cwd");
            }
        }
        // Своя сессия процессов: приложение переживает остановку демона.
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid().map(|_| ()).map_err(std::io::Error::other)
            });
        }
        let child = command.spawn().with_context(|| format!("{app}: не удалось запустить {cmd}"))?;
        let exe = which(&cmd).and_then(|p| p.canonicalize().ok());
        let cmd_base = Path::new(&cmd).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        log::info!("{app}: запущен pid {} ({cmd} {})", child.id(), args.join(" "));
        self.pending.push(Pending { app: app.to_string(), workspace: workspace.map(String::from), desktop, child, exe, cmd_base, started: Instant::now(), exited_at: None, target, focus });
        Ok(())
    }

    // ---- Операции ---------------------------------------------------------------

    fn find_app_window<'a>(clients: &'a [Client], app: &str) -> Option<&'a Client> {
        clients.iter().filter(|c| c.app().as_deref() == Some(app)).min_by_key(|c| c.on_hidden())
    }

    /// Захват открытых окон для приложений без окна с тегом (design D2–D3
    /// изменения work-workspace): кандидат — окно подходящего класса и
    /// заголовка без тега или с тегом приложения, которого нет в конфиге.
    /// Теги переставляются в композиторе и в локальном списке клиентов.
    fn adopt_untagged(&mut self, clients: &mut [Client], apps: &[String]) -> Result<()> {
        for app in apps {
            if Self::find_app_window(clients, app).is_some() {
                continue;
            }
            let Some(cfg) = self.cfg.apps.get(app) else {
                continue;
            };
            let Some((class_re, title_re)) = cfg.matchers()? else {
                continue;
            };
            let Some(i) = pick_candidate(clients, &self.cfg, &class_re, title_re.as_ref()) else {
                continue;
            };
            let addr = clients[i].address.clone();
            let mut ex = Vec::new();
            if let Some(old) = clients[i].app() {
                ex.push(hypr::d_tag(&addr, &format!("-app:{old}")));
            }
            ex.push(hypr::d_tag(&addr, &format!("app:{app}")));
            self.hypr.dispatch_all(&ex)?;
            clients[i].tags.retain(|t| !t.starts_with("app:"));
            clients[i].tags.push(format!("app:{app}"));
            // Захваченное окно больше не постороннее.
            self.st.foreign.remove(&addr);
            log::info!("захват: окно {addr} ({}, «{}») → приложение {app}", clients[i].class, clients[i].title);
        }
        Ok(())
    }

    /// Поднять workspace на столе (design D5).
    /// Стол, на котором workspace сейчас активен.
    fn active_desktop_of(&self, ws: &str) -> Option<u8> {
        self.st.desktops.iter().find(|(_, d)| d.active.as_deref() == Some(ws)).map(|(n, _)| *n)
    }

    pub fn raise(&mut self, ws: &str, desktop: Option<u8>) -> Result<()> {
        if !self.cfg.workspaces.contains_key(ws) {
            bail!("workspace {ws} не найден");
        }
        // Без явного стола workspace, уже активный на другом столе, не переезжает:
        // демон переходит на тот стол и отдаёт фокус главному окну.
        let n = desktop.unwrap_or_else(|| self.active_desktop_of(ws).unwrap_or(self.current));
        let w = self.cfg.workspaces[ws].clone();
        let apps: Vec<String> = w.apps.keys().cloned().collect();
        let mut clients = self.hypr.clients()?;
        self.adopt_untagged(&mut clients, &apps)?;
        let mut ex = Vec::new();
        if n != self.current {
            ex.push(hypr::d_focus_desktop(n));
            self.current = n;
        }
        let mut main_addr: Option<String> = None;
        let main_app = w.main.clone().or_else(|| self.st.main_app(&self.cfg, ws, self.mon));
        for app in &apps {
            let rect = self.st.rect_for(&self.cfg, ws, app, self.mon);
            match Self::find_app_window(&clients, app) {
                Some(c) if !c.on_hidden() => {
                    if c.desktop() != Some(n) {
                        ex.push(hypr::d_move_to(&c.address, &n.to_string()));
                    }
                    if let Some(r) = rect {
                        ex.extend(hypr::d_place(&c.address, r));
                    }
                    if main_app.as_deref() == Some(app) {
                        main_addr = Some(c.address.clone());
                    }
                }
                Some(_) => {} // окно скрыто пользователем: не трогаем
                None => {
                    let target = rect.map(Target::Place).unwrap_or(Target::Center);
                    self.spawn(app, Some(ws), n, target, main_app.as_deref() == Some(app))?;
                }
            }
        }
        // Паркуются только окна живых приложений других workspace. Посторонние
        // окна и окна с тегом приложения, которого нет в конфиге, остаются на столе.
        for c in &clients {
            if c.desktop() != Some(n) {
                continue;
            }
            let Some(a) = c.app() else { continue };
            if !apps.contains(&a) && self.cfg.apps.contains_key(&a) {
                ex.push(hypr::d_move_to(&c.address, "special:pool"));
            }
        }
        if let Some(a) = &main_addr {
            ex.push(hypr::d_focus_window(a));
            ex.push(hypr::d_bring_to_top());
        }
        for (k, d) in self.st.desktops.iter_mut() {
            if *k != n && d.active.as_deref() == Some(ws) {
                d.active = None;
            }
        }
        let d = self.st.desktop(n);
        if !d.workspaces.iter().any(|x| x == ws) {
            d.workspaces.push(ws.to_string());
        }
        d.active = Some(ws.to_string());
        self.hypr.dispatch_all(&ex)?;
        self.mark_dirty();
        self.broadcast();
        Ok(())
    }

    /// Сделать приложение главным в workspace: обмен ячеек с текущим главным.
    fn make_main(&mut self, ws: &str, app: &str) -> Result<()> {
        let mut clients = self.hypr.clients()?;
        self.adopt_untagged(&mut clients, std::slice::from_ref(&app.to_string()))?;
        let n = self.current;
        let main_app = self.st.main_app(&self.cfg, ws, self.mon);
        let main_cell = self.cfg.templates[&self.cfg.workspaces[ws].template].main.clone();
        if main_app.as_deref() != Some(app) {
            let cells = self.st.cells_of(&self.cfg, ws, self.mon);
            let app_place = cells.get(app).cloned();
            match (main_app.as_deref(), app_place) {
                (Some(m), Some(p)) => {
                    cells.insert(m.to_string(), p);
                    cells.insert(app.to_string(), Place::Cell(main_cell));
                }
                (Some(m), None) => {
                    cells.remove(m);
                    cells.insert(app.to_string(), Place::Cell(main_cell));
                }
                (None, _) => {
                    cells.insert(app.to_string(), Place::Cell(main_cell));
                }
            }
        }
        let mut ex = Vec::new();
        if let Some(m) = &main_app
            && m != app
            && let Some(c) = Self::find_app_window(&clients, m)
            && !c.on_hidden()
            && let Some(r) = self.st.rect_for(&self.cfg, ws, m, self.mon)
        {
            ex.extend(hypr::d_place(&c.address, r));
        }
        let rect = self.st.rect_for(&self.cfg, ws, app, self.mon);
        match Self::find_app_window(&clients, app) {
            Some(c) => {
                if c.desktop() != Some(n) {
                    ex.push(hypr::d_move_to(&c.address, &n.to_string()));
                }
                if let Some(r) = rect {
                    ex.extend(hypr::d_place(&c.address, r));
                }
                ex.push(hypr::d_focus_window(&c.address));
                ex.push(hypr::d_bring_to_top());
            }
            None => self.spawn(app, Some(ws), n, rect.map(Target::Place).unwrap_or(Target::Center), true)?,
        }
        self.hypr.dispatch_all(&ex)?;
        self.mark_dirty();
        self.broadcast();
        Ok(())
    }

    /// Цепочка приложения (design D6). `apps` — кандидаты с одной цепочкой.
    pub fn app(&mut self, apps: &[String], desktop: Option<u8>, workspace: Option<&str>) -> Result<()> {
        for a in apps {
            if !self.cfg.apps.contains_key(a) {
                bail!("приложение {a} не описано");
            }
        }
        if let Some(n) = desktop
            && n != self.current
        {
            self.hypr.dispatch(&hypr::d_focus_desktop(n))?;
            self.current = n;
        }
        if let Some(ws) = workspace {
            self.raise(ws, None)?;
        }
        let in_ws = |cfg: &Config, ws: &str| -> Option<String> { apps.iter().find(|a| cfg.workspaces.get(ws).is_some_and(|w| w.apps.contains_key(*a))).cloned() };
        let n = self.current;
        // 1. Активный workspace текущего стола.
        if let Some(ws) = self.st.desktop(n).active.clone()
            && let Some(a) = in_ws(&self.cfg, &ws)
        {
            return self.make_main(&ws, &a);
        }
        // 2. Список текущего стола по порядку (workspace, активный на другом
        // столе, поднимается там же, см. raise).
        let list = self.st.desktop(n).workspaces.clone();
        for ws in &list {
            if let Some(a) = in_ws(&self.cfg, ws) {
                self.raise(ws, None)?;
                return self.make_main(ws, &a);
            }
        }
        // 3. Другие столы по возрастанию номера.
        let others: Vec<(u8, Vec<String>)> = self.st.desktops.iter().filter(|(k, _)| **k != n).map(|(k, d)| (*k, d.workspaces.clone())).collect();
        for (k, list) in others {
            for ws in &list {
                if let Some(a) = in_ws(&self.cfg, ws) {
                    self.raise(ws, Some(k))?;
                    return self.make_main(ws, &a);
                }
            }
        }
        // 3б. Workspace из конфига, который ещё не поднимали ни на одном столе.
        for (ws, _) in self.cfg.workspaces.clone() {
            if let Some(a) = in_ws(&self.cfg, &ws) {
                self.raise(&ws, Some(n))?;
                return self.make_main(&ws, &a);
            }
        }
        // 4. Приложение без workspace: плавающее в центре экрана.
        let app = apps[0].clone();
        let mut clients = self.hypr.clients()?;
        self.adopt_untagged(&mut clients, apps)?;
        if let Some(c) = Self::find_app_window(&clients, &app) {
            let mut ex = Vec::new();
            if c.on_hidden() {
                ex.push(hypr::d_move_to(&c.address, &n.to_string()));
            }
            ex.push(hypr::d_focus_window(&c.address));
            ex.push(hypr::d_bring_to_top());
            return self.hypr.dispatch_all(&ex);
        }
        self.spawn(&app, None, n, Target::Center, true)
    }

    /// Следующий workspace в списке текущего стола.
    pub fn next(&mut self) -> Result<()> {
        let d = self.st.desktop(self.current).clone();
        if d.workspaces.is_empty() {
            bail!("на столе {} нет workspace", self.current);
        }
        let idx = d.active.as_ref().and_then(|a| d.workspaces.iter().position(|w| w == a)).map(|i| (i + 1) % d.workspaces.len()).unwrap_or(0);
        let ws = d.workspaces[idx].clone();
        self.raise(&ws, None)
    }

    /// Убрать workspace из списка стола (по умолчанию текущего), окна паркуются.
    pub fn remove(&mut self, ws: &str, desktop: Option<u8>) -> Result<()> {
        let n = desktop.unwrap_or(self.current);
        let d = self.st.desktop(n);
        d.workspaces.retain(|w| w != ws);
        let was_active = d.active.as_deref() == Some(ws);
        if was_active {
            d.active = None;
            let clients = self.hypr.clients()?;
            let apps: Vec<String> = self.cfg.workspaces.get(ws).map(|w| w.apps.keys().cloned().collect()).unwrap_or_default();
            let mut ex = Vec::new();
            for c in &clients {
                if c.desktop() != Some(n) {
                    continue;
                }
                if c.app().is_some_and(|a| apps.contains(&a)) {
                    ex.push(hypr::d_move_to(&c.address, "special:pool"));
                }
            }
            self.hypr.dispatch_all(&ex)?;
        }
        self.mark_dirty();
        self.broadcast();
        Ok(())
    }

    /// Окна для сессии и панели.
    pub fn clients(&self) -> Result<Vec<Client>> {
        self.hypr.clients()
    }

    /// Запуск приложения при загрузке сессии: в ячейку на столе или на парковку.
    pub fn spawn_for_session(&mut self, app: &str, workspace: Option<&str>, desktop: Option<u8>) -> Result<()> {
        match desktop {
            Some(n) => {
                let rect = workspace.and_then(|w| self.st.rect_for(&self.cfg, w, app, self.mon));
                self.spawn(app, workspace, n, rect.map(Target::Place).unwrap_or(Target::Center), false)
            }
            None => self.spawn(app, workspace, self.current, Target::Pool, false),
        }
    }

    /// Запуск постороннего окна из снимка сессии по команде и каталогу.
    pub fn spawn_foreign(&mut self, cmd: &[String], cwd: Option<&str>) -> Result<()> {
        let Some((prog, args)) = cmd.split_first() else { bail!("пустая команда") };
        let mut command = Command::new(prog);
        command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        if let Some(dir) = cwd
            && Path::new(dir).is_dir()
        {
            command.current_dir(dir);
        }
        unsafe {
            command.pre_exec(|| nix::unistd::setsid().map(|_| ()).map_err(std::io::Error::other));
        }
        command.spawn().with_context(|| format!("не удалось запустить {prog}"))?;
        Ok(())
    }

    // ---- Половина рабочей области ---------------------------------------------

    /// Активное окно в половину рабочей области активного монитора. Расстояния
    /// одинаковые: рабочая область сужается на `gap`, делится пополам, окно
    /// отступает на `gap` внутрь половины; от края и между окнами получается 2·gap,
    /// как у ячеек workspace и плиток панели.
    fn half(&mut self, side: &str) -> Result<()> {
        let Some(win) = self.hypr.active_window()? else {
            log::info!("half {side}: активного окна нет");
            return Ok(());
        };
        let (cols, rows, col, row) = match side {
            "left" => (2, 1, 0, 0),
            "right" => (2, 1, 1, 0),
            "up" => (1, 2, 0, 0),
            "down" => (1, 2, 0, 1),
            other => bail!("half: неизвестная сторона {other:?} (left, right, up, down)"),
        };
        let r = grid_cell(self.hypr.active_monitor()?.work_area(), self.cfg.gap, cols, rows, col, row);
        self.place_floating(&win.address, r)?;
        log::info!("half {side}: {} → {},{} {}×{}", win.address, r.x, r.y, r.w, r.h);
        Ok(())
    }

    /// Активное окно в позицию на рабочей области (углы и центры рядов —
    /// половина ширины и высоты, `center` — половина ширины на всю высоту,
    /// `full` — вся область) по тому же правилу отступов, что у половин.
    fn place(&mut self, position: &str) -> Result<()> {
        let Some(win) = self.hypr.active_window()? else {
            log::info!("place {position}: активного окна нет");
            return Ok(());
        };
        let r = place_rect(self.hypr.active_monitor()?.work_area(), self.cfg.gap, position)?;
        self.place_floating(&win.address, r)?;
        log::info!("place {position}: {} → {},{} {}×{}", win.address, r.x, r.y, r.w, r.h);
        Ok(())
    }

    /// Окно плавающим в заданный прямоугольник; сессия `default` запомнит новое положение.
    fn place_floating(&mut self, addr: &str, r: PxRect) -> Result<()> {
        let mut ex = vec![hypr::d_float_on(addr)];
        ex.extend(hypr::d_place(addr, r));
        self.hypr.dispatch_all(&ex)?;
        self.mark_dirty();
        Ok(())
    }

    // ---- Развёртывание окна -------------------------------------------------------

    /// Активное окно на всю рабочую область с отступом 2·gap от каждого края, как у
    /// половин и ячеек. Повторный вызов на развёрнутом окне возвращает геометрию,
    /// запомненную перед развёртыванием; сдвинутое окно разворачивается заново.
    /// Полноэкранное окно сначала выводится из полноэкранного режима и всегда
    /// оказывается развёрнутым: возврат прежней геометрии — следующим вызовом.
    fn maximize(&mut self) -> Result<()> {
        let Some(mut win) = self.hypr.active_window()? else {
            log::info!("maximize: активного окна нет");
            return Ok(());
        };
        let mut from_fullscreen = false;
        if win.fullscreen != 0 {
            // В полноэкранном режиме j/activewindow отдаёт размер экрана, а не окна:
            // геометрию читаем заново после выхода. Снимается только тот режим,
            // в котором окно находится (2 — fullscreen, 1 — maximized).
            let mode = if win.fullscreen == 2 { "fullscreen" } else { "maximized" };
            self.hypr.dispatch(&hypr::d_fullscreen_unset(mode))?;
            win = match self.hypr.active_window()? {
                Some(w) if w.address == win.address => w,
                _ => {
                    log::info!("maximize: окно {} пропало после выхода из полноэкранного режима", win.address);
                    return Ok(());
                }
            };
            from_fullscreen = true;
        }
        // Забыть окна, которых уже нет.
        let alive: Vec<String> = self.hypr.clients()?.into_iter().map(|c| c.address).collect();
        self.maximized.retain(|a, _| alive.contains(a));

        let target = maximize_rect(self.hypr.active_monitor()?.work_area(), self.cfg.gap);
        let cur = win.rect();
        if cur == target {
            if from_fullscreen {
                log::info!("maximize: {} вышло из полноэкранного режима, уже развёрнуто", win.address);
                return Ok(());
            }
            match self.maximized.remove(&win.address) {
                Some(prev) => {
                    self.place_floating(&win.address, prev)?;
                    log::info!("maximize: {} возвращено → {},{} {}×{}", win.address, prev.x, prev.y, prev.w, prev.h);
                }
                None => log::info!("maximize: окно {} уже развёрнуто, прежняя геометрия неизвестна", win.address),
            }
            return Ok(());
        }
        self.maximized.insert(win.address.clone(), cur);
        self.place_floating(&win.address, target)?;
        log::info!("maximize: {} → {},{} {}×{}", win.address, target.x, target.y, target.w, target.h);
        Ok(())
    }

    // ---- Запросы и панель -------------------------------------------------------

    fn handle_request(&mut self, line: &str) -> Value {
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return err_json(format!("не JSON: {e}")),
        };
        let cmd = req.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
        let s = |k: &str| req.get(k).and_then(|v| v.as_str()).map(String::from);
        let desktop = req.get("desktop").and_then(|v| v.as_u64()).map(|v| v as u8);
        let r = match cmd {
            "raise" => match s("workspace") {
                Some(ws) => self.raise(&ws, desktop).map(|_| json!({"ok": true})),
                None => Err(anyhow::anyhow!("нет workspace")),
            },
            "app" => {
                let apps: Vec<String> = req.get("apps").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                if apps.is_empty() {
                    Err(anyhow::anyhow!("нет приложений"))
                } else {
                    let ws = s("workspace");
                    self.app(&apps, desktop, ws.as_deref()).map(|_| json!({"ok": true}))
                }
            }
            "next" => self.next().map(|_| json!({"ok": true})),
            "half" => match s("side") {
                Some(side) => self.half(&side).map(|_| json!({"ok": true})),
                None => Err(anyhow::anyhow!("нет side")),
            },
            "maximize" => self.maximize().map(|_| json!({"ok": true})),
            "place" => match s("position") {
                Some(position) => self.place(&position).map(|_| json!({"ok": true})),
                None => Err(anyhow::anyhow!("нет position")),
            },
            "remove" => match s("workspace") {
                Some(ws) => self.remove(&ws, desktop).map(|_| json!({"ok": true})),
                None => Err(anyhow::anyhow!("нет workspace")),
            },
            "save-workspace" => crate::save::save_workspace(self).map(|ws| json!({"ok": true, "workspace": ws})),
            "sessions" => session::list(self).map(|list| {
                let ev = json!({"event": "show-sessions", "sessions": list});
                self.send_all(&ev);
                json!({"ok": true})
            }),
            "session" => {
                let op = s("op").unwrap_or_default();
                let name = s("name").unwrap_or_default();
                match op.as_str() {
                    "save" => session::save_named(self, &name).map(|_| json!({"ok": true})),
                    "load" => session::load(self, &name).map(|_| json!({"ok": true})),
                    "list" => session::list(self).map(|l| json!({"ok": true, "sessions": l})),
                    _ => Err(anyhow::anyhow!("неизвестная операция {op}")),
                }
            }
            "status" => Ok(self.status_json()),
            other => Err(anyhow::anyhow!("неизвестная команда {other:?}")),
        };
        match r {
            Ok(v) => v,
            Err(e) => {
                log::warn!("{cmd}: {e:#}");
                err_json(format!("{e:#}"))
            }
        }
    }

    fn state_json(&self) -> Value {
        let clients = self.hypr.clients().unwrap_or_default();
        let mut desktops = serde_json::Map::new();
        for n in 1..=8u8 {
            let d = self.st.desktops.get(&n).cloned().unwrap_or_default();
            let list: Vec<Value> = d
                .workspaces
                .iter()
                .map(|ws| {
                    let w = self.cfg.workspaces.get(ws);
                    let apps: Vec<String> = w.map(|w| w.apps.keys().cloned().collect()).unwrap_or_default();
                    let windows: Vec<String> = clients.iter().filter(|c| c.app().is_some_and(|a| apps.contains(&a))).map(|c| c.address.clone()).collect();
                    json!({ "name": ws, "icon": w.and_then(|w| w.icon.clone()), "active": d.active.as_deref() == Some(ws), "apps": apps, "windows": windows })
                })
                .collect();
            desktops.insert(n.to_string(), json!({ "workspaces": list, "active": d.active }));
        }
        json!({ "event": "state", "current_desktop": self.current, "desktops": desktops })
    }

    fn status_json(&self) -> Value {
        let mut v = self.state_json();
        let clients = self.hypr.clients().unwrap_or_default();
        let windows: Vec<Value> = clients
            .iter()
            .map(|c| {
                json!({ "address": c.address, "class": c.class, "title": c.title, "app": c.app(), "workspace": c.workspace.name, "foreign": self.st.foreign.contains_key(&c.address), "rect": c.rect(), "pid": c.pid })
            })
            .collect();
        let pending: Vec<Value> = self.pending.iter().map(|p| json!({ "app": p.app, "pid": p.child.id(), "desktop": p.desktop })).collect();
        let obj = v.as_object_mut().unwrap();
        obj.insert("ok".into(), json!(true));
        obj.insert("windows".into(), json!(windows));
        obj.insert("pending".into(), json!(pending));
        obj.insert("cells".into(), serde_json::to_value(&self.st.cells).unwrap_or_default());
        obj.insert("lazy".into(), serde_json::to_value(&self.st.lazy).unwrap_or_default());
        obj.remove("event");
        v
    }

    fn send_all(&mut self, v: &Value) {
        let line = v.to_string();
        self.subs.retain_mut(|s| writeln!(s, "{line}").is_ok());
    }

    pub fn broadcast(&mut self) {
        if self.subs.is_empty() {
            return;
        }
        let v = self.state_json();
        self.send_all(&v);
    }

}

// ---- /proc ------------------------------------------------------------------

/// pid и все его предки.
fn ancestors(pid: i32) -> Vec<i32> {
    let mut chain = Vec::new();
    let mut p = pid;
    while p > 1 && chain.len() < 64 {
        chain.push(p);
        let Ok(st) = std::fs::read_to_string(format!("/proc/{p}/stat")) else { break };
        let Some(rest) = st.rsplit_once(')').map(|(_, r)| r) else { break };
        let Some(ppid) = rest.split_whitespace().nth(1).and_then(|s| s.parse().ok()) else { break };
        p = ppid;
    }
    chain
}

/// Командная строка и каталог процесса окна.
pub fn proc_info(pid: i32) -> (Vec<String>, Option<String>) {
    let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).map(|b| b.split(|&x| x == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()).unwrap_or_default();
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned());
    (cmd, cwd)
}

fn proc_exe(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

fn which(cmd: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        return Some(PathBuf::from(cmd));
    }
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| Path::new(d).join(cmd)).find(|p| p.is_file())
}

/// Ленивое поднятие для стола: используется при восстановлении сессии.
pub fn set_lazy(d: &mut Daemon, map: BTreeMap<u8, String>) {
    d.st.lazy = map;
}

/// Развёрнутая область: рабочая область без 2·gap с каждой стороны. У `half`
/// рабочая область сужается на gap, затем половина ещё на gap; здесь оба шага
/// складываются, и расстояние до краёв то же.
fn maximize_rect(work_area: PxRect, gap: i32) -> PxRect {
    work_area.inset(2 * gap)
}

/// Кандидат на захват: не на `special:hidden`, класс и заголовок подходят,
/// тега нет или он принадлежит приложению, которого нет в конфиге. Окно на
/// обычном столе предпочтительнее окна на `special:pool`.
fn pick_candidate(clients: &[Client], cfg: &Config, class_re: &regex::Regex, title_re: Option<&regex::Regex>) -> Option<usize> {
    let fits = |c: &Client| !c.on_hidden() && class_re.is_match(&c.class) && title_re.is_none_or(|t| t.is_match(&c.title)) && c.app().is_none_or(|a| !cfg.apps.contains_key(&a));
    let mut pool = None;
    for (i, c) in clients.iter().enumerate() {
        if !fits(c) {
            continue;
        }
        if !c.on_pool() {
            return Some(i);
        }
        pool.get_or_insert(i);
    }
    pool
}

/// Позиция окна по правилу одинаковых расстояний: рабочая область сужается на
/// gap, окно получает половину её ширины (кроме `full`) и половину высоты
/// (кроме `center` и `full`), ставится к левому краю, по центру или к правому
/// краю и в верхний или нижний ряд, затем сужается на gap. Центральные позиции
/// перекрывают боковые: это набор мест для одного окна, а не разбиение.
fn place_rect(work_area: PxRect, gap: i32, position: &str) -> Result<PxRect> {
    let area = work_area.inset(gap);
    let (hw, hh) = (area.w / 2, area.h / 2);
    let (x, y, w, h) = match position {
        "top-left" => (area.x, area.y, hw, hh),
        "top-center" => (area.x + (area.w - hw) / 2, area.y, hw, hh),
        "top-right" => (area.x + area.w - hw, area.y, hw, hh),
        "bottom-left" => (area.x, area.y + area.h - hh, hw, hh),
        "bottom-center" => (area.x + (area.w - hw) / 2, area.y + area.h - hh, hw, hh),
        "bottom-right" => (area.x + area.w - hw, area.y + area.h - hh, hw, hh),
        "center" => (area.x + (area.w - hw) / 2, area.y, hw, area.h),
        "full" => (area.x, area.y, area.w, area.h),
        other => bail!("place: неизвестная позиция {other:?} ({})", crate::config::PLACES.join(", ")),
    };
    Ok(PxRect { x, y, w, h }.inset(gap))
}

/// Ячейка сетки cols×rows по правилу одинаковых расстояний: рабочая область
/// сужается на gap, делится на равные ячейки (остаток деления достаётся
/// последним столбцу и строке), выбранная ячейка сужается на gap. От края и
/// между соседними ячейками получается 2·gap.
fn grid_cell(work_area: PxRect, gap: i32, cols: i32, rows: i32, col: i32, row: i32) -> PxRect {
    let area = work_area.inset(gap);
    let (cw, ch) = (area.w / cols, area.h / rows);
    let w = if col == cols - 1 { area.w - cw * col } else { cw };
    let h = if row == rows - 1 { area.h - ch * row } else { ch };
    PxRect { x: area.x + cw * col, y: area.y + ch * row, w, h }.inset(gap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximize_rect_with_panel_on_the_left() {
        // Монитор 3840×2160, панель резервирует 330 px слева, gap = 5.
        let area = PxRect { x: 330, y: 0, w: 3510, h: 2160 };
        assert_eq!(maximize_rect(area, 5), PxRect { x: 340, y: 10, w: 3490, h: 2140 });
    }

    #[test]
    fn maximize_rect_without_reserved() {
        let area = PxRect { x: 0, y: 0, w: 3840, h: 2160 };
        assert_eq!(maximize_rect(area, 5), PxRect { x: 10, y: 10, w: 3820, h: 2140 });
    }

    #[test]
    fn grid_halves_keep_previous_numbers() {
        let panel = PxRect { x: 330, y: 0, w: 3510, h: 2160 };
        assert_eq!(grid_cell(panel, 5, 2, 1, 0, 0), PxRect { x: 340, y: 10, w: 1740, h: 2140 });
        assert_eq!(grid_cell(panel, 5, 2, 1, 1, 0), PxRect { x: 2090, y: 10, w: 1740, h: 2140 });
        let full = PxRect { x: 0, y: 0, w: 3840, h: 2160 };
        assert_eq!(grid_cell(full, 5, 2, 1, 0, 0), PxRect { x: 10, y: 10, w: 1905, h: 2140 });
        assert_eq!(grid_cell(full, 5, 1, 2, 0, 1), PxRect { x: 10, y: 1085, w: 3820, h: 1065 });
    }

    #[test]
    fn place_positions_with_panel() {
        let panel = PxRect { x: 330, y: 0, w: 3510, h: 2160 };
        let r = |p| place_rect(panel, 5, p).unwrap();
        assert_eq!(r("top-left"), PxRect { x: 340, y: 10, w: 1740, h: 1065 });
        assert_eq!(r("top-center"), PxRect { x: 1215, y: 10, w: 1740, h: 1065 });
        assert_eq!(r("top-right"), PxRect { x: 2090, y: 10, w: 1740, h: 1065 });
        assert_eq!(r("bottom-left"), PxRect { x: 340, y: 1085, w: 1740, h: 1065 });
        assert_eq!(r("bottom-center"), PxRect { x: 1215, y: 1085, w: 1740, h: 1065 });
        assert_eq!(r("bottom-right"), PxRect { x: 2090, y: 1085, w: 1740, h: 1065 });
        assert_eq!(r("center"), PxRect { x: 1215, y: 10, w: 1740, h: 2140 });
        assert_eq!(r("full"), maximize_rect(panel, 5));
        assert!(place_rect(panel, 5, "left").is_err());
    }

    fn client(addr: &str, class: &str, title: &str, ws: &str, tags: &[&str]) -> Client {
        serde_json::from_value(json!({
            "address": addr, "class": class, "title": title, "workspace": { "id": 1, "name": ws },
            "tags": tags, "at": [0, 0], "size": [10, 10], "floating": true, "mapped": true
        }))
        .unwrap()
    }

    #[test]
    fn pick_candidate_by_class_title_and_tag() {
        let cfg = Config::parse(
            "[templates.t]\nmain = \"c\"\ncells = { c = { x = 0, y = 0, w = 10, h = 10 } }\n[apps.herdr]\ncmd = \"wezterm-gui\"\nclass = \"^org\\\\.wezfurlong\\\\.wezterm$\"\ntitle = \"^herdr · \"\n[apps.neovide]\ncmd = \"neovide\"\nclass = \"^neovide$\"\n[workspaces.work]\ntemplate = \"t\"\napps = { herdr = \"c\" }\n",
        )
        .unwrap();
        let clients = vec![
            client("0x1", "org.wezfurlong.wezterm", "bash · mne@dev-lab", "1", &[]),
            client("0x2", "org.wezfurlong.wezterm", "herdr · dev-lab", "special:pool", &[]),
            client("0x3", "org.wezfurlong.wezterm", "herdr · dev-lab", "2", &["app:terminal-dots"]),
            client("0x4", "org.wezfurlong.wezterm", "herdr · dev-lab", "special:hidden", &[]),
            client("0x5", "org.wezfurlong.wezterm", "herdr · x", "1", &["app:neovide"]),
            client("0x6", "neovide", "[Scratch]", "1", &["app:editor-dots"]),
        ];
        let (cre, tre) = cfg.apps["herdr"].matchers().unwrap().unwrap();
        // Заголовок отсекает 0x1, hidden — 0x4, тег живого приложения — 0x5; обычный стол предпочтительнее пула.
        assert_eq!(pick_candidate(&clients, &cfg, &cre, tre.as_ref()), Some(2));
        let only_pool = &clients[..2];
        assert_eq!(pick_candidate(only_pool, &cfg, &cre, tre.as_ref()), Some(1));
        let (cre, tre) = cfg.apps["neovide"].matchers().unwrap().unwrap();
        assert_eq!(pick_candidate(&clients, &cfg, &cre, tre.as_ref()), Some(5));
        assert_eq!(pick_candidate(&clients[..1], &cfg, &cre, tre.as_ref()), None);
    }

    #[test]
    fn place_positions_without_reserved() {
        let full = PxRect { x: 0, y: 0, w: 3840, h: 2160 };
        assert_eq!(place_rect(full, 5, "bottom-left").unwrap(), PxRect { x: 10, y: 1085, w: 1905, h: 1065 });
        assert_eq!(place_rect(full, 5, "top-center").unwrap(), PxRect { x: 967, y: 10, w: 1905, h: 1065 });
    }
}
