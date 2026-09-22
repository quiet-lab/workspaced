//! Демон: владелец модели. Слушает события Hyprland, выполняет команды клиентов
//! и панели, запускает приложения, расставляет окна, пишет сессию `default`.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::{Config, Mode, Placement, PxRect, config_path};
use crate::hypr::{self, Client, Event, Hypr};
use crate::session;
use crate::state::{Cycle, ExtraApp, Foreign, Place, State};

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

/// Запущенное приложение, чьё окно ещё не появилось. Запись живёт до окна:
/// сроков у ожидания нет, всё решают события — появление окна и выход процесса.
struct Pending {
    app: String,
    workspace: Option<String>,
    desktop: u8,
    pid: u32,
    /// Исполняемый файл: `which` команды, затем `/proc/<pid>/exe`, пока процесс
    /// жив (обёртка-скрипт вроде `/usr/bin/firefox` выполняет настоящий
    /// бинарник); уточняется при появлении окна.
    exe: Option<PathBuf>,
    /// Имя команды для мягкого сопоставления с классом окна.
    cmd_base: String,
    /// Выражения `class` и `title` приложения: окно из контейнера (distrobox,
    /// podman) не потомок запущенного процесса, его узнают только по ним.
    class_re: Option<regex::Regex>,
    title_re: Option<regex::Regex>,
    /// Процесс вышел, а окна не было: запись ждёт первое подходящее окно,
    /// а повторное нажатие клавиши приложения запускает его заново.
    exited: bool,
    target: Target,
    focus: bool,
}

impl Pending {
    /// Похоже ли окно на результат этого запуска.
    fn matches(&self, c: &Client, ancestors: &[i32]) -> bool {
        if ancestors.contains(&(self.pid as i32)) {
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
/// Ожидание живёт до окна или до выхода запущенного процесса.
#[derive(Debug, Clone)]
pub struct ExpectedForeign {
    pub cmd: Vec<String>,
    pub cwd: Option<String>,
    pub desktop: Option<u8>,
    pub rect: PxRect,
    /// Процесс, запущенный ради этого окна.
    pub pid: u32,
}

enum Msg {
    Event(Event),
    Request { line: String, reply: Sender<String> },
    Subscribe(UnixStream),
    ConfigChanged,
    /// Запущенный демоном процесс завершился.
    ChildExited { pid: u32 },
}

pub struct Daemon {
    /// Конфиг файла вместе с дополнительными приложениями сессии.
    cfg: Config,
    /// Конфиг как прочитан из файла, без дополнительных приложений сессии.
    cfg_file: Config,
    cfg_text: String,
    cfg_path: PathBuf,
    expected: Vec<ExpectedForeign>,
    hypr: Hypr,
    st: State,
    mon: (i32, i32),
    current: u8,
    pending: Vec<Pending>,
    subs: Vec<UnixStream>,
    /// Отправитель сообщений демону: нужен потокам, ждущим выхода процессов.
    tx: Sender<Msg>,
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
    let mut d = Daemon { cfg: cfg.clone(), cfg_file: cfg, cfg_text, cfg_path, expected: Vec::new(), hypr, st: State::default(), mon, current, pending: Vec::new(), subs: Vec::new(), tx: tx.clone(), maximized: HashMap::new() };
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
            Msg::ChildExited { pid } => {
                if let Err(e) = d.on_child_exit(pid) {
                    log::warn!("выход процесса {pid}: {e:#}");
                }
            }
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

/// Завершена ли этим событием запись конфига. Учитываются только события,
/// означающие готовый файл: закрытие файла, открытого на запись (правка
/// на месте — `nvim`, `tee`), и появление файла под своим именем после записи
/// во временный файл с переименованием (`chezmoi apply`). Промежуточные
/// `Modify(Data)` приходят посреди записи и дали бы перечитывание неполного
/// файла. `Create` не учитывается: за созданием файла всегда следует закрытие.
/// `Modify(Name(Both))` тоже не учитывается: на одно переименование в
/// наблюдаемом каталоге `notify` присылает и `To`, и парный `Both`, и учёт
/// обоих дал бы два перечитывания на одно сохранение.
fn write_finished(kind: &notify::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind, RenameMode};
    matches!(kind, EventKind::Access(AccessKind::Close(AccessMode::Write)) | EventKind::Modify(ModifyKind::Name(RenameMode::To)))
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
    // Следим за каталогом: редакторы пишут через временный файл и переименование,
    // при котором у файла меняется inode и наблюдение за ним самим теряется.
    if let Some(dir) = path.parent()
        && let Err(e) = watcher.watch(dir, RecursiveMode::NonRecursive)
    {
        log::error!("наблюдатель конфига: {e}");
        return;
    }
    for ev in wrx {
        let touches = ev.paths.iter().any(|p| p.file_name() == path.file_name());
        if touches && write_finished(&ev.kind) {
            let _ = tx.send(Msg::ConfigChanged);
        }
    }
}

/// Ждать выхода запущенного процесса в отдельном потоке и сообщить о нём
/// демону. Так выход процесса становится событием, и опрос `try_wait`
/// по таймеру не нужен; заодно поток забирает код возврата, и процесса-зомби
/// не остаётся.
fn watch_child(mut child: Child, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let pid = child.id();
        let _ = child.wait();
        let _ = tx.send(Msg::ChildExited { pid });
    });
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
        self.save_session_file();
        Ok(())
    }

    pub fn reload_config(&mut self) {
        match Config::load(&self.cfg_path) {
            Ok(cfg) => {
                let old = std::mem::replace(&mut self.cfg_file, cfg);
                self.cfg_text = std::fs::read_to_string(&self.cfg_path).unwrap_or_default();
                // Назначения ячеек снимаются у workspace, чей раздел изменился
                // или исчез: правка файла возвращает расстановку, а обмен ячеек
                // в нетронутых workspace сохраняется.
                keep_cells(&old, &self.cfg_file, &mut self.st.cells);
                self.rebuild_cfg();
                self.drop_stale_pending();
                self.free_stale_tagged();
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

    /// Пересобрать эффективный конфиг: файл плюс дополнительные приложения
    /// сессии (спецификация ws-sessions, «Дополнительные приложения сессии»).
    /// Каждое такое приложение попадает в `apps` и в таблицу `apps` своего
    /// workspace, поэтому поднятие, цепочка, парковка, расстановка и состояние
    /// для панели обращаются с ним как с приложением конфига.
    pub fn rebuild_cfg(&mut self) {
        let (cfg, dropped) = merge_extra(&self.cfg_file, &self.st.extra);
        for (ws, name, reason) in dropped {
            log::warn!("дополнительное приложение сессии {name} (workspace {ws}) снято: {reason}");
            if let Some(apps) = self.st.extra.get_mut(&ws) {
                apps.remove(&name);
            }
            if let Some(cells) = self.st.cells.get_mut(&ws) {
                cells.remove(&name);
            }
        }
        self.st.extra.retain(|_, apps| !apps.is_empty());
        // У workspace, назначения которого уже собраны, дополнительное
        // приложение иначе осталось бы без места. Места приложений, уже
        // описанных в workspace, не трогаются.
        let extra = self.st.extra.clone();
        for (ws, apps) in &extra {
            let Some(cells) = self.st.cells.get_mut(ws) else { continue };
            for (name, e) in apps {
                cells.entry(name.clone()).or_insert(Place::Rect { rect: e.rect });
            }
        }
        self.cfg = cfg;
    }

    /// Снять ожидания окон для приложений, которых в эффективном конфиге
    /// больше нет: ждать такое окно некому и незачем, а запись с мёртвым pid
    /// иначе остаётся в `pending` до перезапуска демона. Запущенный процесс
    /// не убивается — если его окно всё-таки появится, оно придёт посторонним.
    /// Вызывается там же, где освобождаются окна снятых приложений.
    pub fn drop_stale_pending(&mut self) {
        let apps: Vec<String> = self.pending.iter().map(|p| p.app.clone()).collect();
        let stale = stale_pending(&self.cfg, &apps);
        for app in &stale {
            log::info!("ожидание окна {app} снято: приложения в конфиге больше нет");
        }
        self.pending.retain(|p| !stale.contains(&p.app));
    }

    /// Освободить окна, чей тег называет приложение, которого в эффективном
    /// конфиге больше нет (спецификация ws-daemon, «Захват открытых окон
    /// приложения»): тег экземпляра снимается в композиторе, окно заводится
    /// как постороннее. Вызывается после каждой пересборки эффективного
    /// конфига, потому что именно она решает, какие приложения существуют.
    pub fn free_stale_tagged(&mut self) {
        let clients = match self.hypr.clients() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("сверка тегов окон с конфигом: {e:#}");
                return;
            }
        };
        let stale: Vec<Client> = stale_tagged(&self.cfg, &clients).into_iter().cloned().collect();
        for c in &stale {
            let app = c.app().unwrap_or_default();
            let ex: Vec<String> = c.app_tags().map(|t| hypr::d_untag(&c.address, t)).collect();
            if let Err(e) = self.hypr.dispatch_all(&ex) {
                log::warn!("окно {} ({}): тег приложения {app} не снят: {e:#}", c.address, c.class);
                continue;
            }
            let (cmd, cwd) = proc_info(c.pid);
            self.st.foreign.insert(c.address.clone(), Foreign { rect: c.rect(), cmd, cwd });
            log::info!("окно {} ({}): приложения {app} в конфиге больше нет, окно свободно", c.address, c.class);
        }
    }

    pub fn cfg(&self) -> &Config {
        &self.cfg
    }
    /// Конфиг как прочитан из файла: нужен там, где дополнительные приложения
    /// сессии учитывать нельзя (сверка списка workspace со снимком).
    pub fn cfg_file(&self) -> &Config {
        &self.cfg_file
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
        self.expected.push(e);
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

    /// Записать снимок сессии `default`. Вызывается в конце обработки события
    /// или команды, изменивших состояние, — до ответа клиенту, поэтому
    /// к возврату команды файл уже обновлён.
    pub fn save_session_file(&mut self) {
        if let Err(e) = session::save_default(self) {
            log::warn!("запись сессии default: {e:#}");
        }
    }

    // ---- События ----------------------------------------------------------------

    fn on_event(&mut self, ev: Event) -> Result<()> {
        let changed = match ev {
            Event::OpenWindow { addr, .. } => {
                self.on_open(&addr)?;
                true
            }
            Event::CloseWindow { addr } => {
                self.st.foreign.remove(&addr);
                self.forget_window(&addr);
                self.broadcast();
                true
            }
            Event::MoveWindow { .. } => {
                self.broadcast();
                true
            }
            Event::Workspace { name } => match name.parse::<u8>() {
                Ok(n) if (1..=8).contains(&n) => {
                    self.current = n;
                    if let Some(ws) = self.st.lazy.remove(&n) {
                        log::info!("стол {n}: ленивое поднятие {ws}");
                        if let Err(e) = self.raise(&ws, Some(n)) {
                            log::warn!("ленивое поднятие {ws}: {e:#}");
                        }
                    }
                    self.broadcast();
                    true
                }
                _ => false,
            },
            Event::ActiveWindow { .. } => {
                // Последнее окно workspace, получавшее фокус: из него берётся
                // prev, когда цикл начинается с чужого окна. Состояние окон
                // при этом не меняется, поэтому снимок сессии не пишется.
                if let Err(e) = self.on_focus() {
                    log::warn!("смена фокуса: {e:#}");
                }
                false
            }
            Event::Other(_) => false,
        };
        // Снимок сессии пишет обработчик события, а не отложенная запись
        // по таймеру: одна запись на событие, изменившее состояние.
        if changed {
            self.save_session_file();
        }
        Ok(())
    }

    /// Новое окно: либо ожидаемое приложение, либо постороннее.
    fn on_open(&mut self, addr: &str) -> Result<()> {
        let clients = self.hypr.clients()?;
        let Some(c) = clients.iter().find(|c| c.address == *addr).cloned() else { return Ok(()) };
        if c.app().is_some() {
            self.broadcast();
            return Ok(());
        }
        let ancestors = ancestors(c.pid);
        // Исполняемый файл уточняется здесь, а не опросом: обёртка-скрипт
        // (`/usr/bin/firefox`) к появлению окна уже выполнила настоящий бинарник.
        for p in self.pending.iter_mut().filter(|p| !p.exited) {
            if let Some(e) = proc_exe(p.pid as i32) {
                p.exe = Some(e);
            }
        }
        let hit = self
            .pending
            .iter()
            .position(|p| ancestors.contains(&(p.pid as i32)))
            .or_else(|| self.pending.iter().position(|p| p.matches(&c, &ancestors)));
        if let Some(i) = hit {
            let p = self.pending.remove(i);
            self.adopt(&c, p)?;
        } else {
            let (cmd, cwd) = proc_info(c.pid);
            if let Some(i) = self.expected.iter().position(|e| e.cmd == cmd && e.cwd == cwd) {
                // Постороннее окно из снимка: на своё место.
                let e = self.expected.remove(i);
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
            } else if let Some(app) = app_for_window(&self.cfg, &c) {
                // Окно приложения конфига: захват сразу, не дожидаясь
                // поднятия workspace или вызова приложения (design D16).
                self.capture_new(&c, &app, &clients)?;
            } else {
                // Постороннее окно остаётся свободным: частью workspace его
                // делает только команда сохранения — сессии или конфига.
                self.st.foreign.insert(c.address.clone(), Foreign { rect: c.rect(), cmd, cwd });
                log::info!("постороннее окно {} ({}) свободно", c.address, c.class);
            }
        }
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
                let r = rect.unwrap_or_else(|| center_rect(c, self.mon));
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

    /// Окно приложения конфига, открытое не демоном (design D16): пользователь
    /// нажал Ctrl+N в браузере, приложение открыло второе окно само. Окно сразу
    /// получает тег экземпляра и встаёт на место своего приложения в активном
    /// workspace стола, поверх уже открытых окон приложения. На другой стол
    /// демон его не переносит и фокус не трогает: фокус новому окну уже отдал
/// композитор, а отбирать его у окна, которое пользователь только что открыл,
/// незачем.
    fn capture_new(&mut self, c: &Client, app: &str, clients: &[Client]) -> Result<()> {
        let cfg = self.cfg.clone();
        let num = free_instance(clients, app);
        let mut ex = vec![hypr::d_tag(&c.address, &format!("app:{app}#{num}"))];
        // Место ищется только для окна на обычном столе: окно, открытое сразу
        // на `special:pool` или `special:hidden`, остаётся там, где открылось.
        let ws = c.desktop().and_then(|n| self.st.desktops.get(&n).and_then(|d| d.active.clone()));
        let rect = c.desktop().and_then(|_| open_rect(&mut self.st, &cfg, ws.as_deref(), app, self.mon));
        if let Some(r) = rect {
            ex.extend(hypr::d_place(&c.address, r));
        }
        // В режиме `stack` прямоугольник окна запоминается сразу (design D9):
        // ушедшее на `special:pool` и вернувшееся окно встанет туда же.
        if let Some(w) = ws.filter(|w| cfg.workspaces.get(w).map(|x| x.mode()) == Some(Mode::Stack)) {
            self.st.geom.entry(w).or_default().insert(c.address.clone(), rect.unwrap_or_else(|| c.rect()));
        }
        log::info!("новое окно {} ({}, «{}») → приложение {app}, экземпляр {num}", c.address, c.class, c.title);
        self.hypr.dispatch_all(&ex)
    }

    /// Свободное окно, подходящее ожиданию запуска номер `i`.
    fn free_window_for(&self, i: usize) -> Result<Option<Client>> {
        Ok(self.hypr.clients()?.into_iter().find(|c| c.app().is_none() && self.pending[i].matches(c, &[])))
    }

    /// Выход запущенного демоном процесса. Сроков у ожидания нет: запись
    /// `pending` живёт до окна. Если к моменту выхода подходящее свободное окно
    /// уже есть (одноэкземплярное приложение открыло его в прежнем процессе),
    /// оно принимается сразу; иначе запись переходит в состояние «процесс вышел,
    /// окно ожидается» и ждёт первого подходящего `openwindow`. Ожидание
    /// постороннего окна из снимка выхода процесса не переживает: восстановить
    /// его уже нечем.
    fn on_child_exit(&mut self, pid: u32) -> Result<()> {
        if let Some(i) = self.expected.iter().position(|e| e.pid == pid) {
            let e = self.expected.remove(i);
            log::warn!("восстановление окна {:?}: процесс завершился, окна нет", e.cmd);
            return Ok(());
        }
        let Some(i) = self.pending.iter().position(|p| p.pid == pid) else { return Ok(()) };
        let found = self.free_window_for(i)?;
        match pending_step(true, found.is_some(), false) {
            PendingStep::Adopt => {
                let c = found.expect("окно найдено");
                let p = self.pending.remove(i);
                log::info!("{}: процесс завершился без окна, принято окно {} ({})", p.app, c.address, c.class);
                self.st.foreign.remove(&c.address);
                self.adopt(&c, p)?;
                self.save_session_file();
                self.broadcast();
            }
            _ => {
                self.pending[i].exited = true;
                log::info!("{}: процесс завершился, окна пока нет — ждём его появления", self.pending[i].app);
            }
        }
        Ok(())
    }

    // ---- Запуск -----------------------------------------------------------------

    fn spawn(&mut self, app: &str, workspace: Option<&str>, desktop: u8, target: Target, focus: bool) -> Result<()> {
        // Пока запуск идёт, повторная клавиша второго экземпляра не заводит.
        // Если процесс уже вышел, а окна так и нет, клавиша не должна остаться
        // без ответа: принимается подходящее свободное окно, а когда его нет —
        // прежнее ожидание снимается и приложение запускается заново.
        if let Some(i) = self.pending.iter().position(|p| p.app == app) {
            let exited = self.pending[i].exited;
            let found = if exited { self.free_window_for(i)? } else { None };
            match pending_step(exited, found.is_some(), true) {
                PendingStep::Wait => return Ok(()),
                PendingStep::Adopt => {
                    let c = found.expect("окно найдено");
                    let p = self.pending.remove(i);
                    log::info!("{app}: принято уже открытое окно {} ({})", c.address, c.class);
                    self.st.foreign.remove(&c.address);
                    return self.adopt(&c, p);
                }
                _ => {
                    self.pending.remove(i);
                    log::info!("{app}: прежний запуск окна не дал, запускаем заново");
                }
            }
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
        let pid = child.id();
        watch_child(child, self.tx.clone());
        let exe = which(&cmd).and_then(|p| p.canonicalize().ok());
        let cmd_base = Path::new(&cmd).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let (class_re, title_re) = a.matchers()?.map_or((None, None), |(c, t)| (Some(c), t));
        log::info!("{app}: запущен pid {pid} ({cmd} {})", args.join(" "));
        self.pending.push(Pending { app: app.to_string(), workspace: workspace.map(String::from), desktop, pid, exe, cmd_base, class_re, title_re, exited: false, target, focus });
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
            let mut ex: Vec<String> = clients[i].app_tags().map(|t| hypr::d_untag(&addr, t)).collect();
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
        // Ленивое поднятие для этого стола больше не нужно: workspace там
        // поднимает эта команда. Иначе событие смены стола пришло бы следом
        // и отдало фокус главному окну поверх того, что сделала команда.
        self.st.lazy.remove(&n);
        let w = self.cfg.workspaces[ws].clone();
        let apps: Vec<String> = w.apps.keys().cloned().collect();
        let stack = w.mode() == Mode::Stack;
        let mut clients = self.hypr.clients()?;
        self.adopt_untagged(&mut clients, &apps)?;
        // Вытесняемый workspace запоминает, где оставлены его окна: в режиме
        // `stack` следующее поднятие вернёт их именно туда.
        if let Some(old) = self.st.desktops.get(&n).and_then(|d| d.active.clone()).filter(|a| a != ws) {
            self.remember_geometry(&old, &clients);
        }
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
                let moving = c.desktop() != Some(n);
                if moving {
                    ex.push(hypr::d_move_to(&c.address, &n.to_string()));
                }
                if stack {
                    // Окно, уже стоящее на столе, поднятие не двигает: оно там,
                    // где его оставил пользователь. Вернувшееся окно встаёт
                    // в запомненный прямоугольник, а без него — на место из конфига.
                    let kept = self.st.geom.get(ws).and_then(|g| g.get(&c.address)).copied();
                    let target = if moving { kept.or(rect) } else { None };
                    if let Some(r) = target {
                        ex.extend(hypr::d_place(&c.address, r));
                    }
                    self.st.geom.entry(ws.to_string()).or_default().insert(c.address.clone(), target.unwrap_or_else(|| c.rect()));
                } else if let Some(r) = rect {
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
        self.broadcast();
        Ok(())
    }

    /// Последнее окно workspace, получавшее фокус. Берётся из события
    /// `activewindow`: опроса нет, сведение приходит от композитора. Из него
    /// выводится prev, когда цикл начинается с чужого окна.
    fn on_focus(&mut self) -> Result<()> {
        let Some(c) = self.hypr.active_window()? else { return Ok(()) };
        let Some(n) = c.desktop() else { return Ok(()) };
        let Some(ws) = self.st.desktops.get(&n).and_then(|d| d.active.clone()) else { return Ok(()) };
        let ws_apps = self.ws_apps(&ws);
        if ws_app_of(&self.cfg, &ws_apps, &c).is_some() {
            self.st.focus.insert(ws, c.address);
        }
        Ok(())
    }

    /// Забыть закрытое окно: запомненный прямоугольник, последний фокус
    /// workspace и окно возврата незаконченного цикла.
    fn forget_window(&mut self, addr: &str) {
        for g in self.st.geom.values_mut() {
            g.remove(addr);
        }
        self.st.focus.retain(|_, a| a != addr);
        for c in self.st.cycle.values_mut() {
            if c.back.as_deref() == Some(addr) {
                c.back = None;
            }
        }
    }

    /// Приложения workspace по порядку записи в его разделе.
    fn ws_apps(&self, ws: &str) -> Vec<String> {
        self.cfg.workspaces.get(ws).map(|w| w.apps.keys().cloned().collect()).unwrap_or_default()
    }

    /// Запомнить, где стоят окна workspace, прежде чем увести их со стола:
    /// в режиме `stack` поднятие вернёт их именно туда. В режиме обмена ячеек
    /// место задаёт конфиг, и запоминать нечего.
    fn remember_geometry(&mut self, ws: &str, clients: &[Client]) {
        if self.cfg.workspaces.get(ws).map(|w| w.mode()) != Some(Mode::Stack) {
            return;
        }
        let cfg = self.cfg.clone();
        let apps = self.ws_apps(ws);
        for c in clients.iter().filter(|c| c.desktop().is_some()) {
            if c.app().is_some_and(|a| apps.iter().any(|x| cfg.app_is(&a, x))) {
                self.st.geom.entry(ws.to_string()).or_default().insert(c.address.clone(), c.rect());
            }
        }
    }

    /// Обмен ячеек: приложение занимает главную ячейку, прежнее главное — его.
    /// Стопки меняются целиком, остальные окна не двигаются. Диспетчеры
    /// дописываются в `ex`, чтобы обмен и фокус ушли одной последовательностью.
    fn swap_to_main(&mut self, ws: &str, app: &str, clients: &[Client], ex: &mut Vec<String>) {
        let cfg = self.cfg.clone();
        let ws_apps = self.ws_apps(ws);
        let main_app = self.st.main_app(&cfg, ws, self.mon);
        if main_app.as_deref() == Some(app) {
            return;
        }
        let Some(main_cell) = cfg.templates.get(&cfg.workspaces[ws].template).map(|t| t.main.clone()) else { return };
        // Обмен виден потом в снимке сессии и в записи workspace, поэтому
        // след в журнале нужен: иначе причину перестановки не восстановить.
        log::info!("{ws}: {app} становится главным, прежнее главное — {}", main_app.as_deref().unwrap_or("нет"));
        swap_cells(self.st.cells_of(&cfg, ws, self.mon), main_app.as_deref(), app, &main_cell);
        let n = self.current;
        for name in [main_app.as_deref(), Some(app)].into_iter().flatten() {
            let Some(r) = self.st.rect_for(&cfg, ws, name, self.mon) else { continue };
            for c in placed_windows(&cfg, clients, &ws_apps, name).iter().filter(|c| !c.on_hidden()) {
                if c.desktop() != Some(n) {
                    ex.push(hypr::d_move_to(&c.address, &n.to_string()));
                }
                ex.extend(hypr::d_place(&c.address, r));
            }
        }
    }

    /// Выбрать окно (спецификация ws-daemon, «Цепочка приложения»): в режиме
    /// обмена ячеек приложение окна сначала занимает главную ячейку, в режиме
    /// `stack` окно только поднимается наверх и получает фокус.
    fn select_window(&mut self, ws: &str, addr: &str, clients: &[Client]) -> Result<()> {
        let cfg = self.cfg.clone();
        let ws_apps = self.ws_apps(ws);
        let mut ex = Vec::new();
        if cfg.workspaces.get(ws).map(|w| w.mode()) == Some(Mode::Swap)
            && let Some(c) = clients.iter().find(|c| c.address == addr)
            && let Some(app) = ws_app_of(&cfg, &ws_apps, c)
        {
            self.swap_to_main(ws, &app, clients, &mut ex);
        }
        ex.push(hypr::d_focus_window(addr));
        ex.push(hypr::d_bring_to_top());
        self.hypr.dispatch_all(&ex)?;
        self.broadcast();
        Ok(())
    }

    /// Цикл по экземплярам приложения в workspace (спецификация ws-daemon,
    /// «Цепочка приложения»). `fresh` означает, что workspace подняла эта же
    /// цепочка: фокус уже отдан главному окну, и цикл начинается заново.
    fn cycle(&mut self, ws: &str, app: &str, fresh: bool) -> Result<()> {
        let mut clients = self.hypr.clients()?;
        self.adopt_untagged(&mut clients, std::slice::from_ref(&app.to_string()))?;
        let cfg = self.cfg.clone();
        let ws_apps = self.ws_apps(ws);
        let wins = placed_windows(&cfg, &clients, &ws_apps, app);
        let order: Vec<String> = wins.iter().filter(|c| !c.on_hidden()).map(|c| c.address.clone()).collect();
        if order.is_empty() && !wins.is_empty() {
            // Все окна приложения спрятаны пользователем на `special:hidden`:
            // второй экземпляр не запускается, показывать нечего.
            log::info!("{app}: все окна скрыты, цикл пропущен");
            return Ok(());
        }
        let active = self.hypr.active_window()?.map(|c| c.address);
        let on_instance = active.as_deref().is_some_and(|a| order.iter().any(|x| x == a));
        if fresh || !on_instance {
            // Начало цикла: запомнить окно, к которому вернёт его конец.
            let main_window = (cfg.workspaces.get(ws).map(|w| w.mode()) == Some(Mode::Swap))
                .then(|| self.st.main_app(&cfg, ws, self.mon))
                .flatten()
                .filter(|m| m != app)
                .and_then(|m| placed_windows(&cfg, &clients, &ws_apps, &m).into_iter().find(|c| !c.on_hidden()).map(|c| c.address.clone()));
            let focus = self.st.focus.get(ws).cloned();
            let back = anchor_window(&cfg, &clients, &ws_apps, main_window.as_deref(), &order, active.as_deref(), focus.as_deref());
            self.st.cycle.insert(ws.to_string(), Cycle { app: app.to_string(), back });
        }
        let prev = self
            .st
            .cycle
            .get(ws)
            .filter(|c| c.app == app)
            .and_then(|c| c.back.clone())
            .filter(|a| clients.iter().any(|c| c.address == *a && !c.on_hidden()));
        let next = next_app_window(&cfg, &clients, ws, app);
        match cycle_step(&order, active.as_deref(), fresh, prev.as_deref(), next.as_deref()) {
            CycleStep::Launch => {
                // Запуск — это тоже выбор экземпляра: в режиме обмена ячеек
                // приложение сначала занимает главную ячейку, и окно появляется
                // уже в ней, а прежнее главное уходит в ячейку приложения.
                let mut ex = Vec::new();
                if cfg.workspaces.get(ws).map(|w| w.mode()) == Some(Mode::Swap) {
                    self.swap_to_main(ws, app, &clients, &mut ex);
                }
                self.hypr.dispatch_all(&ex)?;
                let rect = self.st.rect_for(&cfg, ws, app, self.mon);
                let n = self.current;
                self.spawn(app, Some(ws), n, rect.map(Target::Place).unwrap_or(Target::Free(None)), true)
            }
            CycleStep::Select(addr) => self.select_window(ws, &addr, &clients),
            CycleStep::Back(addr) => {
                log::info!("{ws}: цикл {app} закончен, возврат к окну {addr}");
                self.select_window(ws, &addr, &clients)
            }
            CycleStep::NextApp(addr) => {
                log::info!("{ws}: цикл {app} закончен, prev нет — следующее приложение workspace, окно {addr}");
                self.select_window(ws, &addr, &clients)
            }
        }
    }

    /// Цепочка приложения: цикл по экземплярам в workspace приложения
    /// (спецификация ws-daemon, «Цепочка приложения»). `apps` — кандидаты
    /// с одной цепочкой: действует тот из них, чей workspace найдётся раньше.
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
        // Поднятый этой же цепочкой workspace начинает цикл заново: поднятие
        // уже отдало фокус первому экземпляру главного приложения.
        let mut raised = false;
        if let Some(ws) = workspace {
            self.raise(ws, None)?;
            raised = true;
        }
        let in_ws = |cfg: &Config, ws: &str| -> Option<String> { apps.iter().find(|a| cfg.workspaces.get(ws).is_some_and(|w| w.apps.contains_key(*a))).cloned() };
        let n = self.current;
        // 1. Активный workspace текущего стола.
        if let Some(ws) = self.st.desktop(n).active.clone()
            && let Some(a) = in_ws(&self.cfg, &ws)
        {
            return self.cycle(&ws, &a, raised);
        }
        // 2. Список текущего стола по порядку (workspace, активный на другом
        // столе, поднимается там же, см. raise).
        let list = self.st.desktop(n).workspaces.clone();
        for ws in &list {
            if let Some(a) = in_ws(&self.cfg, ws) {
                self.raise(ws, None)?;
                return self.cycle(ws, &a, true);
            }
        }
        // 3. Другие столы по возрастанию номера.
        let others: Vec<(u8, Vec<String>)> = self.st.desktops.iter().filter(|(k, _)| **k != n).map(|(k, d)| (*k, d.workspaces.clone())).collect();
        for (k, list) in others {
            for ws in &list {
                if let Some(a) = in_ws(&self.cfg, ws) {
                    self.raise(ws, Some(k))?;
                    return self.cycle(ws, &a, true);
                }
            }
        }
        // 3б. Workspace из конфига, который ещё не поднимали ни на одном столе.
        for (ws, _) in self.cfg.workspaces.clone() {
            if let Some(a) = in_ws(&self.cfg, &ws) {
                self.raise(&ws, Some(n))?;
                return self.cycle(&ws, &a, true);
            }
        }
        // 4. Приложение без workspace: цикл по его окнам без prev и без
        // следующего приложения, поэтому за последним экземпляром идёт первый.
        // Окно, которого ещё нет, открывается на своём месте по умолчанию
        // (`rect` приложения, а без него — центр экрана).
        let app = apps[0].clone();
        let mut clients = self.hypr.clients()?;
        self.adopt_untagged(&mut clients, apps)?;
        let wins = app_windows(&self.cfg, &clients, &app);
        let order: Vec<String> = wins.iter().filter(|c| !c.on_hidden()).map(|c| c.address.clone()).collect();
        if order.is_empty()
            && let Some(c) = wins.first()
        {
            // Все окна скрыты пользователем: показываем первое на этом столе.
            let ex = vec![hypr::d_move_to(&c.address, &n.to_string()), hypr::d_focus_window(&c.address), hypr::d_bring_to_top()];
            return self.hypr.dispatch_all(&ex);
        }
        let active = self.hypr.active_window()?.map(|c| c.address);
        match cycle_step(&order, active.as_deref(), false, None, None) {
            CycleStep::Launch => {
                let rect = State::app_rect(&self.cfg, &app, self.mon);
                self.spawn(&app, None, n, Target::Free(rect), true)
            }
            CycleStep::Select(addr) | CycleStep::Back(addr) | CycleStep::NextApp(addr) => {
                self.hypr.dispatch_all(&[hypr::d_focus_window(&addr), hypr::d_bring_to_top()])
            }
        }
    }

    /// Стол, на котором композитор находится сейчас. Поле `current` — только
    /// кэш: его обновляют события `workspacev2` и команды самого демона,
    /// а событие приходит отдельным соединением и обрабатывается в общей
    /// очереди, поэтому событие, отставшее от команды, может вернуть в кэш
    /// стол, с которого демон уже ушёл. Команда, которая действует на «стол,
    /// где пользователь сейчас», MUST отталкиваться от композитора, как
    /// `half`, `place` и `maximize` отталкиваются от активного окна.
    /// Специальный стол (`special:*`) номера не имеет, и кэш тогда остаётся
    /// прежним.
    fn sync_current(&mut self) -> u8 {
        let live = self.hypr.active_workspace().ok().and_then(|w| w.parse::<u8>().ok()).filter(|n| (1..=8).contains(n));
        if let Some(n) = live
            && n != self.current
        {
            log::info!("текущий стол уточнён у композитора: был {}, стал {n}", self.current);
            self.current = n;
        }
        self.current
    }

    /// Перенести активный workspace текущего стола на стол `n` и перейти туда
    /// (спецификация ws-daemon, «Перенос workspace на стол»). Перенос — это
    /// поднятие с явным столом, поэтому остальное делает `raise`: окна всех
    /// приложений переезжают, а workspace, активный на столе `n`, сворачивается
    /// на `special:pool` по общему правилу.
    pub fn move_desktop(&mut self, n: u8) -> Result<()> {
        if !(1..=8).contains(&n) {
            bail!("стол должен быть от 1 до 8, получено {n}");
        }
        let cur = self.sync_current();
        let active = self.st.desktops.get(&cur).and_then(|d| d.active.clone());
        match move_step(cur, n, active.as_deref()) {
            MoveStep::Nothing => {
                log::info!("перенос на стол {n}: на столе {cur} нет активного workspace");
                Ok(())
            }
            MoveStep::Here(ws) => {
                log::info!("перенос на стол {n}: workspace {ws} уже на этом столе");
                Ok(())
            }
            MoveStep::Move(ws) => {
                // Окна запоминают, где их оставили: в режиме `stack` перенос
                // не отменяет сдвиг окна мышью, и на новом столе окно встаёт
                // туда, где стояло.
                let clients = self.hypr.clients()?;
                self.remember_geometry(&ws, &clients);
                log::info!("перенос workspace {ws} со стола {cur} на стол {n}");
                self.raise(&ws, Some(n))
            }
        }
    }

    /// Расставить окна текущего стола по описанию активного workspace
    /// (спецификация ws-daemon, «Расстановка по команде»). Назначения ячеек
    /// команда не меняет: окна встают по тем местам, которые назначены сейчас.
    fn arrange(&mut self) -> Result<()> {
        // Расстановка тоже действует на стол, где пользователь сейчас,
        // поэтому стол берётся у композитора, а не из кэша.
        let n = self.sync_current();
        let cfg = self.cfg.clone();
        let ws = self.st.desktops.get(&n).and_then(|d| d.active.clone());
        let clients = self.hypr.clients()?;
        // Порядок появления даёт номер окна у композитора: по нему выбирается
        // первое окно класса, на место которого встаёт вся стопка свободных окон.
        let mut windows: Vec<&Client> = clients.iter().filter(|c| c.desktop() == Some(n)).collect();
        windows.sort_by_key(|c| c.stable());
        if windows.is_empty() {
            log::info!("расстановка на столе {n}: окон нет");
            return Ok(());
        }
        let plan = arrange_plan(&mut self.st, &cfg, ws.as_deref(), &windows, self.mon);
        let mut ex: Vec<String> = plan.iter().flat_map(|(addr, r)| hypr::d_place(addr, *r)).collect();
        // Фокус остаётся у активного окна, а само окно поднимается наверх своей стопки.
        if let Some(a) = self.hypr.active_window()?.filter(|c| c.desktop() == Some(n)).map(|c| c.address) {
            ex.push(hypr::d_focus_window(&a));
            ex.push(hypr::d_bring_to_top());
        }
        self.hypr.dispatch_all(&ex)?;
        log::info!("расстановка на столе {n}: окон {}, workspace {}", plan.len(), ws.as_deref().unwrap_or("нет"));
        // В режиме `stack` запомненные прямоугольники сменяются местами
        // из конфига: вернуть окна к описанию — и есть смысл команды.
        if let Some(w) = ws.filter(|w| cfg.workspaces.get(w).map(|x| x.mode()) == Some(Mode::Stack)) {
            let ws_apps = self.ws_apps(&w);
            for (addr, r) in &plan {
                if clients.iter().any(|c| c.address == *addr && ws_app_of(&cfg, &ws_apps, c).is_some()) {
                    self.st.geom.entry(w.clone()).or_default().insert(addr.clone(), *r);
                }
            }
        }
        self.broadcast();
        Ok(())
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
            self.remember_geometry(ws, &clients);
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
    pub fn spawn_foreign(&mut self, cmd: &[String], cwd: Option<&str>) -> Result<u32> {
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
        let child = command.spawn().with_context(|| format!("не удалось запустить {prog}"))?;
        let pid = child.id();
        watch_child(child, self.tx.clone());
        Ok(pid)
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
        let changes = changes_state(cmd, s("op").as_deref());
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
            "move-desktop" => match desktop {
                Some(n) => self.move_desktop(n).map(|_| json!({"ok": true})),
                None => Err(anyhow::anyhow!("нет desktop")),
            },
            "arrange" => self.arrange().map(|_| json!({"ok": true})),
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
            "save-session" => crate::save::save_session(self).map(|(ws, n)| json!({"ok": true, "workspace": ws, "adopted": n})),
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
            Ok(v) => {
                // Снимок сессии пишется здесь, до ответа клиенту: к возврату
                // команды файл уже обновлён.
                if changes {
                    self.save_session_file();
                }
                v
            }
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
        let pending: Vec<Value> = self.pending.iter().map(|p| json!({ "app": p.app, "pid": p.pid, "desktop": p.desktop })).collect();
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

/// Имя юнита systemd с раскодированными последовательностями `\xNN`:
/// точку и прочие особые символы systemd записывает именно так.
fn unescape_unit(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1)) {
            (b'\\', Some(b'x')) if i + 4 <= b.len() && u8::from_str_radix(&s[i + 2..i + 4], 16).is_ok() => {
                out.push(u8::from_str_radix(&s[i + 2..i + 4], 16).unwrap());
                i += 4;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Идентификатор приложения flatpak по содержимому `/proc/<pid>/cgroup`.
/// Процесс в песочнице лежит в юните вида `app-flatpak-<идентификатор>-<номер>.scope`;
/// хвост `-<номер>` добавляет systemd, поэтому он отбрасывается. Если юнита
/// такого вида нет, процесс работает не в песочнице.
pub fn flatpak_app_id(cgroup: &str) -> Option<String> {
    let scope = cgroup.split(['/', '\n']).find(|p| p.starts_with("app-flatpak-") && p.ends_with(".scope"))?;
    let body = scope.strip_prefix("app-flatpak-")?.strip_suffix(".scope")?;
    let id = match body.rsplit_once('-') {
        Some((id, num)) if !id.is_empty() && !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) => id,
        _ => body,
    };
    let id = unescape_unit(id);
    (!id.is_empty()).then_some(id)
}

/// Командная строка и каталог процесса окна. У процесса в песочнице flatpak
/// путь к исполняемому файлу (`/app/…`) и рабочий каталог существуют только
/// внутри песочницы: с хоста по ним ничего не запустить, и окно из снимка
/// сессии не восстановилось бы. Поэтому командной строкой такого окна
/// считается `flatpak run <идентификатор>` с исходными аргументами процесса,
/// а каталог не записывается.
pub fn proc_info(pid: i32) -> (Vec<String>, Option<String>) {
    let cmd: Vec<String> = std::fs::read(format!("/proc/{pid}/cmdline")).map(|b| b.split(|&x| x == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()).unwrap_or_default();
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned());
    if let Some(id) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok().and_then(|c| flatpak_app_id(&c)) {
        let mut out = vec!["flatpak".to_string(), "run".to_string(), id];
        out.extend(cmd.into_iter().skip(1));
        return (out, None);
    }
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

/// Эффективный конфиг из файла и дополнительных приложений сессии. Запись
/// с командой описывает и приложение, и его место в workspace; запись без
/// команды называет приложение конфига и задаёт ему только место. Вторым
/// значением возвращается перечень снятых записей: workspace, имя и причина.
/// Снимаются записи, чей workspace из файла исчез, чьё имя занято приложением
/// конфига (конфиг главнее), чьё приложение конфига пропало и чьё место конфиг
/// теперь задаёт сам.
#[allow(clippy::type_complexity)]
pub fn merge_extra(file: &Config, extra: &BTreeMap<String, BTreeMap<String, ExtraApp>>) -> (Config, Vec<(String, String, String)>) {
    let mut cfg = file.clone();
    let mut dropped = Vec::new();
    for (ws, apps) in extra {
        for (name, e) in apps {
            let known = file.apps.contains_key(name);
            let reason = match file.workspaces.get(ws) {
                None => Some(format!("workspace {ws} из конфига исчез")),
                Some(_) if e.cmd.is_empty() && !known => Some("приложения с таким именем в конфиге больше нет".to_string()),
                Some(_) if !e.cmd.is_empty() && known => Some("приложение с таким именем описано в конфиге, запись конфига главнее".to_string()),
                Some(w) if w.apps.contains_key(name) => Some(format!("место приложения задано в разделе workspace {ws}")),
                Some(_) => None,
            };
            match reason {
                Some(r) => dropped.push((ws.clone(), name.clone(), r)),
                None => {
                    if !e.cmd.is_empty() {
                        cfg.apps.insert(name.clone(), e.to_app());
                    }
                    if let Some(w) = cfg.workspaces.get_mut(ws) {
                        w.apps.insert(name.clone(), Placement::Rect { rect: e.rect.to_rect() });
                    }
                }
            }
        }
    }
    (cfg, dropped)
}

/// Что делать с ожиданием запуска приложения.
#[derive(Debug, PartialEq, Eq)]
pub enum PendingStep {
    /// Запуск идёт: ждать окна, второго экземпляра не заводить.
    Wait,
    /// Принять уже открытое подходящее окно.
    Adopt,
    /// Запомнить, что процесс вышел, и ждать первого подходящего окна.
    MarkExited,
    /// Снять ожидание и запустить приложение заново.
    Restart,
}

/// Решение по уже заведённому ожиданию запуска. `exited` — процесс вышел,
/// `window` — в системе есть подходящее свободное окно, `key` — решение
/// принимается по нажатию клавиши приложения, иначе по выходу процесса.
/// Сроков в этой схеме нет: клавиша не остаётся без ответа, потому что после
/// выхода процесса она либо принимает окно, либо запускает приложение заново.
pub fn pending_step(exited: bool, window: bool, key: bool) -> PendingStep {
    match (key, exited, window) {
        (false, _, true) => PendingStep::Adopt,
        (false, _, false) => PendingStep::MarkExited,
        (true, false, _) => PendingStep::Wait,
        (true, true, true) => PendingStep::Adopt,
        (true, true, false) => PendingStep::Restart,
    }
}

/// Меняет ли команда состояние демона. После такой команды снимок сессии
/// `default` записывается до ответа клиенту; команды только для чтения
/// (`status`, `sessions`, `session list`) файла не трогают.
pub fn changes_state(cmd: &str, op: Option<&str>) -> bool {
    match cmd {
        "raise" | "app" | "next" | "move-desktop" | "arrange" | "half" | "maximize" | "place" | "remove" | "save-session" | "save-workspace" => true,
        "session" => op == Some("load"),
        _ => false,
    }
}

/// Назначения ячеек, переживающие перечитывание конфига: остаются только
/// workspace, чей раздел в файле не менялся (спецификация ws-config, «Слежение
/// за конфигом»). Назначения остальных собираются заново при следующем
/// обращении, поэтому откат правки возвращает расстановку.
pub fn keep_cells(old: &Config, new: &Config, cells: &mut BTreeMap<String, BTreeMap<String, Place>>) {
    cells.retain(|ws, _| match (old.workspaces.get(ws), new.workspaces.get(ws)) {
        (Some(o), Some(n)) => o == n,
        _ => false,
    });
}

/// Приложение конфига, которому подходит окно по `class` и, если он задан,
/// `title`. Сначала перебираются варианты, затем семейства, внутри группы
/// по именам: более точное совпадение побеждает, как при захвате открытых окон.
pub fn app_for_window(cfg: &Config, c: &Client) -> Option<String> {
    let mut names: Vec<&String> = cfg.apps.keys().collect();
    names.sort_by_key(|n| (cfg.family_of(n).is_none(), n.as_str()));
    names
        .into_iter()
        .find(|n| matches!(cfg.apps[*n].matchers(), Ok(Some((cr, tr))) if matcher_fits(Some(&cr), tr.as_ref(), c)))
        .cloned()
}

/// Место окна, которое приложение конфига открыло само (design D16). Правило
/// то же, что у расстановки (спецификация ws-daemon, «Расстановка окон»), но
/// без последнего шага: ячейка или `rect` приложения в активном workspace
/// стола, то же у его семейства, `rect` самого приложения. `None` означает,
/// что места нет ни там, ни там: окно остаётся, где его открыл композитор,
/// и демон его не центрирует.
pub fn open_rect(st: &mut State, cfg: &Config, ws: Option<&str>, app: &str, mon: (i32, i32)) -> Option<PxRect> {
    match ws.filter(|w| cfg.workspaces.contains_key(*w)) {
        Some(w) => st.rect_for(cfg, w, app, mon),
        None => State::app_rect(cfg, app, mon),
    }
}

/// Что делает команда переноса workspace на стол.
#[derive(Debug, PartialEq, Eq)]
pub enum MoveStep {
    /// Перенести названный workspace на целевой стол и перейти туда.
    Move(String),
    /// Ничего не делать: workspace уже на целевом столе.
    Here(String),
    /// Ничего не делать: на текущем столе активного workspace нет.
    Nothing,
}

/// Решение команды `move-desktop` (спецификация ws-daemon, «Перенос workspace
/// на стол») по фактическому текущему столу, целевому столу и активному
/// workspace текущего стола. Текущий стол читается у композитора, а не берётся
/// из кэша демона (`Daemon::sync_current`): иначе перенос идёт не с того стола,
/// на котором пользователь.
pub fn move_step(current: u8, target: u8, active: Option<&str>) -> MoveStep {
    match active {
        None => MoveStep::Nothing,
        Some(ws) if target == current => MoveStep::Here(ws.to_string()),
        Some(ws) => MoveStep::Move(ws.to_string()),
    }
}

/// План расстановки по команде (спецификация ws-daemon, «Расстановка
/// по команде»): адрес окна и место, которое оно займёт. Окно приложения,
/// описанного в эффективном конфиге, встаёт на место своего приложения
/// по правилу мест; окно без описания — в положение по умолчанию, а окна
/// одного класса без описания образуют одну стопку на месте первого из них.
/// Окна передаются по порядку появления: он и задаёт это первое окно.
pub fn arrange_plan(st: &mut State, cfg: &Config, ws: Option<&str>, windows: &[&Client], mon: (i32, i32)) -> Vec<(String, PxRect)> {
    let mut stacks: BTreeMap<String, PxRect> = BTreeMap::new();
    let mut plan = Vec::new();
    for c in windows {
        let rect = c.app().filter(|a| cfg.apps.contains_key(a)).and_then(|a| open_rect(st, cfg, ws, &a, mon));
        let r = match rect {
            Some(r) => r,
            None => *stacks.entry(c.class.clone()).or_insert_with(|| center_rect(c, mon)),
        };
        plan.push((c.address.clone(), r));
    }
    plan
}

/// Положение по умолчанию: центр экрана с нынешним размером окна — так ставит
/// новое окно и композитор по правилам `float-by-default` и `center` сессии.
fn center_rect(c: &Client, mon: (i32, i32)) -> PxRect {
    PxRect { x: (mon.0 - c.size.0) / 2, y: (mon.1 - c.size.1) / 2, w: c.size.0, h: c.size.1 }
}

/// Окна, которые пора освободить: тег называет приложение, которого
/// в эффективном конфиге нет. Такое окно никому не принадлежит, поэтому
/// считается посторонним (спецификация ws-daemon, «Захват открытых окон
/// приложения»). Дополнительные приложения сессии к этому моменту уже входят
/// в эффективный конфиг, и их окна здесь не выбираются.
pub fn stale_tagged<'a>(cfg: &Config, clients: &'a [Client]) -> Vec<&'a Client> {
    clients.iter().filter(|c| c.app().is_some_and(|a| !cfg.apps.contains_key(&a))).collect()
}

/// Приложения ожидания, чьи записи пора снять: приложения нет в эффективном
/// конфиге, значит запускать и ждать нечего. На одно приложение ожидание
/// заводится не больше одного, поэтому записи выбираются по имени.
pub fn stale_pending(cfg: &Config, apps: &[String]) -> Vec<String> {
    apps.iter().filter(|a| !cfg.apps.contains_key(*a)).cloned().collect()
}

/// Наименьший свободный номер, начиная с 1, среди занятых.
pub fn free_number(used: &[u32]) -> u32 {
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

/// Приложение workspace, которому принадлежит окно: само приложение, если оно
/// описано в workspace, иначе его семейство. Окно чужого приложения даёт `None`.
fn ws_app_of(cfg: &Config, ws_apps: &[String], c: &Client) -> Option<String> {
    let own = c.app()?;
    if ws_apps.contains(&own) {
        return Some(own);
    }
    ws_apps.iter().find(|x| cfg.app_is(&own, x)).cloned()
}

/// Что делает клавиша приложения на этом нажатии.
#[derive(Debug, PartialEq, Eq)]
pub enum CycleStep {
    /// Окон нет: запустить приложение.
    Launch,
    /// Выбрать окно (что значит «выбрать», решает режим workspace).
    Select(String),
    /// Цикл закончен: вернуться к окну prev.
    Back(String),
    /// Цикл закончен, prev нет: первое окно следующего приложения workspace.
    NextApp(String),
}

/// Шаг цикла по экземплярам приложения (спецификация ws-daemon, «Цепочка
/// приложения»). `order` — адреса экземпляров по порядку, `active` — активное
/// окно, `fresh` — workspace только что поднят этой же цепочкой, `prev` — окно
/// возврата, `next` — первое окно следующего приложения workspace.
///
/// Счётчика нажатий нет: шаг выводится из активного окна, поэтому выбор
/// экземпляра мышью учёта не меняет.
pub fn cycle_step(order: &[String], active: Option<&str>, fresh: bool, prev: Option<&str>, next: Option<&str>) -> CycleStep {
    let Some(first) = order.first() else { return CycleStep::Launch };
    if fresh {
        return CycleStep::Select(first.clone());
    }
    match active.and_then(|a| order.iter().position(|x| x == a)) {
        // Активно окно приложения, и за ним есть следующий экземпляр.
        Some(i) if i + 1 < order.len() => CycleStep::Select(order[i + 1].clone()),
        // Активен последний экземпляр: цикл закончен.
        Some(_) => match (prev, next) {
            (Some(p), _) => CycleStep::Back(p.to_string()),
            (None, Some(n)) => CycleStep::NextApp(n.to_string()),
            (None, None) => CycleStep::Select(first.clone()),
        },
        // Активно чужое окно: цикл начинается с первого экземпляра.
        None => CycleStep::Select(first.clone()),
    }
}

/// Окно, к которому вернёт конец цикла (prev). В режиме обмена ячеек это
/// первое окно приложения, стоявшего в главной ячейке (`main_window`): конец
/// цикла обязан вернуть расстановку. В режиме `stack` — активное окно
/// workspace, а если активно чужое окно, то последнее окно workspace,
/// получавшее фокус. Экземпляры самого вызванного приложения prev не бывают.
pub fn anchor_window(cfg: &Config, clients: &[Client], ws_apps: &[String], main_window: Option<&str>, order: &[String], active: Option<&str>, focus: Option<&str>) -> Option<String> {
    if let Some(m) = main_window {
        return Some(m.to_string());
    }
    let fits = |a: &str| {
        !order.iter().any(|x| x == a) && clients.iter().any(|c| c.address == a && !c.on_hidden() && ws_app_of(cfg, ws_apps, c).is_some())
    };
    active.filter(|a| fits(a)).or_else(|| focus.filter(|a| fits(a))).map(String::from)
}

/// Первое нескрытое окно следующего приложения workspace: по порядку записи
/// в разделе workspace, циклически после вызванного. Приложения без окон
/// пропускаются — конец цикла не запускает программу, которую не вызывали.
pub fn next_app_window(cfg: &Config, clients: &[Client], ws: &str, app: &str) -> Option<String> {
    let names: Vec<String> = cfg.workspaces.get(ws)?.apps.keys().cloned().collect();
    let i = names.iter().position(|n| n == app)?;
    (1..names.len())
        .map(|k| &names[(i + k) % names.len()])
        .find_map(|name| placed_windows(cfg, clients, &names, name).into_iter().find(|c| !c.on_hidden()).map(|c| c.address.clone()))
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

[apps.calc]
cmd = "qalculate-gtk"
class = "^qalculate-gtk$"
rect = { x = 2600, y = 1500, w = 600, h = 400 }

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
    fn app_for_window_prefers_variant() {
        let cfg = Config::parse(CFG).unwrap();
        let gmail = client("0x1", "chromium", "Gmail — Входящие", "1", &[]);
        let news = client("0x2", "chromium", "Новости", "1", &[]);
        let alien = client("0x3", "galculator", "Калькулятор", "1", &[]);
        // Более точное совпадение побеждает: окно Gmail достаётся варианту.
        assert_eq!(app_for_window(&cfg, &gmail).as_deref(), Some("chromium-mail"));
        assert_eq!(app_for_window(&cfg, &news).as_deref(), Some("chromium"));
        // Окно, которому не подходит ни одно приложение конфига.
        assert_eq!(app_for_window(&cfg, &alien), None);
    }

    #[test]
    fn new_window_takes_the_place_of_its_app() {
        let cfg = Config::parse(CFG).unwrap();
        let mon = (3840, 2160);
        let mut st = State::default();
        let left = Some(PxRect { x: -805, y: 10, w: 1920, h: 2140 });
        let own = Some(PxRect { x: 2600, y: 1500, w: 600, h: 400 });
        // Второе окно Chromium встаёт в ячейку `chromium`, а не в центр экрана.
        assert_eq!(open_rect(&mut st, &cfg, Some("work"), "chromium", mon), left);
        // Окно варианта встаёт в ячейку семейства: своей записи в `work` у него нет.
        assert_eq!(open_rect(&mut st, &cfg, Some("work"), "chromium-mail", mon), left);
        // Приложения в активном workspace нет, но своё место у него есть.
        assert_eq!(open_rect(&mut st, &cfg, Some("work"), "calc", mon), own);
        assert_eq!(open_rect(&mut st, &cfg, None, "calc", mon), own);
        // Ни места в активном workspace, ни своего `rect`: окно не двигается.
        assert_eq!(open_rect(&mut st, &cfg, Some("work"), "wezterm", mon), None);
        assert_eq!(open_rect(&mut st, &cfg, None, "chromium", mon), None);
        assert_eq!(open_rect(&mut st, &cfg, Some("нет-такого"), "chromium", mon), None);
        // Окно встаёт туда, где приложение стоит сейчас, а не где записано в файле.
        swap_cells(st.cells_of(&cfg, "work", mon), Some("herdr"), "chromium", "center");
        assert_eq!(open_rect(&mut st, &cfg, Some("work"), "chromium", mon), Some(PxRect { x: 1125, y: 10, w: 1920, h: 2140 }));
    }

    #[test]
    fn stale_tags_are_freed_only_for_unknown_apps() {
        let cfg = Config::parse(CFG).unwrap();
        let live = client("0x1", "chromium", "Новости", "1", &["app:chromium#1"]);
        let variant = client("0x2", "wezterm-herdr", "herdr · dev-lab", "1", &["app:herdr#1"]);
        // Приложение, которого в конфиге нет: сохранение записало его в файл,
        // а файл откатили.
        let gone = client("0x3", "Alacritty", "bash", "1", &["app:alacritty#1"]);
        // Тег прежней версии демона, без номера экземпляра.
        let gone_old = client("0x4", "Galculator", "Калькулятор", "1", &["app:galculator"]);
        // Свободное окно: освобождать нечего, тега приложения у него нет.
        let free = client("0x5", "Galculator", "Калькулятор", "1", &["pin:1"]);
        let clients = vec![live, variant, gone, gone_old, free];

        let addrs = |cfg: &Config| stale_tagged(cfg, &clients).iter().map(|c| c.address.clone()).collect::<Vec<_>>();
        assert_eq!(addrs(&cfg), vec!["0x3".to_string(), "0x4".to_string()]);

        // Дополнительное приложение сессии входит в эффективный конфиг,
        // и его окно свободным не становится.
        let rect = PxRect { x: 100, y: 200, w: 800, h: 600 };
        let mut extra: BTreeMap<String, BTreeMap<String, ExtraApp>> = BTreeMap::new();
        extra.entry("work".into()).or_default().insert("galculator".into(), ExtraApp { class: Some("Galculator".into()), cmd: vec!["galculator".into()], cwd: None, rect });
        let (eff, dropped) = merge_extra(&cfg, &extra);
        assert!(dropped.is_empty());
        assert_eq!(addrs(&eff), vec!["0x3".to_string()]);
    }

    #[test]
    fn flatpak_app_id_from_cgroup() {
        // Строка из живой системы: окно Postman из flatpak.
        let postman = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-com.getpostman.Postman-1890165170.scope\n";
        assert_eq!(flatpak_app_id(postman).as_deref(), Some("com.getpostman.Postman"));
        // Обычный процесс: юнита flatpak нет, командная строка берётся как есть.
        let plain = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/workspaced.service\n";
        assert_eq!(flatpak_app_id(plain), None);
        assert_eq!(flatpak_app_id(""), None);
        // Особые символы systemd записывает как \xNN.
        let escaped = r"0::/user.slice/app-flatpak-com\x2egetpostman\x2ePostman-17.scope";
        assert_eq!(flatpak_app_id(escaped).as_deref(), Some("com.getpostman.Postman"));
        // Идентификатор с дефисом: отбрасывается только числовой хвост.
        let dashed = "0::/app.slice/app-flatpak-org.gnome.gedit-tools-42.scope";
        assert_eq!(flatpak_app_id(dashed).as_deref(), Some("org.gnome.gedit-tools"));
    }

    #[test]
    fn stale_pending_is_dropped_only_for_unknown_apps() {
        let cfg = Config::parse(CFG).unwrap();
        // `failtest` в конфиг добавляли и откатили: ожидание его окна ждёт
        // приложения, которого больше нет, и снимается вместе с мёртвым pid.
        let apps = ["chromium".to_string(), "herdr".to_string(), "failtest".to_string()];
        assert_eq!(stale_pending(&cfg, &apps), vec!["failtest".to_string()]);

        // Дополнительное приложение сессии входит в эффективный конфиг,
        // и ожидание его окна не снимается.
        let rect = PxRect { x: 100, y: 200, w: 800, h: 600 };
        let mut extra: BTreeMap<String, BTreeMap<String, ExtraApp>> = BTreeMap::new();
        extra.entry("work".into()).or_default().insert("galculator".into(), ExtraApp { class: Some("Galculator".into()), cmd: vec!["galculator".into()], cwd: None, rect });
        let (eff, dropped) = merge_extra(&cfg, &extra);
        assert!(dropped.is_empty());
        let apps = ["galculator".to_string(), "failtest".to_string()];
        assert_eq!(stale_pending(&eff, &apps), vec!["failtest".to_string()]);
        // Пока приложение в конфиге есть, ожидание живёт до окна.
        assert!(stale_pending(&cfg, &["neovide".to_string()]).is_empty());
    }

    #[test]
    fn merge_extra_adds_apps_and_drops_shadowed() {
        let cfg = Config::parse(CFG).unwrap();
        let rect = PxRect { x: 100, y: 200, w: 800, h: 600 };
        let full = ExtraApp { class: Some("Galculator".into()), cmd: vec!["galculator".into()], cwd: None, rect };
        let place_only = ExtraApp { rect, ..ExtraApp::default() };
        let mut extra: BTreeMap<String, BTreeMap<String, ExtraApp>> = BTreeMap::new();
        let apps = extra.entry("work".into()).or_default();
        apps.insert("galculator".into(), full.clone());
        apps.insert("chromium-mail".into(), place_only.clone());
        let (eff, dropped) = merge_extra(&cfg, &extra);
        assert!(dropped.is_empty());
        // Новое приложение появилось и в [apps], и в таблице apps workspace.
        assert_eq!(eff.apps["galculator"].cmd.as_deref(), Some("galculator"));
        assert_eq!(eff.workspaces["work"].apps["galculator"], Placement::Rect { rect: rect.to_rect() });
        // Запись только о месте описание приложения конфига не подменяет,
        // а лишь добавляет его в этот workspace.
        assert_eq!(eff.apps["chromium-mail"].title.as_deref(), Some("^Gmail"));
        assert_eq!(eff.workspaces["work"].apps["chromium-mail"], Placement::Rect { rect: rect.to_rect() });
        // Файл конфига не менялся.
        assert!(!cfg.apps.contains_key("galculator"));
        assert!(!cfg.workspaces["work"].apps.contains_key("galculator"));

        // Имя занято конфигом — запись конфига главнее; приложения конфига
        // не стало — запись только о месте снимается; workspace исчез — тоже.
        let mut extra: BTreeMap<String, BTreeMap<String, ExtraApp>> = BTreeMap::new();
        extra.entry("work".into()).or_default().insert("chromium".into(), full);
        extra.entry("work".into()).or_default().insert("keepassxc".into(), place_only.clone());
        extra.entry("notes".into()).or_default().insert("galculator".into(), place_only.clone());
        let (eff, dropped) = merge_extra(&cfg, &extra);
        let mut names: Vec<&str> = dropped.iter().map(|(_, n, _)| n.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["chromium", "galculator", "keepassxc"]);
        assert_eq!(eff.apps["chromium"].cmd.as_deref(), Some("chromium"));
        assert!(!eff.workspaces["work"].apps.contains_key("keepassxc"));

        // Конфиг сам задал место приложению в этом workspace: запись снимается.
        let mut extra: BTreeMap<String, BTreeMap<String, ExtraApp>> = BTreeMap::new();
        extra.entry("work".into()).or_default().insert("herdr".into(), place_only);
        let (eff, dropped) = merge_extra(&cfg, &extra);
        assert_eq!(dropped.len(), 1);
        assert_eq!(eff.workspaces["work"].apps["herdr"], Placement::Cell("center".into()));
    }

    #[test]
    fn keep_cells_resets_only_changed_workspace() {
        let cfg = Config::parse(CFG).unwrap();
        let changed = Config::parse(&CFG.replace("neovide = \"right\"", "neovide = { rect = { x = 0, y = 0, w = 10, h = 10 } }")).unwrap();
        let added = Config::parse(&format!("{CFG}\n[workspaces.surf]\ntemplate = \"thirds\"\napps = {{ neovide = \"right\" }}\n")).unwrap();
        let cells = || BTreeMap::from([("work".to_string(), BTreeMap::from([("chromium".to_string(), Place::Cell("center".into()))]))]);
        // Раздел workspace изменился: назначения снимаются и соберутся из файла.
        let mut c = cells();
        keep_cells(&cfg, &changed, &mut c);
        assert!(c.is_empty());
        // Правка соседнего workspace обмен ячеек в `work` не сбрасывает.
        let mut c = cells();
        keep_cells(&cfg, &added, &mut c);
        assert_eq!(c["work"]["chromium"], Place::Cell("center".into()));
        // Workspace из конфига исчез.
        let mut c = cells();
        keep_cells(&cfg, &Config::default(), &mut c);
        assert!(c.is_empty());
    }

    #[test]
    fn pending_waits_for_the_window_without_deadlines() {
        // Окно появилось до выхода процесса: запись снимает обработчик
        // openwindow, до решения дело не доходит. Здесь — остальные переходы.
        // Процесс вышел, подходящее окно уже есть: принять сразу.
        assert_eq!(pending_step(false, true, false), PendingStep::Adopt);
        // Процесс вышел, окна нет: ждать первого подходящего окна.
        assert_eq!(pending_step(false, false, false), PendingStep::MarkExited);
        // Клавиша нажата, пока запуск идёт: второй экземпляр не заводится.
        assert_eq!(pending_step(false, false, true), PendingStep::Wait);
        assert_eq!(pending_step(false, true, true), PendingStep::Wait);
        // Клавиша нажата после выхода процесса: принять окно, если оно есть,
        // иначе запустить заново — клавиша не остаётся без ответа.
        assert_eq!(pending_step(true, true, true), PendingStep::Adopt);
        assert_eq!(pending_step(true, false, true), PendingStep::Restart);
    }

    #[test]
    fn config_reload_only_on_finished_writes() {
        use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, EventKind, ModifyKind, RenameMode};
        // Правка на месте (nvim, tee): перечитывание по закрытию файла.
        assert!(write_finished(&EventKind::Access(AccessKind::Close(AccessMode::Write))));
        // Запись во временный файл с переименованием (chezmoi apply).
        assert!(write_finished(&EventKind::Modify(ModifyKind::Name(RenameMode::To))));
        // Парное событие переименования приходит вдобавок к `To`: учёт обоих
        // дал бы два перечитывания на одно сохранение.
        assert!(!write_finished(&EventKind::Modify(ModifyKind::Name(RenameMode::Both))));
        assert!(!write_finished(&EventKind::Modify(ModifyKind::Name(RenameMode::From))));
        // Посреди записи и при создании файла не перечитываем: иначе на одно
        // сохранение пришлось бы несколько перечитываний, в том числе неполного файла.
        assert!(!write_finished(&EventKind::Modify(ModifyKind::Data(DataChange::Any))));
        assert!(!write_finished(&EventKind::Create(CreateKind::File)));
        assert!(!write_finished(&EventKind::Access(AccessKind::Close(AccessMode::Read))));
    }

    #[test]
    fn only_state_changing_commands_write_the_snapshot() {
        for cmd in ["raise", "app", "next", "move-desktop", "arrange", "half", "maximize", "place", "remove", "save-session", "save-workspace"] {
            assert!(changes_state(cmd, None), "{cmd}");
        }
        assert!(changes_state("session", Some("load")));
        for (cmd, op) in [("status", None), ("sessions", None), ("session", Some("list")), ("session", Some("save"))] {
            assert!(!changes_state(cmd, op), "{cmd}");
        }
    }

    /// Окна `work`: herdr, chromium, neovide и одно свободное.
    fn work_clients() -> Vec<Client> {
        vec![
            client("0x1", "wezterm-herdr", "herdr · dev-lab", "1", &["app:herdr#1"]),
            client("0x2", "chromium", "Новости", "1", &["app:chromium#1"]),
            client("0x3", "neovide", "[Scratch]", "1", &["app:neovide#1"]),
            client("0x9", "Galculator", "Калькулятор", "1", &[]),
        ]
    }

    #[test]
    fn cycle_walks_instances_and_returns_to_prev() {
        let a = |s: &str| s.to_string();
        let order = vec![a("0x1"), a("0x2"), a("0x3")];
        // Активно чужое окно: цикл начинается с первого экземпляра.
        assert_eq!(cycle_step(&order, Some("0xf"), false, Some("0xf"), None), CycleStep::Select(a("0x1")));
        // Дальше экземпляры идут по порядку.
        assert_eq!(cycle_step(&order, Some("0x1"), false, Some("0xf"), None), CycleStep::Select(a("0x2")));
        assert_eq!(cycle_step(&order, Some("0x2"), false, Some("0xf"), None), CycleStep::Select(a("0x3")));
        // За последним экземпляром цикл заканчивается возвратом к prev.
        assert_eq!(cycle_step(&order, Some("0x3"), false, Some("0xf"), None), CycleStep::Back(a("0xf")));
        // prev нет: первое окно следующего приложения workspace.
        assert_eq!(cycle_step(&order, Some("0x3"), false, None, Some("0x7")), CycleStep::NextApp(a("0x7")));
        // Ни prev, ни следующего приложения: цикл начинается сначала.
        assert_eq!(cycle_step(&order, Some("0x3"), false, None, None), CycleStep::Select(a("0x1")));
        // Выбор экземпляра мышью учёта не меняет: после клика по второму окну
        // следующее нажатие выбирает третье, а первое повторно не показывается.
        assert_eq!(cycle_step(&order, Some("0x2"), false, None, None), CycleStep::Select(a("0x3")));
        // Единственный экземпляр — переключатель «туда и обратно».
        let one = vec![a("0x1")];
        assert_eq!(cycle_step(&one, Some("0xf"), false, Some("0xf"), None), CycleStep::Select(a("0x1")));
        assert_eq!(cycle_step(&one, Some("0x1"), false, Some("0xf"), None), CycleStep::Back(a("0xf")));
        // Только что поднятый workspace начинает цикл заново.
        assert_eq!(cycle_step(&order, Some("0x3"), true, Some("0xf"), None), CycleStep::Select(a("0x1")));
        // Окон нет: приложение запускается.
        assert_eq!(cycle_step(&[], Some("0xf"), false, None, None), CycleStep::Launch);
        // Активного окна нет вовсе (пустой стол): первый экземпляр.
        assert_eq!(cycle_step(&order, None, false, None, None), CycleStep::Select(a("0x1")));
    }

    #[test]
    fn anchor_is_main_window_in_swap_and_previous_in_stack() {
        let cfg = Config::parse(CFG).unwrap();
        let ws_apps: Vec<String> = vec!["herdr".into(), "chromium".into(), "neovide".into()];
        let clients = work_clients();
        // Цикл идёт по единственному окну chromium.
        let order = vec!["0x2".to_string()];
        let anchor = |main: Option<&str>, active: Option<&str>, focus: Option<&str>| anchor_window(&cfg, &clients, &ws_apps, main, &order, active, focus).unwrap_or_default();
        // Режим обмена ячеек: возврат к окну прежнего главного приложения,
        // каким бы ни было активное окно.
        assert_eq!(anchor(Some("0x1"), Some("0x3"), None), "0x1");
        // Режим stack: активное окно workspace.
        assert_eq!(anchor(None, Some("0x3"), None), "0x3");
        // Активно свободное окно: берётся последнее окно workspace с фокусом.
        assert_eq!(anchor(None, Some("0x9"), Some("0x1")), "0x1");
        // Экземпляр вызванного приложения prev не бывает.
        assert_eq!(anchor(None, Some("0x2"), Some("0x2")), "");
        // Закрытое окно prev не даёт.
        assert_eq!(anchor(None, Some("0xdead"), None), "");
    }

    #[test]
    fn next_app_follows_config_order() {
        // В разделе `work` приложения записаны как herdr, chromium, neovide —
        // порядок не алфавитный, и цикл идёт именно по нему.
        let cfg = Config::parse(CFG).unwrap();
        let clients = work_clients();
        let next = |app: &str, cs: &[Client]| next_app_window(&cfg, cs, "work", app).unwrap_or_default();
        assert_eq!(next("herdr", &clients), "0x2");
        assert_eq!(next("chromium", &clients), "0x3");
        // За последним приложением списка снова идёт первое.
        assert_eq!(next("neovide", &clients), "0x1");
        // Приложение без окон пропускается.
        let without: Vec<Client> = clients.iter().filter(|c| c.address != "0x2").cloned().collect();
        assert_eq!(next("herdr", &without), "0x3");
        // Окон нет ни у кого, кроме вызванного: следующего приложения нет.
        assert_eq!(next("herdr", &clients[..1]), "");
        // Приложение вне workspace следующего не имеет.
        assert_eq!(next("wezterm", &clients), "");
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
    fn move_step_by_current_and_target_desktop() {
        // Целевой стол не текущий: активный workspace текущего стола переезжает.
        assert_eq!(move_step(2, 3, Some("surf")), MoveStep::Move("surf".to_string()));
        assert_eq!(move_step(1, 8, Some("work")), MoveStep::Move("work".to_string()));
        // Целевой стол — текущий: workspace уже здесь, действия нет.
        assert_eq!(move_step(3, 3, Some("surf")), MoveStep::Here("surf".to_string()));
        // На текущем столе активного workspace нет: переносить нечего.
        assert_eq!(move_step(4, 5, None), MoveStep::Nothing);
        assert_eq!(move_step(3, 3, None), MoveStep::Nothing);
    }

    #[test]
    fn arrange_plan_places_apps_and_stacks_free_windows() {
        let cfg = Config::parse(CFG).unwrap();
        let mon = (3840, 2160);
        let mut st = State::default();
        let sized = |addr: &str, class: &str, tags: &[&str], w: i32, h: i32| {
            let mut c = client(addr, class, "", "1", tags);
            c.size = (w, h);
            c
        };
        let clients = [
            sized("0x1", "wezterm-herdr", &["app:herdr#1"], 100, 100),
            sized("0x2", "chromium", &["app:chromium#1"], 100, 100),
            sized("0x3", "chromium", &["app:chromium#2"], 100, 100),
            sized("0x4", "qalculate-gtk", &["app:calc#1"], 600, 400),
            sized("0x5", "Galculator", &[], 800, 600),
            sized("0x6", "Galculator", &[], 400, 300),
            sized("0x7", "org.telegram.desktop", &[], 1000, 900),
            // Тег называет приложение, которого в конфиге нет: описания у окна тоже нет.
            sized("0x8", "neovide", &["app:editor-dots#1"], 500, 500),
        ];
        let windows: Vec<&Client> = clients.iter().collect();
        let plan = arrange_plan(&mut st, &cfg, Some("work"), &windows, mon);
        let place = |addr: &str| plan.iter().find(|(a, _)| a == addr).map(|(_, r)| *r).unwrap();
        // Окна приложений — на свои места по правилу мест: вариант в ячейке
        // семейства, два окна Chromium стопкой в одной ячейке, приложение вне
        // workspace — в свой `rect`.
        assert_eq!(place("0x1"), PxRect { x: 1125, y: 10, w: 1920, h: 2140 });
        assert_eq!(place("0x2"), PxRect { x: -805, y: 10, w: 1920, h: 2140 });
        assert_eq!(place("0x3"), place("0x2"));
        assert_eq!(place("0x4"), PxRect { x: 2600, y: 1500, w: 600, h: 400 });
        // Свободные окна — в центр экрана со своим размером, а второе окно того
        // же класса встаёт на место первого по порядку появления.
        assert_eq!(place("0x5"), PxRect { x: 1520, y: 780, w: 800, h: 600 });
        assert_eq!(place("0x6"), place("0x5"));
        assert_eq!(place("0x7"), PxRect { x: 1420, y: 630, w: 1000, h: 900 });
        assert_eq!(place("0x8"), PxRect { x: 1670, y: 830, w: 500, h: 500 });

        // Активного workspace на столе нет: остаётся правило «`rect` приложения,
        // иначе центр экрана».
        let mut st = State::default();
        let plan = arrange_plan(&mut st, &cfg, None, &windows, mon);
        let place = |addr: &str| plan.iter().find(|(a, _)| a == addr).map(|(_, r)| *r).unwrap();
        assert_eq!(place("0x4"), PxRect { x: 2600, y: 1500, w: 600, h: 400 });
        assert_eq!(place("0x1"), PxRect { x: 1870, y: 1030, w: 100, h: 100 });
        assert_eq!(place("0x3"), place("0x2"));
        // Назначения ячеек расстановка не меняет.
        assert!(st.cells.is_empty());
    }

    #[test]
    fn place_positions_without_reserved() {
        let full = PxRect { x: 0, y: 0, w: 3840, h: 2160 };
        assert_eq!(place_rect(full, 5, "bottom-left").unwrap(), PxRect { x: 10, y: 1085, w: 1905, h: 1065 });
        assert_eq!(place_rect(full, 5, "top-center").unwrap(), PxRect { x: 967, y: 10, w: 1905, h: 1065 });
    }
}
