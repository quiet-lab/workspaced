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
    /// Приложение вне workspace: своё место по умолчанию (`rect` приложения),
    /// а без него центр экрана; окно ложится поверх остальных.
    Free(Option<PxRect>),
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
    /// Выражения `class` и `title` приложения: окно из контейнера (distrobox,
    /// podman) не потомок запущенного процесса, его узнают только по ним.
    class_re: Option<regex::Regex>,
    title_re: Option<regex::Regex>,
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
        if matcher_fits(self.class_re.as_ref(), self.title_re.as_ref(), c) {
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

/// Окно целиком подходит под `class` и, если задано, `title` приложения;
/// без `class` сопоставления нет.
fn matcher_fits(class_re: Option<&regex::Regex>, title_re: Option<&regex::Regex>, c: &Client) -> bool {
    class_re.is_some_and(|cr| cr.is_match(&c.class)) && title_re.is_none_or(|t| t.is_match(&c.title))
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

    /// Окно ожидаемого приложения: тег экземпляра, стол, место, фокус.
    fn adopt(&mut self, c: &Client, p: Pending) -> Result<()> {
        let clients = self.hypr.clients().unwrap_or_default();
        let num = free_instance(&clients, &p.app);
        let mut ex = vec![hypr::d_tag(&c.address, &format!("app:{}#{num}", p.app))];
        match &p.target {
            Target::Pool => ex.push(hypr::d_move_to(&c.address, "special:pool")),
            Target::Place(r) => {
                if c.desktop() != Some(p.desktop) {
                    ex.push(hypr::d_move_to(&c.address, &p.desktop.to_string()));
                }
                ex.extend(hypr::d_place(&c.address, *r));
            }
            Target::Free(rect) => {
                if c.desktop() != Some(p.desktop) {
                    ex.push(hypr::d_move_to(&c.address, &p.desktop.to_string()));
                }
                let r = rect.unwrap_or(PxRect { x: (self.mon.0 - c.size.0) / 2, y: (self.mon.1 - c.size.1) / 2, w: c.size.0, h: c.size.1 });
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
                && let Some(mw) = first_window(&self.cfg, &clients, &m)
                && mw.desktop() == Some(p.desktop)
            {
                ex.push(hypr::d_focus_window(&mw.address));
                ex.push(hypr::d_bring_to_top());
            }
        }
        log::info!("окно {} → приложение {} (экземпляр {num})", c.address, p.app);
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
        // Приложение без cmd только собирает окна: запускать нечего, место остаётся пустым.
        let Some((cmd, args, cwd, env)) = self.cfg.app_command(ws, a) else {
            log::info!("{app}: поля cmd нет, окно не открывается");
            return Ok(());
        };
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
        let (class_re, title_re) = a.matchers()?.map_or((None, None), |(c, t)| (Some(c), t));
        log::info!("{app}: запущен pid {} ({cmd} {})", child.id(), args.join(" "));
        self.pending.push(Pending { app: app.to_string(), workspace: workspace.map(String::from), desktop, child, exe, cmd_base, class_re, title_re, started: Instant::now(), exited_at: None, target, focus });
        Ok(())
    }

    // ---- Операции ---------------------------------------------------------------

    /// Захват открытых окон приложений (спецификация ws-daemon, «Захват
    /// открытых окон приложения»): берутся все подходящие окна, в том числе
    /// у приложения, окна которого уже есть. Теги переставляются
    /// в композиторе и в локальном списке клиентов.
    fn adopt_untagged(&mut self, clients: &mut [Client], apps: &[String]) -> Result<()> {
        for (i, app, num) in adopt_plan(&self.cfg, clients, apps)? {
            let addr = clients[i].address.clone();
            let tag = format!("app:{app}#{num}");
            let mut ex: Vec<String> = clients[i].app_tags().map(|t| hypr::d_tag(&addr, &format!("-{t}"))).collect();
            ex.push(hypr::d_tag(&addr, &tag));
            self.hypr.dispatch_all(&ex)?;
            clients[i].tags.retain(|t| !t.starts_with("app:"));
            clients[i].tags.push(tag);
            // Захваченное окно больше не постороннее.
            self.st.foreign.remove(&addr);
            log::info!("захват: окно {addr} ({}, «{}») → приложение {app}, экземпляр {num}", clients[i].class, clients[i].title);
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
            // Переезжают и расставляются окна всех экземпляров приложения, окна
            // его вариантов в том числе; скрытые пользователем не трогаются.
            let wins = placed_windows(&self.cfg, &clients, &apps, app);
            if wins.is_empty() {
                let target = rect.map(Target::Place).unwrap_or(Target::Free(None));
                self.spawn(app, Some(ws), n, target, main_app.as_deref() == Some(app))?;
                continue;
            }
            for c in wins.iter().filter(|c| !c.on_hidden()) {
                if c.desktop() != Some(n) {
                    ex.push(hypr::d_move_to(&c.address, &n.to_string()));
                }
                if let Some(r) = rect {
                    ex.extend(hypr::d_place(&c.address, r));
                }
            }
            if main_app.as_deref() == Some(app) {
                // Фокус получает первый экземпляр, он же поднимается наверх;
                // порядок остальных окон по глубине не меняется.
                main_addr = wins.iter().find(|c| !c.on_hidden()).map(|c| c.address.clone());
            }
        }
        // Паркуются только окна живых приложений других workspace. Посторонние
        // окна и окна с тегом приложения, которого нет в конфиге, остаются на столе.
        for c in &clients {
            if c.desktop() != Some(n) {
                continue;
            }
            let Some(a) = c.app() else { continue };
            if self.cfg.apps.contains_key(&a) && !apps.iter().any(|x| self.cfg.app_is(&a, x)) {
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
    /// Стопки приложений меняются ячейками целиком, фокус и верхнее место
    /// получает первый экземпляр вызванного приложения.
    fn make_main(&mut self, ws: &str, app: &str) -> Result<()> {
        let mut clients = self.hypr.clients()?;
        self.adopt_untagged(&mut clients, std::slice::from_ref(&app.to_string()))?;
        let ws_apps: Vec<String> = self.cfg.workspaces[ws].apps.keys().cloned().collect();
        let n = self.current;
        let main_app = self.st.main_app(&self.cfg, ws, self.mon);
        let main_cell = self.cfg.templates[&self.cfg.workspaces[ws].template].main.clone();
        if main_app.as_deref() != Some(app) {
            swap_cells(self.st.cells_of(&self.cfg, ws, self.mon), main_app.as_deref(), app, &main_cell);
        }
        let mut ex = Vec::new();
        if let Some(m) = &main_app
            && m != app
            && let Some(r) = self.st.rect_for(&self.cfg, ws, m, self.mon)
        {
            for c in placed_windows(&self.cfg, &clients, &ws_apps, m).iter().filter(|c| !c.on_hidden()) {
                ex.extend(hypr::d_place(&c.address, r));
            }
        }
        let rect = self.st.rect_for(&self.cfg, ws, app, self.mon);
        let wins = placed_windows(&self.cfg, &clients, &ws_apps, app);
        if wins.is_empty() {
            // Приложение без окон запускается; без cmd остаётся без окна, место пустым.
            self.spawn(app, Some(ws), n, rect.map(Target::Place).unwrap_or(Target::Free(None)), true)?;
        } else {
            for c in wins.iter().filter(|c| !c.on_hidden()) {
                if c.desktop() != Some(n) {
                    ex.push(hypr::d_move_to(&c.address, &n.to_string()));
                }
                if let Some(r) = rect {
                    ex.extend(hypr::d_place(&c.address, r));
                }
            }
            if let Some(c) = wins.iter().find(|c| !c.on_hidden()) {
                ex.push(hypr::d_focus_window(&c.address));
                ex.push(hypr::d_bring_to_top());
            }
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
        // 4. Приложение без workspace: плавающее на своём месте по умолчанию
        // (`rect` приложения, а без него — центр экрана).
        let app = apps[0].clone();
        let mut clients = self.hypr.clients()?;
        self.adopt_untagged(&mut clients, apps)?;
        let wins = app_windows(&self.cfg, &clients, &app);
        if let Some(c) = wins.iter().find(|c| !c.on_hidden()).or_else(|| wins.first()) {
            // Повторная цепочка только даёт фокус первому экземпляру и поднимает
            // его окно наверх, места не меняя.
            let mut ex = Vec::new();
            if c.on_hidden() {
                ex.push(hypr::d_move_to(&c.address, &n.to_string()));
            }
            ex.push(hypr::d_focus_window(&c.address));
            ex.push(hypr::d_bring_to_top());
            return self.hypr.dispatch_all(&ex);
        }
        let rect = State::app_rect(&self.cfg, &app, self.mon);
        self.spawn(&app, None, n, Target::Free(rect), true)
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
                if c.app().is_some_and(|a| apps.iter().any(|x| self.cfg.app_is(&a, x))) {
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
                self.spawn(app, workspace, n, rect.map(Target::Place).unwrap_or(Target::Free(None)), false)
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
                    // Окно варианта входит в состав как окно своего семейства.
                    let windows: Vec<String> = clients.iter().filter(|c| c.app().is_some_and(|a| apps.iter().any(|x| self.cfg.app_is(&a, x)))).map(|c| c.address.clone()).collect();
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
                json!({ "address": c.address, "class": c.class, "title": c.title, "app": c.app(), "instance": c.app_instance().map(|(_, n)| n), "workspace": c.workspace.name, "foreign": self.st.foreign.contains_key(&c.address), "rect": c.rect(), "pid": c.pid })
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

/// Окна приложения по возрастанию номера экземпляра. Окно варианта входит
/// и в набор своего семейства (спецификация ws-daemon, «Идентификация окон»);
/// номера у семейства и у варианта свои, поэтому при совпадении номеров окна
/// идут по имени приложения.
pub fn app_windows<'a>(cfg: &Config, clients: &'a [Client], app: &str) -> Vec<&'a Client> {
    let mut out: Vec<&Client> = clients.iter().filter(|c| c.app_instance().is_some_and(|(a, _)| cfg.app_is(&a, app))).collect();
    out.sort_by_key(|c| c.app_instance().map(|(a, n)| (n, a)));
    out
}

/// Окна, которые расставляет запись приложения `app` в workspace с набором
/// приложений `apps`. Окно варианта — окно своего семейства, но если вариант
/// сам описан в этом workspace, место ему задаёт его собственная запись, а не
/// запись семейства: более точное совпадение побеждает (спецификация ws-daemon,
/// «Расстановка окон»).
fn placed_windows<'a>(cfg: &Config, clients: &'a [Client], apps: &[String], app: &str) -> Vec<&'a Client> {
    app_windows(cfg, clients, app).into_iter().filter(|c| c.app().is_none_or(|own| own == app || !apps.contains(&own))).collect()
}

/// Первый экземпляр приложения: окно с наименьшим номером, скрытые последними.
fn first_window<'a>(cfg: &Config, clients: &'a [Client], app: &str) -> Option<&'a Client> {
    let wins = app_windows(cfg, clients, app);
    wins.iter().find(|c| !c.on_hidden()).or_else(|| wins.first()).copied()
}

/// Наименьший свободный номер, начиная с 1, среди занятых.
fn free_number(used: &[u32]) -> u32 {
    let mut used = used.to_vec();
    used.sort_unstable();
    let mut num = 1;
    for u in used {
        match u.cmp(&num) {
            std::cmp::Ordering::Equal => num += 1,
            std::cmp::Ordering::Greater => break,
            std::cmp::Ordering::Less => {}
        }
    }
    num
}

/// Наименьший свободный номер экземпляра приложения среди живых окон.
pub fn free_instance(clients: &[Client], app: &str) -> u32 {
    let used: Vec<u32> = clients.iter().filter_map(|c| c.app_instance()).filter(|(a, _)| a == app).map(|(_, n)| n).collect();
    free_number(&used)
}

/// Что захватить: окно (его номер в списке клиентов), приложение и номер
/// экземпляра. Кандидат — окно не на `special:hidden`, чей класс и заголовок
/// подходят приложению, без тега или с тегом приложения, которого нет
/// в конфиге. Сопоставление идёт сначала по вариантам, затем по семействам:
/// более точное совпадение побеждает. Окна обычных столов разбираются раньше
/// окон на `special:pool`, поэтому первый экземпляр — окно на столе.
fn adopt_plan(cfg: &Config, clients: &[Client], apps: &[String]) -> Result<Vec<(usize, String, u32)>> {
    // Порядок сопоставления: варианты по именам, затем семейства.
    let mut variants: Vec<String> = Vec::new();
    let mut families: Vec<String> = Vec::new();
    for app in apps {
        if !cfg.apps.contains_key(app) {
            continue;
        }
        if cfg.family_of(app).is_some() {
            variants.push(app.clone());
        } else {
            families.push(app.clone());
            variants.extend(cfg.variants_of(app).into_iter().map(String::from));
        }
    }
    for list in [&mut variants, &mut families] {
        list.sort();
        list.dedup();
    }
    let mut matchers: Vec<(String, regex::Regex, Option<regex::Regex>)> = Vec::new();
    for name in variants.iter().chain(families.iter()) {
        if let Some((class_re, title_re)) = cfg.apps[name].matchers().with_context(|| format!("приложение {name}"))? {
            matchers.push((name.clone(), class_re, title_re));
        }
    }
    let mut order: Vec<usize> = (0..clients.len()).collect();
    order.sort_by_key(|i| clients[*i].on_pool());
    let mut used: Vec<(String, u32)> = clients.iter().filter_map(|c| c.app_instance()).collect();
    let mut plan = Vec::new();
    for i in order {
        let c = &clients[i];
        if c.on_hidden() || c.app().is_some_and(|a| cfg.apps.contains_key(&a)) {
            continue;
        }
        let fitting: Vec<&String> = matchers.iter().filter(|(_, cr, tr)| matcher_fits(Some(cr), tr.as_ref(), c)).map(|(n, _, _)| n).collect();
        let Some(app) = fitting.first().map(|n| (*n).clone()) else { continue };
        // Пересечение выражений двух вариантов одного семейства статически
        // не проверить: окно достаётся первому по имени, а об остальных
        // подходящих вариантах демон пишет предупреждение в журнал.
        let rivals: Vec<&&String> = fitting.iter().skip(1).filter(|n| cfg.family_of(n).is_some() && cfg.family_of(n) == cfg.family_of(&app)).collect();
        if !rivals.is_empty() {
            log::warn!("окно {} ({}) подходит вариантам {app} и {}; отдано {app}", c.address, c.class, rivals.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", "));
        }
        let num = free_number(&used.iter().filter(|(a, _)| *a == app).map(|(_, n)| *n).collect::<Vec<u32>>());
        used.push((app.clone(), num));
        plan.push((i, app, num));
    }
    Ok(plan)
}

/// Обмен ячеек: приложение идёт в главную ячейку, прежнее главное — на его
/// место. Стопка меняется ячейкой целиком, остальные окна не двигаются.
fn swap_cells(cells: &mut BTreeMap<String, Place>, main_app: Option<&str>, app: &str, main_cell: &str) {
    let app_place = cells.get(app).cloned();
    match (main_app, app_place) {
        (Some(m), Some(p)) => {
            cells.insert(m.to_string(), p);
        }
        (Some(m), None) => {
            cells.remove(m);
        }
        (None, _) => {}
    }
    cells.insert(app.to_string(), Place::Cell(main_cell.to_string()));
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

    use crate::hypr::test_client as client;

    /// Семейство `wezterm` с вариантом `herdr`, семейство `chromium`
    /// с вариантом `chromium-mail` и обычные приложения.
    const CFG: &str = r#"
[templates.thirds]
main = "center"
[templates.thirds.cells]
left   = { x = -805, y = 10, w = 1920, h = 2140 }
center = { x = 1125, y = 10, w = 1920, h = 2140 }
right  = { x = 3055, y = 10, w = 1920, h = 2140 }

[apps.wezterm]
class = "^org\\.wezfurlong\\.wezterm$"

[apps.herdr]
family = "wezterm"
cmd = "wezterm-gui"
class = "^wezterm-herdr$"

[apps.chromium]
cmd = "chromium"
class = "(?i)^chromium(-browser)?$"

[apps.chromium-mail]
family = "chromium"
cmd = "chromium"
class = "(?i)^chromium(-browser)?$"
title = "^Gmail"

[apps.neovide]
cmd = "neovide"
class = "^neovide$"

[workspaces.work]
template = "thirds"
main = "herdr"
apps = { herdr = "center", chromium = "left", neovide = "right" }
"#;

    #[test]
    fn matcher_fits_by_class_and_title() {
        let class_re = regex::Regex::new("^(?:(?i)^google-chrome$)$").unwrap();
        let title_re = regex::Regex::new("^herdr · ").unwrap();
        let chrome = client("0x1", "google-chrome", "Новая вкладка", "2", &[]);
        let term = client("0x2", "org.wezfurlong.wezterm", "herdr · dev-lab", "1", &[]);
        let other = client("0x3", "org.wezfurlong.wezterm", "bash · mne", "1", &[]);
        assert!(matcher_fits(Some(&class_re), None, &chrome));
        assert!(!matcher_fits(Some(&class_re), None, &term));
        let wez = regex::Regex::new("^org\\.wezfurlong\\.wezterm$").unwrap();
        assert!(matcher_fits(Some(&wez), Some(&title_re), &term));
        assert!(!matcher_fits(Some(&wez), Some(&title_re), &other));
        // Без class сопоставления нет, даже если title подходит.
        assert!(!matcher_fits(None, Some(&title_re), &term));
    }

    #[test]
    fn instance_numbers_fill_gaps() {
        assert_eq!(free_number(&[]), 1);
        assert_eq!(free_number(&[1, 2]), 3);
        assert_eq!(free_number(&[2, 3]), 1);
        assert_eq!(free_number(&[1, 3]), 2);
        let clients = vec![
            client("0x1", "chromium", "Новости", "1", &["app:chromium#2"]),
            // Тег прежней версии без номера читается как экземпляр 1.
            client("0x2", "neovide", "[Scratch]", "1", &["app:neovide"]),
        ];
        assert_eq!(free_instance(&clients, "chromium"), 1);
        assert_eq!(free_instance(&clients, "neovide"), 2);
        assert_eq!(free_instance(&clients, "herdr"), 1);
    }

    #[test]
    fn app_windows_by_instance_and_family() {
        let cfg = Config::parse(CFG).unwrap();
        let clients = vec![
            client("0x1", "chromium", "Новости", "1", &["app:chromium#2"]),
            client("0x2", "chromium", "Gmail — Входящие", "1", &["app:chromium-mail#1"]),
            client("0x3", "chromium", "Документы", "1", &["app:chromium#1"]),
            client("0x4", "org.wezfurlong.wezterm", "bash · mne", "1", &["app:wezterm#1"]),
            client("0x5", "wezterm-herdr", "herdr · dev-lab", "1", &["app:herdr#1"]),
        ];
        let addrs = |app: &str| app_windows(&cfg, &clients, app).iter().map(|c| c.address.clone()).collect::<Vec<_>>();
        // Экземпляры семейства по возрастанию номера; окно варианта входит в набор.
        assert_eq!(addrs("chromium"), vec!["0x3", "0x2", "0x1"]);
        // У варианта только его собственные окна.
        assert_eq!(addrs("chromium-mail"), vec!["0x2"]);
        // Совпавшие номера семейства и варианта разводятся по имени приложения.
        assert_eq!(addrs("wezterm"), vec!["0x5", "0x4"]);
        assert_eq!(addrs("neovide"), Vec::<String>::new());
        assert_eq!(first_window(&cfg, &clients, "chromium").map(|c| c.address.clone()), Some("0x3".to_string()));
    }

    #[test]
    fn variant_window_is_placed_by_its_own_entry() {
        let cfg = Config::parse(CFG).unwrap();
        let clients = vec![
            client("0x1", "chromium", "Новости", "1", &["app:chromium#1"]),
            client("0x2", "chromium", "Gmail — Входящие", "1", &["app:chromium-mail#1"]),
        ];
        let addrs = |apps: &[String], app: &str| placed_windows(&cfg, &clients, apps, app).iter().map(|c| c.address.clone()).collect::<Vec<_>>();
        // Workspace описывает и семейство, и вариант: окно варианта ставит
        // его собственная запись, семейство его не трогает.
        let both = ["chromium".to_string(), "chromium-mail".to_string()];
        assert_eq!(addrs(&both, "chromium"), vec!["0x1"]);
        assert_eq!(addrs(&both, "chromium-mail"), vec!["0x2"]);
        // Workspace описывает только семейство: окно варианта встаёт в его ячейку.
        let only_family = ["chromium".to_string()];
        assert_eq!(addrs(&only_family, "chromium"), vec!["0x1", "0x2"]);
    }

    #[test]
    fn adopt_plan_takes_all_fitting_windows() {
        let cfg = Config::parse(CFG).unwrap();
        let apps: Vec<String> = ["herdr".to_string(), "chromium".to_string(), "neovide".to_string()].into();
        // Три окна Chromium без тегов получают номера 1—3; окно на special:hidden
        // не захватывается, окно с тегом живого приложения тоже.
        let clients = vec![
            client("0x1", "chromium", "Новости", "1", &[]),
            client("0x2", "Chromium-browser", "Документы", "special:pool", &[]),
            client("0x3", "chromium", "Почта", "3", &[]),
            client("0x4", "chromium", "Скрытое", "special:hidden", &[]),
            client("0x5", "neovide", "[Scratch]", "1", &["app:neovide#1"]),
        ];
        let plan = adopt_plan(&cfg, &clients, &apps).unwrap();
        assert_eq!(plan, vec![(0, "chromium".to_string(), 1), (2, "chromium".to_string(), 2), (1, "chromium".to_string(), 3)]);

        // Захват работает и у приложения, окна которого уже есть.
        let with_window = vec![client("0x1", "chromium", "Новости", "1", &["app:chromium#1"]), client("0x2", "chromium", "Документы", "1", &[])];
        let plan = adopt_plan(&cfg, &with_window, &apps).unwrap();
        assert_eq!(plan, vec![(1, "chromium".to_string(), 2)]);

        // Окно с тегом приложения, которого нет в конфиге, переходит к приложению.
        let stale = vec![client("0x1", "neovide", "[Scratch]", "1", &["app:editor-dots#1"])];
        assert_eq!(adopt_plan(&cfg, &stale, &apps).unwrap(), vec![(0, "neovide".to_string(), 1)]);
    }

    #[test]
    fn adopt_plan_prefers_variant_over_family() {
        let cfg = Config::parse(CFG).unwrap();
        let clients = vec![client("0x1", "chromium", "Gmail — Входящие", "1", &[]), client("0x2", "chromium", "Новости", "1", &[])];
        // Workspace описывает и семейство, и вариант.
        let apps: Vec<String> = ["chromium".to_string(), "chromium-mail".to_string()].into();
        let plan = adopt_plan(&cfg, &clients, &apps).unwrap();
        assert_eq!(plan, vec![(0, "chromium-mail".to_string(), 1), (1, "chromium".to_string(), 1)]);
        // Workspace описывает только семейство: окно варианта всё равно достаётся
        // варианту и попадает в ячейку семейства как окно своего семейства.
        let plan = adopt_plan(&cfg, &clients, &["chromium".to_string()]).unwrap();
        assert_eq!(plan, vec![(0, "chromium-mail".to_string(), 1), (1, "chromium".to_string(), 1)]);
        // Окно, подходящее двум вариантам одного семейства, достаётся первому по имени.
        let two = format!("{CFG}\n[apps.chromium-docs]\nfamily = \"chromium\"\ncmd = \"chromium\"\nclass = \"(?i)^chromium(-browser)?$\"\ntitle = \"Gmail\"\n");
        let cfg = Config::parse(&two).unwrap();
        let plan = adopt_plan(&cfg, &clients[..1], &["chromium".to_string()]).unwrap();
        assert_eq!(plan, vec![(0, "chromium-docs".to_string(), 1)]);
    }

    #[test]
    fn swap_cells_exchanges_stacks() {
        let cfg = Config::parse(CFG).unwrap();
        let mon = (3840, 2160);
        let mut st = State::default();
        swap_cells(st.cells_of(&cfg, "work", mon), Some("herdr"), "chromium", "center");
        // Стопки меняются ячейками целиком, третье приложение не двигается.
        assert_eq!(st.rect_for(&cfg, "work", "chromium", mon), Some(PxRect { x: 1125, y: 10, w: 1920, h: 2140 }));
        assert_eq!(st.rect_for(&cfg, "work", "herdr", mon), Some(PxRect { x: -805, y: 10, w: 1920, h: 2140 }));
        assert_eq!(st.rect_for(&cfg, "work", "neovide", mon), Some(PxRect { x: 3055, y: 10, w: 1920, h: 2140 }));
        // Приложение вне ячеек: прежнее главное остаётся без назначения.
        let mut st = State::default();
        swap_cells(st.cells_of(&cfg, "work", mon), Some("herdr"), "wezterm", "center");
        assert_eq!(st.cells_of(&cfg, "work", mon).get("herdr"), None);
        // У семейства без cmd запускать нечего: место остаётся пустым, демон пишет в журнал.
        assert!(cfg.app_command(None, &cfg.apps["wezterm"]).is_none());
    }

    #[test]
    fn place_positions_without_reserved() {
        let full = PxRect { x: 0, y: 0, w: 3840, h: 2160 };
        assert_eq!(place_rect(full, 5, "bottom-left").unwrap(), PxRect { x: 10, y: 1085, w: 1905, h: 1065 });
        assert_eq!(place_rect(full, 5, "top-center").unwrap(), PxRect { x: 967, y: 10, w: 1905, h: 1065 });
    }
}
