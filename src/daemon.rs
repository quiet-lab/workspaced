//! Демон: владелец модели. Слушает события Hyprland, выполняет команды клиентов
//! и панели, запускает приложения, расставляет окна, пишет сессию `default`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::{Config, Mode, Placement, PxRect, WsApp, config_path};
use crate::keys::same_chain;
use crate::hypr::{self, Client, Event, Hypr};
use crate::session;
use crate::state::{Cycle, Desktop, ExtraApp, Foreign, Moved, Place, RestoreEntry, RestoreState, State};

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
        // Окно без класса остаётся свободным всегда (изменение
        // classless-windows-stay-free, решение D1): его не отличить
        // от служебного окна приложения вроде диспетчера задач.
        if classless(c) {
            return false;
        }
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
    !classless(c)
        && class_re.is_some_and(|cr| cr.is_match(&c.class)) && title_re.is_none_or(|t| t.is_match(&c.title))
}

/// У окна нет класса (`class` пуст): так композитор показывает, например,
/// диспетчер задач Chromium и Яндекс.Браузера. Такое окно не принадлежит
/// ни одному приложению и остаётся свободным всегда (изменение
/// classless-windows-stay-free, решение D1).
pub fn classless(c: &Client) -> bool {
    c.class.trim().is_empty()
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
    // Конфиг при старте прочитан выше, значит привязки в нём могут быть новее
    // тех, что знает композитор: файл могли поправить, пока демон не работал,
    // или демон перезапустили с бинарником, где появились новые действия.
    reload_hypr_binds();
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

/// Перечитать конфиг композитора, чтобы он заново выполнил `hyprland.lua`,
/// а тот — `workspaced keys --lua`: только так новые привязки попадают
/// в Hyprland. Вызывается при старте демона и после каждого удачного
/// перечитывания файла конфига. Неудача — предупреждение, а не ошибка:
/// при старте сессии композитор мог ещё не открыть свой сокет, но привязки
/// там и без того свежие — `hyprland.lua` выполняется при старте сессии
/// и читает конфиг сам, без демона.
fn reload_hypr_binds() {
    match Command::new("hyprctl").arg("reload").arg("config-only").output() {
        Ok(o) if o.status.success() => {}
        Ok(o) => log::warn!("hyprctl reload: {}", String::from_utf8_lossy(&o.stderr)),
        Err(e) => log::warn!("hyprctl reload: {e}"),
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
        Ok(())
    }

    pub fn reload_config(&mut self) {
        match Config::load(&self.cfg_path) {
            Ok(cfg) => {
                let old = std::mem::replace(&mut self.cfg_file, cfg);
                self.cfg_text = std::fs::read_to_string(&self.cfg_path).unwrap_or_default();
                // Раскладка снимается у workspace, чей раздел изменился или
                // исчез: правка файла возвращает расстановку, а обмен мест
                // и сдвинутые окна в нетронутых workspace сохраняются.
                let changed = keep_layout(&old, &self.cfg_file, &mut self.st);
                self.rebuild_cfg();
                self.drop_stale_pending();
                self.free_stale_tagged();
                self.sync_membership(false);
                log::info!("конфиг перечитан: {} workspace, {} приложений", self.cfg.workspaces.len(), self.cfg.apps.len());
                // Поднятие окна на столе больше не двигает (решение D8),
                // поэтому новая раскладка изменённого раздела применяется
                // сразу там, где workspace активен (решение D12).
                for ws in changed {
                    if let Some(n) = self.active_desktop_of(&ws)
                        && let Err(e) = self.apply_layout(&ws, n)
                    {
                        log::warn!("раскладка {ws} после перечитывания конфига: {e:#}");
                    }
                }
                reload_hypr_binds();
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
            if let Some(m) = self.st.moved.get_mut(&ws) {
                m.remove(&name);
            }
        }
        self.st.moved.retain(|_, m| !m.is_empty());
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

    /// Записать снимок сессии `default`. Вызывается только явной командой
    /// сохранения — `save-session` и загрузкой именованной сессии, которая
    /// страхует прежнее состояние (решение D6): события композитора и прочие
    /// команды файл не трогают.
    pub fn save_session_file(&mut self) {
        if let Err(e) = session::save_default(self) {
            log::warn!("запись сессии default: {e:#}");
        }
    }

    // ---- События ----------------------------------------------------------------

    fn on_event(&mut self, ev: Event) -> Result<()> {
        // Снимок сессии по событию не пишется: файл `default.toml` меняет
        // только команда сохранения сессии (решение D6).
        match ev {
            Event::OpenWindow { addr, .. } => self.on_open(&addr)?,
            Event::CloseWindow { addr } => {
                self.st.foreign.remove(&addr);
                self.forget_window(&addr);
                // Закрытое окно уже не прочитать у композитора: изменённые
                // места, снятые с него, ищутся по владельцу (решение D9).
                match self.hypr.clients() {
                    Ok(mut clients) => {
                        clients.retain(|c| c.address != addr);
                        self.drop_moved_of(&addr, &clients, "окно закрыто");
                    }
                    Err(e) => log::warn!("закрытие окна {addr}: {e:#}"),
                }
                self.broadcast();
            }
            Event::MoveWindow { .. } => self.broadcast(),
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
                    // Общие окна активного workspace стола приходят за
                    // пользователем (изменение shared-windows, решение D5).
                    if let Err(e) = self.follow() {
                        log::warn!("следование окон за столом: {e:#}");
                    }
                    self.broadcast();
                }
            }
            Event::ActiveWindow { .. } => {
                // Последнее окно workspace, получавшее фокус: из него берётся
                // prev, когда цикл начинается с чужого окна.
                if let Err(e) = self.on_focus() {
                    log::warn!("смена фокуса: {e:#}");
                }
            }
            Event::Other(_) => {}
        }
        Ok(())
    }

    /// Следование окон за столом (решения D5, D6): окна активного workspace
    /// стола, где находится композитор, переезжают туда с других столов
    /// и с `special:pool` в состояние, запомненное для этого workspace.
    /// Стол спрашивается у композитора: событие смены стола приходит отдельным
    /// соединением и может отстать от команды демона, и по такому событию
    /// окно не должно уехать со стола, где пользователь уже находится. Фокус
    /// не меняется; окно с фокусом поднимается наверх, чтобы пришедшие окна
    /// его не заслонили. Всё уходит одной последовательностью диспетчеров.
    fn follow(&mut self) -> Result<()> {
        let n = self.sync_current();
        let clients = self.hypr.clients()?;
        let cfg = self.cfg.clone();
        let plan = follow_plan(&mut self.st, &cfg, &clients, n, self.mon);
        let Some(ws) = self.st.desktops.get(&n).and_then(|d| d.active.clone()) else { return Ok(()) };
        if plan.is_empty() {
            return Ok(());
        }
        let mut ex = Vec::new();
        for f in &plan {
            ex.push(hypr::d_move_to(&f.addr, &n.to_string()));
            if let Some(r) = f.rect {
                ex.extend(hypr::d_place(&f.addr, r));
            }
            self.st.geom.entry(ws.clone()).or_default().insert(f.addr.clone(), f.rect.unwrap_or(f.now));
        }
        if let Some(a) = self.hypr.active_window()?.filter(|c| c.desktop() == Some(n)) {
            ex.push(hypr::d_raise(&a.address));
        }
        self.hypr.dispatch_all(&ex)?;
        let list: Vec<&str> = plan.iter().map(|f| f.addr.as_str()).collect();
        log::info!("стол {n}: окна workspace {ws} пришли за пользователем: {}", list.join(", "));
        Ok(())
    }

    /// Сверка тегов состава с эффективным конфигом (решение D2): тег `ws:W`
    /// снимается, если `W` нет или он не описывает приложение окна. С `derive`
    /// окна прежней версии демона — с тегом экземпляра, но без тегов состава —
    /// получают состав по прежнему правилу (`derive_membership`); это
    /// делается при старте демона и после загрузки сессии. Окно, чьи теги
    /// сняла сверка, остаётся свободным, потому что состав выдаётся только окнам,
    /// у которых тегов состава не было и до сверки.
    pub fn sync_membership(&mut self, derive: bool) {
        let clients = match self.hypr.clients() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("сверка состава workspace: {e:#}");
                return;
            }
        };
        let mut ex = Vec::new();
        let stale = stale_ws_tags(&self.cfg, &clients);
        for (addr, w) in &stale {
            let c = clients.iter().find(|c| c.address == *addr);
            log::info!("окно {addr} ({}): тег ws:{w} снят — workspace {w} не описывает его приложение {}", c.map(|c| c.class.as_str()).unwrap_or(""), c.and_then(|c| c.app()).unwrap_or_else(|| "(нет)".into()));
            ex.push(hypr::d_untag(addr, &hypr::ws_tag(w)));
        }
        // Окно, снятое сверкой состава, больше не держит изменённое место
        // своего приложения в этом workspace (решение D9).
        let mut after = clients.clone();
        for (addr, w) in &stale {
            if let Some(c) = after.iter_mut().find(|c| c.address == *addr) {
                c.tags.retain(|t| hypr::parse_ws_tag(t).as_deref() != Some(w.as_str()));
            }
        }
        for (addr, _) in &stale {
            self.drop_moved_of(addr, &after, "окно снято сверкой состава");
        }
        if derive {
            let given = derive_membership(&self.cfg, &self.st, &clients);
            for (addr, w) in &given {
                ex.push(hypr::d_tag(addr, &hypr::ws_tag(w)));
            }
            if !given.is_empty() {
                let list: Vec<String> = given.iter().map(|(a, w)| format!("{a} → {w}")).collect();
                log::info!("состав выдан окнам прежней версии демона ({}): {}", given.len(), list.join(", "));
            }
        }
        if let Err(e) = self.hypr.dispatch_all(&ex) {
            log::warn!("сверка состава workspace: {e:#}");
        }
    }

    /// Новое окно: ожидаемое приложение, восстановленное окно из снимка
    /// сессии либо окно, которое разбирает правило принятия в workspace.
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
        // Порядок сопоставления (изменение session-instances, решение D5):
        // совпадения по pid раньше совпадений по классу, иначе окно
        // экземпляра, запущенного по записи снимка, досталось бы ожиданию
        // запуска приложения по классу.
        let classy = !classless(&c);
        if let Some(i) = self.pending.iter().position(|p| classy && ancestors.contains(&(p.pid as i32))) {
            let p = self.pending.remove(i);
            self.adopt(&c, p)?;
        } else if let Some(i) = match_restore(&self.st.restore, None, &ancestors, None).filter(|_| classy) {
            self.restore_new(&c, &clients, i, "по pid запущенного процесса")?;
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
            } else if let Some(i) = self.pending.iter().position(|p| p.matches(&c, &ancestors)) {
                let p = self.pending.remove(i);
                self.adopt(&c, p)?;
            } else if let Some(i) = self.awaited_entry(&c) {
                self.restore_new(&c, &clients, i, "окно, открытое процессом приложения")?;
            } else {
                // Окно, появившееся на столе с активным workspace, входит
                // в этот workspace (решение D1): приложение конфига
                // захватывается, остальные окна становятся дополнительными
                // приложениями сессии. На столе без активного workspace окно
                // остаётся свободным.
                let ws = c.desktop().and_then(|n| self.st.desktops.get(&n).and_then(|d| d.active.clone()));
                let taken = taken_names(&self.cfg, &self.st.extra);
                let join = join_window(&self.cfg, &self.cfg_file, ws.as_deref(), &c, &cmd, &taken);
                self.join(&c, &clients, ws.as_deref(), join, cmd, cwd)?;
            }
        }
        self.broadcast();
        Ok(())
    }

    /// Окно ожидаемого приложения: тег экземпляра, тег состава workspace,
    /// для которого приложение запущено, стол, место, фокус. Если workspace
    /// приложение не описывает (приложение открыто в нём клавишей, решение
    /// D7), workspace получает запись о месте приложения.
    fn adopt(&mut self, c: &Client, p: Pending) -> Result<()> {
        let clients = self.hypr.clients().unwrap_or_default();
        // Окно, принятое ожиданием запуска, исполняет запись снимка этого
        // приложения, если такая ждёт (решения D5, D7): номер, состав и место
        // берутся из записи вместо места ожидания.
        if let Some(i) = match_restore(&self.st.restore, Some(&p.app), &[], p.workspace.as_deref()) {
            let e = self.st.restore[i].clone();
            self.st.restore[i].state = RestoreState::Done;
            let (mut ex, home, num) = self.instance_ex(c, &clients, &e, true);
            if p.focus {
                ex.push(hypr::d_focus_window(&c.address));
                ex.push(hypr::d_bring_to_top());
            } else {
                ex.extend(self.focus_main_ex(home, &p.app, &clients));
            }
            log::info!("окно {} → приложение {} (экземпляр {num}) по записи снимка {}#{}, workspace {}", c.address, p.app, e.app, e.instance, entry_members(&self.cfg, &e).join(", "));
            self.hypr.dispatch_all(&ex)?;
            self.restore_progress(&e.app);
            return Ok(());
        }
        let num = free_instance(&clients, &p.app);
        let mut ex = entry_tags(&c.address, &p.app, num, p.workspace.as_deref());
        let joining = p.workspace.as_deref().filter(|w| !ws_has_app(&self.cfg, w, &p.app)).map(String::from);
        match &p.target {
            Target::Pool => ex.push(hypr::d_move_to(&c.address, "special:pool")),
            Target::Place(r) => {
                if c.desktop() != Some(p.desktop) {
                    ex.push(hypr::d_move_to(&c.address, &p.desktop.to_string()));
                }
                ex.extend(hypr::d_place(&c.address, *r));
                // Окно встало на место своего приложения: запомнить его
                // прямоугольник и отдать окну место без владельца (решения D8, D9).
                if let Some(w) = &p.workspace {
                    self.st.geom.entry(w.clone()).or_default().insert(c.address.clone(), *r);
                    claim_owner(&mut self.st, &self.cfg, w, &p.app, &c.address);
                }
            }
            Target::Free(rect) => {
                if c.desktop() != Some(p.desktop) {
                    ex.push(hypr::d_move_to(&c.address, &p.desktop.to_string()));
                }
                let r = rect.unwrap_or_else(|| center_rect(c, self.mon));
                ex.extend(hypr::d_place(&c.address, r));
                match &joining {
                    Some(w) => match share_record(&self.cfg_file, &self.st.extra, &p.app, r) {
                        Some(e) => {
                            log::info!("workspace {w}: приложение {} принято записью о месте вместе с окном {}", p.app, c.address);
                            self.remember_extra(w, &p.app, e);
                        }
                        None => log::warn!("workspace {w}: приложение {} нечем описать", p.app),
                    },
                    None => {
                        let (cmd, cwd) = proc_info(c.pid);
                        self.st.foreign.insert(c.address.clone(), Foreign { rect: r, cmd, cwd });
                    }
                }
            }
        }
        if p.focus {
            ex.push(hypr::d_focus_window(&c.address));
            ex.push(hypr::d_bring_to_top());
        } else if let Some(ws) = &p.workspace
            && p.desktop == self.current
        {
            // Новое окно забирает фокус у композитора; возвращаем его главному окну workspace.
            let main = self.st.main_app(&self.cfg, ws, self.mon);
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
    /// композитор, а отбирать его у окна, которое пользователь только что
    /// открыл, незачем.
    fn capture_new(&mut self, c: &Client, app: &str, clients: &[Client], place: Option<PxRect>, ws: Option<&str>) -> Result<()> {
        let cfg = self.cfg.clone();
        let num = free_instance(clients, app);
        // Окно входит в workspace, активный на его столе (изменение
        // shared-windows, решение D2).
        let mut ex = entry_tags(&c.address, app, num, ws);
        let ws = ws.map(String::from);
        // Место ищется только для окна на обычном столе: окно, открытое сразу
        // на `special:pool` или `special:hidden`, остаётся там, где открылось.
        // Готовое место приходит от принятия окна в workspace: там оно уже
        // посчитано и записано дополнительным приложением сессии.
        let rect = place.or_else(|| c.desktop().and_then(|_| open_rect(&mut self.st, &cfg, ws.as_deref(), app, self.mon)));
        if let Some(r) = rect {
            ex.extend(hypr::d_place(&c.address, r));
        }
        // Прямоугольник окна запоминается сразу в обоих режимах (изменение
        // live-layout, решение D8): ушедшее на `special:pool` и вернувшееся
        // окно встанет туда же. Изменённое место без владельца (раскладка
        // из снимка) достаётся этому окну (решение D9).
        if let Some(w) = ws.clone().filter(|_| c.desktop().is_some()) {
            self.st.geom.entry(w.clone()).or_default().insert(c.address.clone(), rect.unwrap_or_else(|| c.rect()));
            claim_owner(&mut self.st, &cfg, &w, app, &c.address);
        }
        log::info!("новое окно {} ({}, «{}») → приложение {app}, экземпляр {num}, workspace {}", c.address, c.class, c.title, ws.as_deref().unwrap_or("нет"));
        self.hypr.dispatch_all(&ex)
    }

    /// Выполнить решение о принятии окна (решения D1 и D2).
    fn join(&mut self, c: &Client, clients: &[Client], ws: Option<&str>, join: Join, cmd: Vec<String>, cwd: Option<String>) -> Result<()> {
        match join {
            Join::App(app) => {
                self.st.foreign.remove(&c.address);
                self.capture_new(c, &app, clients, None, ws)
            }
            Join::AppPlace(app) => {
                let w = ws.unwrap_or_default().to_string();
                let cfg = self.cfg.clone();
                let rect = open_rect(&mut self.st, &cfg, Some(&w), &app, self.mon).unwrap_or_else(|| center_rect(c, self.mon));
                self.remember_extra(&w, &app, ExtraApp { rect, ..ExtraApp::default() });
                self.st.foreign.remove(&c.address);
                log::info!("окно {} ({}) принято в workspace {w} приложением конфига {app}", c.address, c.class);
                self.capture_new(c, &app, clients, Some(rect), Some(&w))
            }
            Join::Extra(name) => {
                let w = ws.unwrap_or_default().to_string();
                let rect = center_rect(c, self.mon);
                self.remember_extra(&w, &name, ExtraApp { class: Some(c.class.clone()), cmd, cwd, rect });
                self.st.foreign.remove(&c.address);
                log::info!("окно {} ({}) принято в workspace {w} дополнительным приложением сессии {name}", c.address, c.class);
                self.capture_new(c, &name, clients, Some(rect), Some(&w))
            }
            Join::Free(reason) => {
                self.st.foreign.insert(c.address.clone(), Foreign { rect: c.rect(), cmd, cwd });
                log::info!("окно {} ({}, «{}») свободно: {}", c.address, c.class, c.title, reason.text());
                Ok(())
            }
        }
    }

    /// Записать дополнительное приложение сессии и пересобрать эффективный
    /// конфиг: после этого приложение живёт наравне с приложениями файла.
    fn remember_extra(&mut self, ws: &str, name: &str, e: ExtraApp) {
        self.st.extra.entry(ws.to_string()).or_default().insert(name.to_string(), e);
        self.rebuild_cfg();
    }

    /// Убрать активное окно из активного workspace текущего стола
    /// (спецификация ws-daemon, «Отделение окна»; изменение shared-windows,
    /// решение D9): снять тег состава. Окно, входящее ещё в другие workspace,
    /// уходит по правилу размещения; окно, для которого этот workspace был
    /// последним, закрывается диспетчером композитора.
    pub fn detach(&mut self) -> Result<()> {
        let n = self.sync_current();
        let clients = self.hypr.clients()?;
        let active = self.hypr.active_window()?.filter(|c| c.desktop() == Some(n));
        let ws = self.st.desktops.get(&n).and_then(|d| d.active.clone());
        let extra = ws.as_deref().and_then(|w| self.st.extra.get(w)).cloned().unwrap_or_default();
        let desks = actives(&self.st);
        let step = detach_step(&self.cfg, &clients, ws.as_deref(), &extra, active.as_ref(), &desks, n);
        let (Some(c), Some(w)) = (active, ws) else {
            if let DetachStep::Skip(why) = step {
                log::info!("отделение окна: {why}");
            }
            return Ok(());
        };
        let addr = c.address.clone();
        // Окно уходит со стола своего workspace: раскладка снимается до этого,
        // и место, которое окно оставляет, остаётся у остальных окон
        // приложения (решения D4, D9).
        self.absorb(&w, n, &clients);
        let (app, drop_extra, last) = match &step {
            DetachStep::Skip(why) => {
                log::info!("отделение окна: {why}");
                return Ok(());
            }
            DetachStep::Leave { app, drop_extra, .. } => (app.clone(), *drop_extra, false),
            DetachStep::Close { app, drop_extra, last } => (app.clone(), *drop_extra, *last),
        };
        if drop_extra {
            let from = drop_extra_app(&mut self.st, &w, &app, last);
            self.rebuild_cfg();
            log::info!("дополнительное приложение сессии {app} снято вместе с последним его окном в workspace: {}", from.join(", "));
        }
        if let Some(g) = self.st.geom.get_mut(&w) {
            g.remove(&addr);
        }
        let mut after = clients.clone();
        if let Some(x) = after.iter_mut().find(|x| x.address == addr) {
            x.tags.retain(|t| hypr::parse_ws_tag(t).as_deref() != Some(w.as_str()));
        }
        self.drop_moved_of(&addr, &after, "окно отделено");
        match step {
            DetachStep::Leave { home, .. } => {
                let mut ex = vec![hypr::d_untag(&addr, &hypr::ws_tag(&w))];
                let rest: Vec<String> = c.workspaces().into_iter().filter(|x| *x != w).collect();
                match home {
                    Home::Desktop(k) if k != n => self.send_to(&c, k, &desks, &mut ex),
                    Home::Pool => ex.push(hypr::d_move_to(&addr, "special:pool")),
                    _ => {}
                }
                log::info!("workspace {w}: окно {addr} приложения {app} отделено и остаётся в {}", rest.join(", "));
                self.hypr.dispatch_all(&ex)?;
            }
            _ => {
                log::info!("workspace {w}: окно {addr} приложения {app} отделено и закрыто");
                self.hypr.dispatch_all(&[hypr::d_untag(&addr, &hypr::ws_tag(&w)), hypr::d_close(&addr)])?;
            }
        }
        self.broadcast();
        Ok(())
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
        if let Some(i) = self.st.restore.iter().position(|e| e.state == RestoreState::Launched(pid)) {
            let e = &mut self.st.restore[i];
            e.state = RestoreState::Done;
            log::warn!("восстановление экземпляра {}#{}: процесс {pid} ({:?}) завершился без окна, запись снята", e.app, e.instance, e.cmd);
            return Ok(());
        }
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
        let child = start(&cmd, &args, cwd.as_deref(), &env).with_context(|| format!("приложение {app}"))?;
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
    /// у приложения, окна которого уже есть. С `ws` захват идёт для
    /// workspace: захваченное окно получает и тег его состава, а свободные
    /// окна приложения с тегом экземпляра входят в workspace, сохраняя номер
    /// (изменение shared-windows, решения D2, D3). Теги переставляются
    /// в композиторе и в локальном списке клиентов. Возвращает число
    /// захваченных окон.
    fn adopt_untagged(&mut self, clients: &mut [Client], apps: &[String], ws: Option<&str>) -> Result<usize> {
        let plan = adopt_plan(&self.cfg, clients, apps, ws.is_some())?;
        for cap in &plan {
            let i = cap.idx;
            let addr = clients[i].address.clone();
            let mut ex: Vec<String> = Vec::new();
            if cap.retag {
                let tag = format!("app:{}#{}", cap.app, cap.num);
                ex.extend(clients[i].app_tags().map(|t| hypr::d_untag(&addr, t)));
                ex.push(hypr::d_tag(&addr, &tag));
                clients[i].tags.retain(|t| !t.starts_with("app:"));
                clients[i].tags.push(tag);
            }
            if let Some(w) = ws {
                ex.push(hypr::d_tag(&addr, &hypr::ws_tag(w)));
                clients[i].tags.push(hypr::ws_tag(w));
            }
            self.hypr.dispatch_all(&ex)?;
            // Захваченное окно больше не постороннее.
            self.st.foreign.remove(&addr);
            log::info!("захват: окно {addr} ({}, «{}») → приложение {}, экземпляр {}, workspace {}", clients[i].class, clients[i].title, cap.app, cap.num, ws.unwrap_or("нет"));
        }
        Ok(plan.len())
    }

    /// Отдать workspace `ws` в общее пользование окна других workspace
    /// (решение D3, `share_plan`): окна получают тег состава `ws` и не
    /// теряют прежних.
    fn share_windows(&mut self, clients: &mut [Client], ws: &str, apps: &[String], skip: &[String]) -> Result<()> {
        let mut plan = share_plan(&self.cfg, clients, ws, apps);
        // Приложения, чьи экземпляры восстанавливаются для `ws`, окон других
        // workspace не получают (изменение session-instances, решение D11).
        plan.retain(|(_, app)| !skip.contains(app));
        let mut ex = Vec::new();
        for (i, app) in &plan {
            let c = &mut clients[*i];
            ex.push(hypr::d_tag(&c.address, &hypr::ws_tag(ws)));
            log::info!("workspace {ws}: окно {} ({}) приложения {app} из {} становится общим", c.address, c.class, c.workspaces().join(", "));
            c.tags.push(hypr::ws_tag(ws));
        }
        self.hypr.dispatch_all(&ex)
    }

    /// Поднять workspace на столе (design D5).
    /// Стол, на котором workspace сейчас активен.
    fn active_desktop_of(&self, ws: &str) -> Option<u8> {
        self.st.desktops.iter().find(|(_, d)| d.active.as_deref() == Some(ws)).map(|(n, _)| *n)
    }

    pub fn raise(&mut self, ws: &str, desktop: Option<u8>) -> Result<()> {
        self.raise_on(ws, desktop, true)
    }

    /// Поднятие workspace. `focus` — перейти на стол и отдать фокус главному
    /// окну; без него окна встают на стол, но композитор остаётся на прежнем
    /// столе и фокус не меняется (преемник после переноса workspace).
    ///
    /// Сначала собирается состав (решение D3): свободные окна приложений
    /// захватываются, а приложению без окон в workspace достаются окна других
    /// workspace. Затем каждое окно workspace и каждое окно других workspace
    /// на целевом столе встаёт по правилу размещения (`window_home`, решение
    /// D4) для столов после поднятия.
    fn raise_on(&mut self, ws: &str, desktop: Option<u8>, focus: bool) -> Result<()> {
        if !self.cfg.workspaces.contains_key(ws) {
            bail!("workspace {ws} не найден");
        }
        // Без явного стола workspace, уже активный на другом столе, не переезжает:
        // демон переходит на тот стол и отдаёт фокус главному окну.
        let was_on = self.active_desktop_of(ws);
        let n = desktop.unwrap_or_else(|| was_on.unwrap_or(self.current));
        // Перенос — это поднятие workspace, уже активного на другом столе.
        // При переносе порядок окон по глубине сохраняется как есть, и главное
        // окно наверх не поднимается: сверху пользователь оставил то, с чем
        // работал, и смена стола этого менять не должна.
        let moving_ws = was_on.is_some_and(|d| d != n);
        // Ленивое поднятие для этого стола больше не нужно: workspace там
        // поднимает эта команда. Иначе событие смены стола пришло бы следом
        // и отдало фокус главному окну поверх того, что сделала команда.
        self.st.lazy.remove(&n);
        let w = self.cfg.workspaces[ws].clone();
        let apps: Vec<String> = w.apps.keys().cloned().collect();
        // Экземпляры снимка с командной строкой, входящие в `ws`, запускаются
        // раньше захвата и запуска приложений (решения D4, D11).
        let launch: Vec<usize> = self.st.restore.iter().enumerate().filter(|(_, e)| e.state == RestoreState::Waiting && !e.cmd.is_empty() && e.workspaces.iter().any(|x| x == ws)).map(|(i, _)| i).collect();
        for i in launch {
            self.spawn_instance(i);
        }
        let mut clients = self.hypr.clients()?;
        self.adopt_untagged(&mut clients, &apps, Some(ws))?;
        let steps: BTreeMap<String, RaiseStep> = {
            let cfg = &self.cfg;
            apps.iter().map(|a| (a.clone(), raise_restore_step(cfg, &self.st.restore, a, ws, !placed_windows(cfg, &clients, ws, &apps, a).is_empty()))).collect()
        };
        let held: Vec<String> = steps.iter().filter(|(_, s)| **s != RaiseStep::Normal).map(|(a, _)| a.clone()).collect();
        self.share_windows(&mut clients, ws, &apps, &held)?;
        let before = actives(&self.st);
        // Раскладка снимается с экрана до того, как окна уйдут со столов
        // (изменение live-layout, решение D4): у workspace, активного
        // на целевом столе (вытесняемого или того же), и у переносимого
        // с прежнего стола. Следующее поднятие вернёт окна туда, где их
        // оставили, в обоих режимах.
        if let Some(old) = before.get(&n).cloned() {
            self.absorb(&old, n, &clients);
        }
        if let Some(k) = was_on.filter(|k| *k != n) {
            self.absorb(ws, k, &clients);
        }
        // Активные workspace столов после поднятия: по ним правило размещения
        // решает, где стоять окнам.
        let mut after = before.clone();
        after.retain(|_, x| x != ws);
        after.insert(n, ws.to_string());
        let mut ex = Vec::new();
        if focus && n != self.current {
            ex.push(hypr::d_focus_desktop(n));
            self.current = n;
        }
        let cur = self.current;
        let cfg = self.cfg.clone();
        let mut main_addr: Option<String> = None;
        // Окна workspace, которым демон задаёт порядок по глубине.
        let mut members: Vec<String> = Vec::new();
        let main_app = self.st.main_app(&cfg, ws, self.mon);
        for app in &apps {
            let rect = self.st.rect_for(&cfg, ws, app, self.mon);
            // Переезжают и расставляются окна всех экземпляров приложения,
            // входящие в workspace, окна его вариантов в том числе; скрытые
            // пользователем не трогаются.
            let wins = placed_windows(&cfg, &clients, ws, &apps, app);
            if wins.is_empty() {
                match steps.get(app).copied().unwrap_or(RaiseStep::Normal) {
                    RaiseStep::Wait => {
                        log::info!("workspace {ws}: окно экземпляра {app}, запущенного по снимку, ещё не появилось — приложение не запускается");
                        continue;
                    }
                    RaiseStep::LaunchFirst => {
                        log::info!("workspace {ws}: первый экземпляр {app} из снимка запускается командой приложения");
                        self.st.restore_apps.insert(app.clone());
                    }
                    RaiseStep::Normal => {}
                }
                let target = rect.map(Target::Place).unwrap_or(Target::Free(None));
                // Сбой запуска одного приложения поднятие не обрывает: его место
                // остаётся пустым, остальные приложения встают, workspace
                // становится активным (изменение classless-windows-stay-free,
                // решение D2).
                if let Err(e) = self.spawn(app, Some(ws), n, target, focus && main_app.as_deref() == Some(app)) {
                    log::warn!("workspace {ws}: {e:#}; поднятие продолжается без этого приложения");
                }
                continue;
            }
            for c in wins.iter().filter(|c| !c.on_hidden()) {
                match window_home(&c.workspaces(), &after, cur, Spot::of(c)) {
                    Home::Desktop(k) if k == n => {}
                    Home::Desktop(k) => {
                        // Поднятие без перехода (преемник) не уводит общее окно
                        // со стола, где находится пользователь (решение D10).
                        if c.desktop() != Some(k) {
                            self.send_to(c, k, &after, &mut ex);
                        }
                        continue;
                    }
                    _ => continue,
                }
                members.push(c.address.clone());
                let moving = c.desktop() != Some(n);
                // Общее окно, стоявшее на столе в составе вытесняемого
                // workspace, приходит в поднимаемый так же, как окно с другого
                // стола: встаёт в состояние, запомненное для него (решение D13).
                let arriving = arrives(c, n, &before, ws);
                if moving {
                    // Окно уходит со стола другого своего workspace: его
                    // раскладку снимает `absorb` до ухода.
                    if let Some(k) = c.desktop()
                        && let Some(v) = before.get(&k).filter(|v| *v != ws && c.in_ws(v))
                    {
                        let v = v.clone();
                        self.absorb(&v, k, &clients);
                    }
                    ex.push(hypr::d_move_to(&c.address, &n.to_string()));
                }
                // Окно, которое едет вместе с workspace с прежнего стола,
                // сохраняет прямоугольник, прочитанный у композитора до переноса.
                let carried = moving_ws && c.desktop() == was_on;
                let kept = self.st.geom.get(ws).and_then(|g| g.get(&c.address)).copied();
                let target = raise_target(carried, arriving, kept, rect, c.rect());
                if let Some(r) = target {
                    ex.extend(hypr::d_place(&c.address, r));
                }
                self.st.geom.entry(ws.to_string()).or_default().insert(c.address.clone(), target.unwrap_or_else(|| c.rect()));
                if target.is_some() && target == rect {
                    claim_owner(&mut self.st, &cfg, ws, app, &c.address);
                }
                if main_app.as_deref() == Some(app) && main_addr.is_none() {
                    // Фокус получает первый экземпляр; наверх он поднимается
                    // при поднятии workspace, но не при переносе на другой стол.
                    main_addr = Some(c.address.clone());
                }
            }
        }
        // Окна других workspace уходят со стола по правилу размещения: на стол,
        // где активен другой их workspace, либо на `special:pool`. Свободные
        // окна и окна с тегом приложения, которого нет в конфиге, остаются.
        for c in &clients {
            if c.desktop() != Some(n) || c.in_ws(ws) || !c.has_ws() {
                continue;
            }
            match window_home(&c.workspaces(), &after, cur, Spot::of(c)) {
                Home::Desktop(k) if k != n => self.send_to(c, k, &after, &mut ex),
                Home::Pool => ex.push(hypr::d_move_to(&c.address, "special:pool")),
                _ => {}
            }
        }
        if let Some(a) = main_addr.as_ref().filter(|_| focus) {
            ex.push(hypr::d_focus_window(a));
        }
        // Порядок окон по глубине задаётся явно, окно за окном снизу вверх.
        // Он прочитан у композитора до переноса и восстанавливается после него,
        // поэтому порядок не зависит от того, как переносы влияют на стопку.
        // Диспетчер `bring_to_top` здесь не годится: он действует на активное
        // окно, а нужен произвольный адрес.
        let depth: Vec<String> = clients.iter().map(|c| c.address.clone()).filter(|a| members.contains(a)).collect();
        for a in depth_plan(&depth, main_addr.as_deref(), !moving_ws) {
            ex.push(hypr::d_raise(&a));
        }
        assign_desktop(&mut self.st, ws, n);
        self.hypr.dispatch_all(&ex)?;
        self.broadcast();
        Ok(())
    }

    /// Увести окно на стол `k`, где активен другой его workspace, и поставить
    /// в состояние, запомненное для того workspace (решение D6). Раскладку
    /// workspace, со стола которого окно уходит, вызывающий снимает заранее
    /// (изменение live-layout, решение D4). Диспетчеры дописываются в `ex`.
    fn send_to(&mut self, c: &Client, k: u8, active: &BTreeMap<u8, String>, ex: &mut Vec<String>) {
        ex.push(hypr::d_move_to(&c.address, &k.to_string()));
        let Some(v) = active.get(&k).cloned() else { return };
        let cfg = self.cfg.clone();
        let r = arrive_rect(&mut self.st, &cfg, &v, c, self.mon);
        if let Some(r) = r {
            ex.extend(hypr::d_place(&c.address, r));
        }
        self.st.geom.entry(v.clone()).or_default().insert(c.address.clone(), r.unwrap_or_else(|| c.rect()));
        log::info!("окно {} ({}) уходит на стол {k} к workspace {v}", c.address, c.class);
    }

    /// Последнее окно workspace, получавшее фокус. Берётся из события
    /// `activewindow`: опроса нет, сведение приходит от композитора. Из него
    /// выводится prev, когда цикл начинается с чужого окна.
    fn on_focus(&mut self) -> Result<()> {
        let Some(c) = self.hypr.active_window()? else { return Ok(()) };
        let Some(n) = c.desktop() else { return Ok(()) };
        let Some(ws) = self.st.desktops.get(&n).and_then(|d| d.active.clone()) else { return Ok(()) };
        let ws_apps = self.ws_apps(&ws);
        if ws_app_of(&self.cfg, &ws, &ws_apps, &c).is_some() {
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

    /// Снять раскладку workspace `ws` со стола `n` (изменение live-layout,
    /// решения D2–D4, `absorb_into`): изменённые места приложений и
    /// прямоугольники окон читаются из списка окон, который действие и так
    /// получило у композитора. На столе, где `ws` не активен, ничего не делает.
    pub fn absorb(&mut self, ws: &str, n: u8, clients: &[Client]) {
        let cfg = self.cfg.clone();
        absorb_into(&cfg, &mut self.st, ws, n, clients, self.mon);
    }

    /// Вернуть к описанию изменённые места, снятые с окна `addr`
    /// (`drop_moved`), со строкой в журнале о каждом снятом месте.
    fn drop_moved_of(&mut self, addr: &str, clients: &[Client], why: &str) {
        let cfg = self.cfg.clone();
        for (ws, entry) in drop_moved(&mut self.st, &cfg, addr, clients) {
            log::info!("{ws}: {why} ({addr}), окон {entry} в workspace не осталось — место {entry} возвращается к описанию");
        }
    }

    /// Применить раскладку `ws`, собранную заново после правки конфига,
    /// на столе `n`, где он активен (изменение live-layout, решение D12):
    /// окна `ws` на этом столе встают на места из новой раскладки, фокус
    /// и порядок по глубине не меняются, прямоугольники запоминаются.
    fn apply_layout(&mut self, ws: &str, n: u8) -> Result<()> {
        let clients = self.hypr.clients()?;
        let cfg = self.cfg.clone();
        let ws_apps = self.ws_apps(ws);
        let mut ex = Vec::new();
        let mut count = 0;
        for c in ws_windows(&clients, ws).into_iter().filter(|c| c.desktop() == Some(n) && c.fullscreen == 0) {
            let Some(app) = ws_app_of(&cfg, ws, &ws_apps, c) else { continue };
            let Some(r) = self.st.rect_for(&cfg, ws, &app, self.mon) else { continue };
            self.st.geom.entry(ws.to_string()).or_default().insert(c.address.clone(), r);
            if c.rect() != r {
                ex.extend(hypr::d_place(&c.address, r));
                count += 1;
            }
        }
        self.hypr.dispatch_all(&ex)?;
        log::info!("{ws}: раскладка собрана заново из конфига и применена на столе {n}, сдвинуто окон {count}");
        Ok(())
    }

    /// Обмен мест (изменение live-layout, решение D5): вызванное приложение
    /// и главное меняются местами целиком — исходным местом и изменённым
    /// прямоугольником, — вызванное становится главным, окна обоих встают
    /// в прямоугольники новых мест стопкой. Остальные окна не двигаются.
    /// Раскладку перед обменом снимает цикл. Диспетчеры дописываются в `ex`,
    /// чтобы обмен и фокус ушли одной последовательностью.
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
        self.st.cells_of(&cfg, ws, self.mon);
        let has_windows = |name: &str| !placed_windows(&cfg, clients, ws, &ws_apps, name).is_empty();
        swap_places(&mut self.st, ws, main_app.as_deref(), app, &main_cell, &has_windows);
        let n = self.current;
        for name in [main_app.as_deref(), Some(app)].into_iter().flatten() {
            let Some(r) = self.st.rect_for(&cfg, ws, name, self.mon) else { continue };
            let wins: Vec<&Client> = placed_windows(&cfg, clients, ws, &ws_apps, name).into_iter().filter(|c| !c.on_hidden()).collect();
            for c in &wins {
                if c.desktop() != Some(n) {
                    ex.push(hypr::d_move_to(&c.address, &n.to_string()));
                }
                ex.extend(hypr::d_place(&c.address, r));
                self.st.geom.entry(ws.to_string()).or_default().insert(c.address.clone(), r);
            }
            // Изменённое место теперь занимает окно этого приложения: его
            // закрытие и вернёт место к описанию (решение D9).
            if let (Some(m), Some(c)) = (self.st.moved.get_mut(ws).and_then(|m| m.get_mut(name)), wins.first()) {
                m.owner = Some(c.address.clone());
            }
        }
    }

    /// Выбрать окно (спецификация ws-daemon, «Цепочка приложения»): при
    /// `swap` — режим обмена мест вызванного приложения — приложение окна
    /// сначала занимает главное место; без него окно только поднимается
    /// наверх и получает фокус. Режим задаёт вызванное приложение, а не
    /// workspace (изменение workspace-overrides, решение D7); без обмена
    /// выбирается и окно, только что открытое или перетащенное в workspace
    /// (изменение shared-windows, решение D15).
    fn select_window(&mut self, ws: &str, addr: &str, clients: &[Client], swap: bool) -> Result<()> {
        let cfg = self.cfg.clone();
        let ws_apps = self.ws_apps(ws);
        let mut ex = Vec::new();
        if swap
            && let Some(c) = clients.iter().find(|c| c.address == addr)
            && let Some(app) = ws_app_of(&cfg, ws, &ws_apps, c)
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
    /// «Цепочка приложения»). `start` говорит, начат ли цикл этой же цепочкой:
    /// workspace поднят (`Raised`) или приложение открыто либо перетащено
    /// в него (`Joined`), и тогда цикл начинается заново.
    fn cycle(&mut self, ws: &str, app: &str, start: Start) -> Result<()> {
        let fresh = start != Start::Continue;
        let mut clients = self.hypr.clients()?;
        self.adopt_untagged(&mut clients, std::slice::from_ref(&app.to_string()), Some(ws))?;
        // Раскладка снимается с экрана перед выбором шага (изменение
        // live-layout, решение D4): сдвинутое окно отдаёт свой прямоугольник
        // месту своего приложения, и обмен мест идёт уже по нему.
        let n = self.current;
        self.absorb(ws, n, &clients);
        let cfg = self.cfg.clone();
        let ws_apps = self.ws_apps(ws);
        // Весь цикл идёт в режиме вызванного приложения (решение D7), а обмен
        // мест переносит запись, которая его описывает: у варианта без своей
        // записи это запись семейства (решение D6).
        let (mode, entry) = cycle_mode(&cfg, ws, app);
        let swap = mode == Mode::Swap;
        let wins = placed_windows(&cfg, &clients, ws, &ws_apps, app);
        let order: Vec<String> = wins.iter().filter(|c| !c.on_hidden()).map(|c| c.address.clone()).collect();
        if order.is_empty() && !wins.is_empty() {
            // Все окна приложения спрятаны пользователем на `special:hidden`:
            // второй экземпляр не запускается, показывать нечего.
            log::info!("{app}: все окна скрыты, цикл пропущен");
            return Ok(());
        }
        let active = self.hypr.active_window()?.map(|c| c.address);
        let on_instance = active.as_deref().is_some_and(|a| order.iter().any(|x| x == a));
        // В режиме обмена мест нажатие клавиши неглавного приложения начинает
        // цикл заново (изменение live-layout, решение D7): выбрать экземпляр —
        // значит поставить приложение на главное место. Нажатия, которое
        // открыло или перетащило приложение, правило не касается.
        let is_main = self.st.main_app(&cfg, ws, self.mon).as_deref() == Some(entry.as_str());
        let restart = if swap && start != Start::Joined { swap_start(&order, active.as_deref(), is_main) } else { None };
        if fresh || !on_instance || restart.is_some() {
            // Начало цикла: запомнить окно, к которому вернёт его конец.
            let main_window = swap
                .then(|| self.st.main_app(&cfg, ws, self.mon))
                .flatten()
                .filter(|m| *m != entry)
                .and_then(|m| placed_windows(&cfg, &clients, ws, &ws_apps, &m).into_iter().find(|c| !c.on_hidden()).map(|c| c.address.clone()));
            let focus = self.st.focus.get(ws).cloned();
            let back = anchor_window(&cfg, &clients, ws, main_window.as_deref(), &order, active.as_deref(), focus.as_deref());
            self.st.cycle.insert(ws.to_string(), Cycle { app: app.to_string(), back });
        }
        let prev = self
            .st
            .cycle
            .get(ws)
            .filter(|c| c.app == app)
            .and_then(|c| c.back.clone())
            .filter(|a| clients.iter().any(|c| c.address == *a && !c.on_hidden()));
        let next = next_app_window(&cfg, &clients, ws, &entry);
        let step = match restart {
            Some(addr) => CycleStep::Select(addr),
            None => cycle_step(&order, active.as_deref(), fresh, prev.as_deref(), next.as_deref()),
        };
        match step {
            CycleStep::Launch => {
                // Запуск — это тоже выбор экземпляра: в режиме обмена мест
                // приложение сначала занимает главное место, и окно появляется
                // уже на нём, а прежнее главное уходит на место приложения.
                let mut ex = Vec::new();
                if swap {
                    self.swap_to_main(ws, &entry, &clients, &mut ex);
                }
                self.hypr.dispatch_all(&ex)?;
                let rect = self.st.rect_for(&cfg, ws, app, self.mon);
                self.spawn(app, Some(ws), n, rect.map(Target::Place).unwrap_or(Target::Free(None)), true)
            }
            CycleStep::Select(addr) => self.select_window(ws, &addr, &clients, swap && start != Start::Joined),
            CycleStep::Back(addr) => {
                log::info!("{ws}: цикл {app} закончен, возврат к окну {addr}");
                self.select_window(ws, &addr, &clients, swap)
            }
            CycleStep::NextApp(addr) => {
                log::info!("{ws}: цикл {app} закончен, prev нет — следующее приложение workspace, окно {addr}");
                self.select_window(ws, &addr, &clients, swap)
            }
        }
    }

    /// Цепочка приложения: цикл по экземплярам в workspace приложения
    /// (спецификация ws-daemon, «Цепочка приложения»; изменение shared-windows,
    /// решение D7). `apps` — кандидаты с одной цепочкой: действует тот из них,
    /// чей workspace найдётся раньше. `pull` — перетащить окна приложения
    /// в активный workspace текущего стола (решение D8).
    pub fn app(&mut self, apps: &[String], desktop: Option<u8>, workspace: Option<&str>, pull: bool) -> Result<()> {
        for a in apps {
            if !self.cfg.apps.contains_key(a) {
                bail!("приложение {a} не описано");
            }
        }
        // Вызов приложения — действие пользователя: окна, которые процесс
        // приложения открывает сам, больше не ждутся (решение D10).
        for a in apps {
            self.end_restore_wait(Some(a), false, "вызов приложения");
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
        let n = self.current;
        let tagged = self.tagged_in_active(n)?;
        let route = app_route(&self.cfg, &self.st.desktops, n, apps, pull, &tagged);
        self.follow_route(route, raised, apps)
    }

    /// Приложения окон, входящих в активный workspace стола `n` тегом
    /// состава: клавиша приложения ведёт к окну там, где оно сейчас есть
    /// (решение D16). Без активного workspace — пусто.
    fn tagged_in_active(&self, n: u8) -> Result<Vec<String>> {
        Ok(match self.st.desktops.get(&n).and_then(|d| d.active.clone()) {
            Some(w) => self.hypr.clients()?.iter().filter(|c| c.in_ws(&w)).filter_map(Client::app).collect(),
            None => Vec::new(),
        })
    }

    /// Клавиша приложения по нажатой цепочке (изменение workspace-overrides,
    /// решения D3, D4, D16): приложение и workspace выбираются по активному
    /// workspace текущего стола и ключам приложений в workspace. Цепочка,
    /// на которую не отзывается ни одно приложение, — строка в журнале
    /// без действия.
    pub fn key(&mut self, chain: &str) -> Result<()> {
        let parsed = crate::keys::parse_chain(chain)?;
        let chain = crate::keys::chain_compact(&parsed);
        let n = self.current;
        let tagged = self.tagged_in_active(n)?;
        let Some(route) = key_route(&self.cfg, &self.st.desktops, n, &chain, &tagged) else {
            log::info!("клавиша {chain}: ни одно приложение на неё не отзывается");
            return Ok(());
        };
        log::info!("клавиша {chain}: {route:?}");
        let app = match &route {
            AppRoute::Cycle { app, .. } | AppRoute::Raise { app, .. } | AppRoute::Pull { app, .. } | AppRoute::Open { app, .. } | AppRoute::Free { app } => app.clone(),
        };
        self.end_restore_wait(Some(&app), false, "клавиша приложения");
        self.follow_route(route, false, &[app])
    }

    /// Выполнить путь клавиши приложения. `raised` — workspace поднят этой
    /// же командой, и цикл начинается заново; `apps` — кандидаты, чьи
    /// свободные окна захватываются при цикле вне workspace.
    fn follow_route(&mut self, route: AppRoute, raised: bool, apps: &[String]) -> Result<()> {
        let n = self.current;
        match route {
            AppRoute::Cycle { ws, app } => self.cycle(&ws, &app, if raised { Start::Raised } else { Start::Continue }),
            AppRoute::Raise { ws, app, desktop } => {
                self.raise(&ws, desktop)?;
                self.cycle(&ws, &app, Start::Raised)
            }
            AppRoute::Pull { ws, app } => self.bring(&ws, &app, true),
            AppRoute::Open { ws, app } => self.bring(&ws, &app, false),
            AppRoute::Free { app } => self.cycle_free(&app, apps, n),
        }
    }

    /// Приложение вне workspace: цикл по его окнам без prev и без следующего
    /// приложения, поэтому за последним экземпляром идёт первый. Окно,
    /// которого ещё нет, открывается на своём месте по умолчанию (`rect`
    /// приложения, а без него — центр экрана).
    fn cycle_free(&mut self, app: &str, apps: &[String], n: u8) -> Result<()> {
        let mut clients = self.hypr.clients()?;
        self.adopt_untagged(&mut clients, apps, None)?;
        let wins = app_windows(&self.cfg, &clients, app);
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
                let rect = State::app_rect(&self.cfg, app, self.mon);
                self.spawn(app, None, n, Target::Free(rect), true)
            }
            CycleStep::Select(addr) | CycleStep::Back(addr) | CycleStep::NextApp(addr) => {
                self.hypr.dispatch_all(&[hypr::d_focus_window(&addr), hypr::d_bring_to_top()])
            }
        }
    }

    /// Открыть приложение в активном workspace `ws` текущего стола (случай
    /// «а», `pull = false`) или перетащить туда его окна (случай «в»,
    /// `pull = true`) — решения D7, D8. Свободные окна приложения
    /// захватываются; при открытии окна других workspace становятся общими,
    /// только если свободных не нашлось, при перетаскивании — всегда. Окна
    /// входят в `ws`, `ws` получает запись о месте приложения, окна встают
    /// на текущий стол на это место, и цикл начинается с первого экземпляра.
    /// Без окон приложение запускается для `ws`, и запись о месте заводится
    /// при появлении окна.
    fn bring(&mut self, ws: &str, app: &str, pull: bool) -> Result<()> {
        let n = self.current;
        let cfg = self.cfg.clone();
        let mut clients = self.hypr.clients()?;
        self.adopt_untagged(&mut clients, std::slice::from_ref(&app.to_string()), Some(ws))?;
        let visible = |c: &&Client| !c.on_hidden();
        let own: Vec<Client> = app_windows(&cfg, &clients, app).into_iter().filter(visible).cloned().collect();
        let in_ws: Vec<Client> = own.iter().filter(|c| c.in_ws(ws)).cloned().collect();
        let take: Vec<Client> = if pull || in_ws.is_empty() { own } else { in_ws };
        let rect = open_rect(&mut self.st, &cfg, Some(ws), app, self.mon);
        let Some(first) = take.first() else {
            log::info!("workspace {ws}: окон {app} нет, приложение запускается в нём");
            return self.spawn(app, Some(ws), n, Target::Free(rect), true);
        };
        let rect = rect.unwrap_or_else(|| center_rect(first, self.mon));
        if !ws_has_app(&cfg, ws, app) {
            match share_record(&self.cfg_file, &self.st.extra, app, rect) {
                Some(e) => {
                    log::info!("workspace {ws}: приложение {app} принято записью {}", if e.cmd.is_empty() { "о месте" } else { "с командой" });
                    self.remember_extra(ws, app, e);
                }
                None => bail!("приложение {app}: нечем описать его в workspace {ws}"),
            }
        }
        let before = actives(&self.st);
        let mut ex = Vec::new();
        for c in &take {
            if !c.in_ws(ws) {
                ex.push(hypr::d_tag(&c.address, &hypr::ws_tag(ws)));
                log::info!("workspace {ws}: окно {} ({}) приложения {app} входит в него{}", c.address, c.class, if c.has_ws() { format!(", оставаясь в {}", c.workspaces().join(", ")) } else { String::new() });
            }
            if c.desktop() != Some(n) {
                // Окно уходит со стола другого своего workspace: раскладка
                // того workspace снимается до ухода (решение D4).
                if let Some(k) = c.desktop()
                    && let Some(v) = before.get(&k).filter(|v| *v != ws && c.in_ws(v))
                {
                    let v = v.clone();
                    self.absorb(&v, k, &clients);
                }
                ex.push(hypr::d_move_to(&c.address, &n.to_string()));
            }
            ex.extend(hypr::d_place(&c.address, rect));
            self.st.geom.entry(ws.to_string()).or_default().insert(c.address.clone(), rect);
            self.st.foreign.remove(&c.address);
        }
        if let Some(c) = take.first() {
            claim_owner(&mut self.st, &cfg, ws, app, &c.address);
        }
        self.hypr.dispatch_all(&ex)?;
        self.cycle(ws, app, Start::Joined)
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
    /// на `special:pool` по общему правилу. На прежнем столе активным становится
    /// преемник (`successor`): его окна возвращаются на тот стол, а композитор
    /// остаётся на столе `n`.
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
                // Преемник на прежнем столе выбирается по списку и истории
                // до переноса: перенос убирает workspace из списка.
                let next = self.st.desktops.get(&cur).and_then(|d| successor(&d.workspaces, &ws, self.st.prev_active.get(&cur).map(String::as_str)));
                // Окна встают на новом столе туда, где стояли на прежнем,
                // в обоих режимах: это делает `raise` (`raise_target`).
                log::info!("перенос workspace {ws} со стола {cur} на стол {n}");
                self.raise(&ws, Some(n))?;
                // Преемник поднимается сразу, без перехода: его окна
                // возвращаются на прежний стол, а композитор и фокус остаются
                // у перенесённого workspace.
                match next {
                    Some(p) => {
                        log::info!("стол {cur}: активным становится {p}");
                        self.raise_on(&p, Some(cur), false)
                    }
                    None => {
                        log::info!("стол {cur}: других workspace нет, активного не остаётся");
                        Ok(())
                    }
                }
            }
        }
    }

    /// Расставить окна текущего стола по описанию активного workspace
    /// (спецификация ws-daemon, «Расстановка по команде»). Назначение мест
    /// и главное приложение команда не меняет: окна встают на исходные места,
    /// которые назначены сейчас.
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
        // Расстановка возвращает раскладку к описанию (изменение live-layout,
        // решение D10): изменённые места снимаются, запомненные прямоугольники
        // окон заменяются местами плана; обмен мест сохраняется.
        let plan = arrange_layout(&mut self.st, &cfg, ws.as_deref(), &windows, self.mon);
        let mut ex: Vec<String> = plan.iter().flat_map(|(addr, r)| hypr::d_place(addr, *r)).collect();
        // Фокус остаётся у активного окна, а само окно поднимается наверх своей стопки.
        if let Some(a) = self.hypr.active_window()?.filter(|c| c.desktop() == Some(n)).map(|c| c.address) {
            ex.push(hypr::d_focus_window(&a));
            ex.push(hypr::d_bring_to_top());
        }
        self.hypr.dispatch_all(&ex)?;
        log::info!("расстановка на столе {n}: окон {}, workspace {}", plan.len(), ws.as_deref().unwrap_or("нет"));
        self.broadcast();
        Ok(())
    }

    /// Следующий workspace в списке текущего стола.
    pub fn next(&mut self) -> Result<()> {
        let d = self.st.desktop(self.current).clone();
        let Some(ws) = next_ws(&d) else {
            bail!("на столе {} нет workspace", self.current);
        };
        self.raise(&ws, None)
    }

    /// Убрать workspace из списка стола (по умолчанию текущего). Окна
    /// активного workspace уходят со стола по правилу размещения (решение
    /// D10): общее окно — на стол, где активен другой его workspace, остальные
    /// на `special:pool`.
    pub fn remove(&mut self, ws: &str, desktop: Option<u8>) -> Result<()> {
        let n = desktop.unwrap_or(self.current);
        let was_active = self.st.desktops.get(&n).and_then(|d| d.active.as_deref()) == Some(ws);
        // Окна уходят со стола: раскладка снимается, пока workspace ещё
        // активен (решение D4).
        let clients = if was_active { Some(self.hypr.clients()?) } else { None };
        if let Some(clients) = &clients {
            self.absorb(ws, n, clients);
        }
        let d = self.st.desktop(n);
        d.workspaces.retain(|w| w != ws);
        if let Some(clients) = clients {
            self.st.desktop(n).active = None;
            let after = actives(&self.st);
            let cur = self.current;
            let mut ex = Vec::new();
            for c in ws_windows(&clients, ws).into_iter().filter(|c| c.desktop() == Some(n)) {
                match window_home(&c.workspaces(), &after, cur, Spot::of(c)) {
                    Home::Desktop(k) if k != n => self.send_to(c, k, &after, &mut ex),
                    Home::Pool => ex.push(hypr::d_move_to(&c.address, "special:pool")),
                    _ => {}
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
        let child = start(prog, args, cwd, &BTreeMap::new())?;
        let pid = child.id();
        watch_child(child, self.tx.clone());
        Ok(pid)
    }

    // ---- Восстановление экземпляров ----------------------------------------------

    /// Диспетчеры, которыми окно становится экземпляром записи снимка
    /// (решения D6, D7, D13): тег экземпляра с номером записи, если он
    /// свободен (`retag`; без него номер окна сохраняется — загрузка сессии),
    /// теги состава ровно тех workspace записи, которые описывают приложение
    /// (лишние снимаются), место по `restore_place`, прямоугольники записи —
    /// в память окна по всем его workspace (изменение live-layout, решение
    /// D13). Возвращает диспетчеры, куда окно ушло и номер.
    fn instance_ex(&mut self, c: &Client, clients: &[Client], e: &RestoreEntry, retag: bool) -> (Vec<String>, Home, u32) {
        let cfg = self.cfg.clone();
        let addr = c.address.clone();
        let mut ex = Vec::new();
        let num = if retag { restored_number(clients, &e.app, e.instance) } else { c.app_instance().map(|(_, n)| n).unwrap_or(e.instance) };
        if retag {
            ex.extend(c.app_tags().map(|t| hypr::d_untag(&addr, t)));
            ex.push(hypr::d_tag(&addr, &format!("app:{}#{num}", e.app)));
        }
        let members = entry_members(&cfg, e);
        for w in c.workspaces().into_iter().filter(|w| !members.contains(w)) {
            ex.push(hypr::d_untag(&addr, &hypr::ws_tag(&w)));
        }
        for w in members.iter().filter(|w| !c.in_ws(w)) {
            ex.push(hypr::d_tag(&addr, &hypr::ws_tag(w)));
        }
        let act = actives(&self.st);
        let (home, rect) = restore_place(&mut self.st, &cfg, e, &act, self.current, self.mon);
        match home {
            Home::Desktop(k) => {
                if c.desktop() != Some(k) {
                    ex.push(hypr::d_move_to(&addr, &k.to_string()));
                }
                if let Some(r) = rect {
                    ex.extend(hypr::d_place(&addr, r));
                }
            }
            Home::Pool => ex.push(hypr::d_move_to(&addr, "special:pool")),
            Home::Keep => {}
        }
        for (w, r) in e.rects.iter().filter(|(w, _)| members.contains(w)) {
            self.st.geom.entry(w.clone()).or_default().insert(addr.clone(), *r);
        }
        if let Home::Desktop(k) = home
            && let Some(w) = act.get(&k).filter(|w| members.contains(*w))
        {
            self.st.geom.entry(w.clone()).or_default().insert(addr.clone(), rect.unwrap_or_else(|| c.rect()));
            claim_owner(&mut self.st, &cfg, w, &e.app, &addr);
        }
        self.st.foreign.remove(&addr);
        (ex, home, num)
    }

    /// Фокус главному окну активного workspace стола, куда встало окно,
    /// если это стол композитора: новое окно забирает фокус у композитора,
    /// а восстановление фокус не меняет (решение D7).
    fn focus_main_ex(&mut self, home: Home, app: &str, clients: &[Client]) -> Vec<String> {
        let Home::Desktop(k) = home else { return Vec::new() };
        if k != self.current {
            return Vec::new();
        }
        let Some(ws) = self.st.desktops.get(&k).and_then(|d| d.active.clone()) else { return Vec::new() };
        let main = self.st.main_app(&self.cfg, &ws, self.mon);
        match main.filter(|m| m != app).and_then(|m| first_window(&self.cfg, clients, &m).filter(|w| w.desktop() == Some(k)).map(|w| w.address.clone())) {
            Some(a) => vec![hypr::d_focus_window(&a), hypr::d_bring_to_top()],
            None => Vec::new(),
        }
    }

    /// Новое окно исполняет запись плана номер `i` (решения D5–D7).
    fn restore_new(&mut self, c: &Client, clients: &[Client], i: usize, how: &str) -> Result<()> {
        let e = self.st.restore[i].clone();
        self.st.restore[i].state = RestoreState::Done;
        let (mut ex, home, num) = self.instance_ex(c, clients, &e, true);
        ex.extend(self.focus_main_ex(home, &e.app, clients));
        log::info!("окно {} ({}) → приложение {} (экземпляр {num}) по записи снимка {}#{} ({how}), workspace {}", c.address, c.class, e.app, e.app, e.instance, entry_members(&self.cfg, &e).join(", "));
        self.hypr.dispatch_all(&ex)?;
        self.restore_progress(&e.app);
        Ok(())
    }

    /// Живое окно, переиспользованное загрузкой сессии для записи снимка
    /// (решение D9): номер сохраняется, состав и место — из записи.
    pub fn reuse_window(&mut self, c: &Client, e: &RestoreEntry) {
        let clients = self.hypr.clients().unwrap_or_default();
        let (ex, _, num) = self.instance_ex(c, &clients, e, false);
        log::info!("загрузка сессии: окно {} ({}) — экземпляр {num} приложения {} по записи {}#{}, workspace {}", c.address, c.class, e.app, e.app, e.instance, entry_members(&self.cfg, e).join(", "));
        if let Err(err) = self.hypr.dispatch_all(&ex) {
            log::warn!("загрузка сессии: окно {}: {err:#}", c.address);
        }
    }

    /// Запись без командной строки, которую может исполнить окно `c`,
    /// открытое процессом приложения (решения D5, D10): приложение окна
    /// демон запустил при восстановлении, окно не диалог и не из списка
    /// `ignore_classes`.
    fn awaited_entry(&self, c: &Client) -> Option<usize> {
        if classless(c) || self.cfg.ignored_class(&c.class) {
            return None;
        }
        let app = app_for_window(&self.cfg, c)?;
        if !self.st.restore_apps.contains(&app) || self.cfg.apps.get(&app).is_some_and(|a| a.is_dialog(&c.title)) {
            return None;
        }
        match_restore(&self.st.restore, Some(&app), &[], None)
    }

    /// Все записи приложения исполнены — ожидание его окон закончено.
    fn restore_progress(&mut self, app: &str) {
        let left: Vec<String> = self.st.restore.iter().filter(|e| e.app == app && e.open()).map(|e| format!("{}#{}", e.app, e.instance)).collect();
        if left.is_empty() {
            if self.st.restore_apps.remove(app) {
                log::info!("восстановление {app}: все записи снимка исполнены");
            }
        } else if self.st.restore_apps.contains(app) {
            log::info!("восстановление {app}: ждём окон, которые откроет процесс приложения, — {}; ожидание снимают клавиша приложения и сохранение сессии", left.join(", "));
        }
    }

    /// Запустить запись плана с командной строкой (решения D2, D4).
    fn spawn_instance(&mut self, i: usize) {
        let e = self.st.restore[i].clone();
        let Some(cmd) = launchable(&e.cmd, e.cwd.as_deref(), None) else {
            log::warn!("восстановление экземпляра {}#{}: исполняемый файл команды {:?} не найден, запись снята", e.app, e.instance, e.cmd);
            self.st.restore[i].state = RestoreState::Done;
            return;
        };
        match self.spawn_foreign(&cmd, e.cwd.as_deref()) {
            Ok(pid) => {
                log::info!("восстановление экземпляра {}#{}: запущен pid {pid} ({} в {})", e.app, e.instance, cmd.join(" "), e.cwd.as_deref().unwrap_or("~"));
                self.st.restore[i].state = RestoreState::Launched(pid);
            }
            Err(err) => {
                log::warn!("восстановление экземпляра {}#{}: {err:#}; запись снята", e.app, e.instance);
                self.st.restore[i].state = RestoreState::Done;
            }
        }
    }

    /// Записи без workspace (свободные окна приложений) запускаются после
    /// поднятия workspace активного стола (решение D4): своей командой,
    /// а без неё первый экземпляр — командой приложения, если у приложения
    /// нет окон. Окно, скрытое при снимке, не запускается, как свободные
    /// окна снимка на `special:hidden` (решение D16).
    pub fn spawn_free_entries(&mut self) {
        let cfg = self.cfg.clone();
        let free: Vec<usize> = self.st.restore.iter().enumerate().filter(|(_, e)| e.state == RestoreState::Waiting && entry_members(&cfg, e).is_empty() && e.desktop != "hidden").map(|(i, _)| i).collect();
        for i in free {
            let e = self.st.restore[i].clone();
            if !e.cmd.is_empty() {
                self.spawn_instance(i);
                continue;
            }
            let first = self.st.restore.iter().filter(|x| x.app == e.app).map(|x| x.instance).min() == Some(e.instance);
            let live = self.hypr.clients().map(|cl| cl.iter().any(|c| c.app().as_deref() == Some(e.app.as_str()))).unwrap_or(false);
            if !first || live {
                continue;
            }
            self.st.restore_apps.insert(e.app.clone());
            let (desktop, target) = match e.desktop.parse::<u8>() {
                Ok(n) => (n, Target::Free(Some(e.rect))),
                Err(_) => (self.current, Target::Pool),
            };
            log::info!("восстановление {}#{}: свободное окно приложения запускается командой приложения", e.app, e.instance);
            if let Err(err) = self.spawn(&e.app, None, desktop, target, false) {
                log::warn!("восстановление {}#{}: {err:#}", e.app, e.instance);
            }
        }
    }

    /// Снять ожидания плана восстановления (решение D10, `end_wait`):
    /// приложения `app`, все ожидания окон (`app` нет) или весь план (`all`).
    pub fn end_restore_wait(&mut self, app: Option<&str>, all: bool, why: &str) {
        let dropped = end_wait(&mut self.st.restore, &self.st.restore_apps, app, all);
        match app {
            Some(a) if !all => {
                self.st.restore_apps.remove(a);
            }
            _ => self.st.restore_apps.clear(),
        }
        if all {
            self.st.restore.clear();
        }
        if !dropped.is_empty() {
            log::info!("{why}: ожидание окон снимка снято — {}", dropped.join(", "));
        }
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
                    let pull = req.get("pull").and_then(|v| v.as_bool()).unwrap_or(false);
                    self.app(&apps, desktop, ws.as_deref(), pull).map(|_| json!({"ok": true}))
                }
            }
            "key" => match s("chain") {
                Some(chain) => self.key(&chain).map(|_| json!({"ok": true})),
                None => Err(anyhow::anyhow!("нет цепочки")),
            },
            "next" => self.next().map(|_| json!({"ok": true})),
            "move-desktop" => match desktop {
                Some(n) => self.move_desktop(n).map(|_| json!({"ok": true})),
                None => Err(anyhow::anyhow!("нет desktop")),
            },
            "arrange" => self.arrange().map(|_| json!({"ok": true})),
            "detach" => self.detach().map(|_| json!({"ok": true})),
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
            "save-session" => crate::save::save_session(self).map(|(ws, windows, adopted)| json!({"ok": true, "workspaces": ws, "windows": windows, "adopted": adopted})),
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
                    // Состав — по тегам: общее окно стоит в составе каждого
                    // своего workspace (изменение shared-windows, решение D11).
                    let windows: Vec<String> = ws_windows(&clients, ws).iter().map(|c| c.address.clone()).collect();
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
                json!({ "address": c.address, "class": c.class, "title": c.title, "app": c.app(), "instance": c.app_instance().map(|(_, n)| n), "workspace": c.workspace.name, "workspaces": c.workspaces(), "foreign": self.st.foreign.contains_key(&c.address), "rect": c.rect(), "pid": c.pid })
            })
            .collect();
        let pending: Vec<Value> = self.pending.iter().map(|p| json!({ "app": p.app, "pid": p.pid, "desktop": p.desktop })).collect();
        let obj = v.as_object_mut().unwrap();
        obj.insert("ok".into(), json!(true));
        obj.insert("windows".into(), json!(windows));
        obj.insert("pending".into(), json!(pending));
        let restore: Vec<Value> = self
            .st
            .restore
            .iter()
            .map(|e| {
                let state = match e.state {
                    RestoreState::Waiting => "waiting".to_string(),
                    RestoreState::Launched(pid) => format!("launched {pid}"),
                    RestoreState::Done => "done".to_string(),
                };
                json!({ "app": e.app, "instance": e.instance, "workspaces": e.workspaces, "cmd": e.cmd, "state": state })
            })
            .collect();
        obj.insert("restore".into(), json!(restore));
        obj.insert("restore_apps".into(), json!(self.st.restore_apps));
        obj.insert("cells".into(), serde_json::to_value(&self.st.cells).unwrap_or_default());
        // Раскладка в памяти для проверок (изменение live-layout, решение
        // D15): главное приложение и изменённые места рядом с назначением.
        obj.insert("main".into(), serde_json::to_value(&self.st.main).unwrap_or_default());
        let moved: BTreeMap<&String, BTreeMap<&String, Value>> = self.st.moved.iter().map(|(w, m)| (w, m.iter().map(|(a, x)| (a, json!({ "rect": x.rect, "owner": x.owner }))).collect())).collect();
        obj.insert("moved".into(), serde_json::to_value(moved).unwrap_or_default());
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
///
/// Командная строка приводится к виду, по которому процесс можно запустить
/// снова (`launchable`; изменение classless-windows-stay-free, решение D4);
/// если исполняемый файл найти не удалось, команда возвращается пустой,
/// а в журнал идёт предупреждение: такое окно не становится дополнительным
/// приложением и в снимок сессии не попадает.
pub fn proc_info(pid: i32) -> (Vec<String>, Option<String>) {
    let cmd: Vec<String> = std::fs::read(format!("/proc/{pid}/cmdline")).map(|b| b.split(|&x| x == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()).unwrap_or_default();
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned());
    if let Some(id) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok().and_then(|c| flatpak_app_id(&c)) {
        let mut out = vec!["flatpak".to_string(), "run".to_string(), id];
        out.extend(cmd.into_iter().skip(1));
        return (out, None);
    }
    if cmd.is_empty() {
        return (cmd, cwd);
    }
    match launchable(&cmd, cwd.as_deref(), proc_exe(pid).as_deref()) {
        Some(c) => (c, cwd),
        None => {
            log::warn!("процесс {pid}: исполняемый файл команды {:?} не найден ни по пути, ни в PATH, ни по /proc/{pid}/exe; запустить окно заново нечем", cmd);
            (Vec::new(), cwd)
        }
    }
}

/// Командная строка процесса в виде, пригодном для повторного запуска
/// (изменение classless-windows-stay-free, решение D4) или `None`, если
/// исполняемый файл не найден.
///
/// Chromium и основанные на нём браузеры переписывают свою командную строку
/// (setproctitle): в `/proc/<pid>/cmdline` лежит одна строка с пробелами
/// вместо аргументов, разделённых нулевыми байтами. Такая строка, если файла
/// с таким именем нет, делится по пробелам. Затем исполняемый файл
/// проверяется: абсолютный путь — как есть, относительный (`./steamwebhelper`)
/// — от рабочего каталога процесса, имя без каталога — по `PATH`. Не нашёлся
/// файл — вместо него берётся `exe`, путь из `/proc/<pid>/exe`.
pub fn launchable(cmd: &[String], cwd: Option<&str>, exe: Option<&Path>) -> Option<Vec<String>> {
    let mut cmd: Vec<String> = cmd.to_vec();
    if cmd.len() == 1 && cmd[0].contains(char::is_whitespace) && !Path::new(&cmd[0]).exists() {
        cmd = cmd[0].split_whitespace().map(String::from).collect();
    }
    let prog = cmd.first()?.clone();
    let path = Path::new(&prog);
    let found: Option<String> = if path.is_absolute() {
        path.is_file().then(|| prog.clone())
    } else if prog.contains('/') {
        cwd.map(|d| Path::new(d).join(path)).filter(|p| p.is_file()).map(|p| p.components().collect::<PathBuf>().to_string_lossy().into_owned())
    } else {
        which(&prog).map(|_| prog.clone())
    };
    let found = found.or_else(|| exe.filter(|e| e.is_file()).map(|e| e.to_string_lossy().into_owned()))?;
    cmd[0] = found;
    Some(cmd)
}

/// Рабочий каталог для запуска: заданный, если он существует, иначе
/// домашний каталог (изменение classless-windows-stay-free, решение D2).
/// Вторым значением — предупреждение для журнала, когда заданного каталога нет.
pub fn launch_dir(cwd: Option<&str>, home: &Path) -> (PathBuf, Option<String>) {
    match cwd {
        Some(d) if Path::new(d).is_dir() => (PathBuf::from(d), None),
        Some(d) => (home.to_path_buf(), Some(format!("каталога {d} нет, запуск в {}", home.display()))),
        None => (home.to_path_buf(), None),
    }
}

/// Запустить процесс в собственной сессии (он переживает остановку демона).
/// Ошибка называет команду, аргументы, каталог и текст ошибки ОС.
pub fn start(cmd: &str, args: &[String], cwd: Option<&str>, env: &BTreeMap<String, String>) -> Result<Child> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let (dir, warn) = launch_dir(cwd, &home);
    if let Some(w) = warn {
        log::warn!("{cmd}: {w}");
    }
    let mut command = Command::new(cmd);
    command.args(args).envs(env).current_dir(&dir).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| nix::unistd::setsid().map(|_| ()).map_err(std::io::Error::other));
    }
    command.spawn().with_context(|| format!("не удалось запустить {cmd:?} с аргументами {args:?} в каталоге {}", dir.display()))
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

/// Окна приложения `app` без окон вариантов, которые workspace с набором
/// приложений `apps` описывает отдельно. Окно варианта — окно своего
/// семейства, но если вариант сам описан в workspace, место ему задаёт его
/// собственная запись, а не запись семейства: более точное совпадение
/// побеждает (спецификация ws-daemon, «Расстановка окон»).
fn app_windows_in<'a>(cfg: &Config, clients: &'a [Client], apps: &[String], app: &str) -> Vec<&'a Client> {
    app_windows(cfg, clients, app).into_iter().filter(|c| c.app().is_none_or(|own| own == app || !apps.contains(&own))).collect()
}

/// Окна, которые расставляет запись приложения `app` в workspace `ws`
/// с набором приложений `apps`: окна приложения, входящие в `ws` по тегу
/// состава (изменение shared-windows, решение D1).
fn placed_windows<'a>(cfg: &Config, clients: &'a [Client], ws: &str, apps: &[String], app: &str) -> Vec<&'a Client> {
    app_windows_in(cfg, clients, apps, app).into_iter().filter(|c| c.in_ws(ws)).collect()
}

/// Диспетчеры, которыми новое окно становится экземпляром `app` с номером
/// `num` и, если задан `ws`, входит в этот workspace (изменение
/// shared-windows, решение D2).
pub fn entry_tags(addr: &str, app: &str, num: u32, ws: Option<&str>) -> Vec<String> {
    let mut ex = vec![hypr::d_tag(addr, &format!("app:{app}#{num}"))];
    if let Some(w) = ws {
        ex.push(hypr::d_tag(addr, &hypr::ws_tag(w)));
    }
    ex
}

/// Окна, входящие в workspace `ws` по тегам состава (изменение
/// shared-windows, решение D1).
pub fn ws_windows<'a>(clients: &'a [Client], ws: &str) -> Vec<&'a Client> {
    clients.iter().filter(|c| c.in_ws(ws)).collect()
}

/// Активные workspace столов: стол → workspace.
pub fn actives(st: &State) -> BTreeMap<u8, String> {
    st.desktops.iter().filter_map(|(n, d)| Some((*n, d.active.clone()?))).collect()
}

/// Где стоит окно: на обычном столе, на `special:pool` (и на прочих
/// специальных столах, кроме скрытого) или на `special:hidden`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spot {
    Desktop(u8),
    Pool,
    Hidden,
}

impl Spot {
    pub fn of(c: &Client) -> Spot {
        if c.on_hidden() {
            Spot::Hidden
        } else if let Some(n) = c.desktop() {
            Spot::Desktop(n)
        } else {
            Spot::Pool
        }
    }
}

/// Где должно стоять окно, входящее в workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Home {
    Desktop(u8),
    Pool,
    /// Окно скрыто пользователем, правило его не трогает.
    Keep,
}

/// Правило размещения окна, входящего в workspace (изменение shared-windows,
/// решение D4; спецификация ws-daemon, «Следование общего окна за столом»).
/// `members` — workspace окна по тегам состава, `active` — активные workspace
/// столов, `current` — стол, где находится композитор, `at` — где окно стоит.
/// По порядку: стол композитора, если активный там workspace содержит окно;
/// нынешний стол окна при том же условии; стол с наименьшим номером, где
/// активен workspace окна; иначе `special:pool`. Скрытое окно остаётся, где есть.
pub fn window_home(members: &[String], active: &BTreeMap<u8, String>, current: u8, at: Spot) -> Home {
    if at == Spot::Hidden {
        return Home::Keep;
    }
    let holds = |n: u8| active.get(&n).is_some_and(|w| members.contains(w));
    if holds(current) {
        return Home::Desktop(current);
    }
    if let Spot::Desktop(d) = at
        && holds(d)
    {
        return Home::Desktop(d);
    }
    match active.iter().filter(|(_, w)| members.contains(w)).map(|(n, _)| *n).min() {
        Some(d) => Home::Desktop(d),
        None => Home::Pool,
    }
}

/// Место окна, пришедшего на стол workspace `ws` (изменение live-layout,
/// решение D8): прямоугольник, запомненный для окна в `ws`, а без него — место
/// его приложения в раскладке `ws`; одинаково в обоих режимах. `None` — окно
/// остаётся в своём прямоугольнике.
pub fn arrive_rect(st: &mut State, cfg: &Config, ws: &str, c: &Client, mon: (i32, i32)) -> Option<PxRect> {
    let ws_apps: Vec<String> = cfg.workspaces.get(ws).map(|w| w.apps.keys().cloned().collect()).unwrap_or_default();
    let place = ws_app_of(cfg, ws, &ws_apps, c).and_then(|a| st.rect_for(cfg, ws, &a, mon));
    let kept = st.geom.get(ws).and_then(|g| g.get(&c.address)).copied();
    raise_target(false, true, kept, place, c.rect())
}

/// Приходит ли окно workspace `ws` при его поднятии на столе `n`: окно
/// стоит на другом столе или на `special:pool` либо стоит на `n`, но
/// в составе вытесняемого workspace (`before` — активные workspace столов
/// до поднятия). Такое окно встаёт в состояние, запомненное для `ws`
/// (решения D6, D16); окно, уже стоящее на столе как окно `ws`, поднятие
/// не двигает.
pub fn arrives(c: &Client, n: u8, before: &BTreeMap<u8, String>, ws: &str) -> bool {
    c.desktop() != Some(n) || before.get(&n).is_some_and(|old| old != ws && c.in_ws(old))
}

/// Раскладка workspace, снятая с экрана (изменение live-layout, решение D3).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Absorb {
    /// Запись приложения и её изменённое место; `None` — окно стоит ровно
    /// на исходном месте, и изменённого места у записи не остаётся.
    pub moved: Vec<(String, Option<Moved>)>,
    /// Окно и прямоугольник, который запоминается для него в `State::geom`.
    pub geom: Vec<(String, PxRect)>,
}

/// Снять раскладку workspace `ws` со стола `n` (решения D2, D3): для каждой
/// записи приложения берётся первое окно записи на этом столе в порядке
/// экземпляров (`placed_windows`, как у цикла), не скрытое и не в полноэкранном
/// режиме композитора (у такого окна композитор отдаёт размер экрана, а не
/// окна). Его прямоугольник становится изменённым местом записи, а равный
/// исходному месту изменённого места не даёт. Записи без таких окон не
/// меняются. Для каждого такого окна `ws` на столе запоминается его
/// прямоугольник. Окна на других столах и на `special:pool` стоят там
/// в состоянии другого workspace и не учитываются. Состояние читается
/// только ради исходных мест: раскладка собирается при первом обращении.
pub fn absorb_plan(cfg: &Config, st: &mut State, ws: &str, n: u8, clients: &[Client], mon: (i32, i32)) -> Absorb {
    let mut out = Absorb::default();
    let apps: Vec<String> = cfg.workspaces.get(ws).map(|w| w.apps.keys().cloned().collect()).unwrap_or_default();
    let shown = |c: &&Client| c.desktop() == Some(n) && !c.on_hidden() && c.fullscreen == 0;
    for app in &apps {
        let Some(first) = placed_windows(cfg, clients, ws, &apps, app).into_iter().find(shown) else { continue };
        let rect = first.rect();
        let base = st.base_rect(cfg, ws, app, mon);
        out.moved.push((app.clone(), (base != Some(rect)).then(|| Moved { rect, owner: Some(first.address.clone()) })));
    }
    for c in ws_windows(clients, ws).into_iter().filter(shown) {
        out.geom.push((c.address.clone(), c.rect()));
    }
    out
}

/// Снять раскладку `ws` со стола `n` и записать её в состояние (решение D4).
/// Снятие идёт только на столе, где `ws` активен: на другом столе окно
/// стоит в состоянии другого workspace. Изменения мест пишутся в журнал:
/// по ним видно, откуда взялось место, которое потом займёт окно.
pub fn absorb_into(cfg: &Config, st: &mut State, ws: &str, n: u8, clients: &[Client], mon: (i32, i32)) {
    if st.desktops.get(&n).and_then(|d| d.active.as_deref()) != Some(ws) {
        return;
    }
    let a = absorb_plan(cfg, st, ws, n, clients, mon);
    let slot = st.moved.entry(ws.to_string()).or_default();
    for (app, m) in a.moved {
        match m {
            Some(m) => {
                if slot.get(&app).map(|x| x.rect) != Some(m.rect) {
                    log::info!("{ws}: место {app} — {},{} {}×{} (окно {})", m.rect.x, m.rect.y, m.rect.w, m.rect.h, m.owner.as_deref().unwrap_or(""));
                }
                slot.insert(app, m);
            }
            None => {
                if slot.remove(&app).is_some() {
                    log::info!("{ws}: окно {app} стоит на исходном месте, изменённого места нет");
                }
            }
        }
    }
    st.moved.retain(|_, m| !m.is_empty());
    let g = st.geom.entry(ws.to_string()).or_default();
    for (addr, r) in a.geom {
        g.insert(addr, r);
    }
}

/// Перенос окна за пользователем на стол при смене стола.
#[derive(Debug, Clone, PartialEq)]
pub struct Follow {
    pub addr: String,
    /// Место на новом столе; `None` — прямоугольник окна не меняется.
    pub rect: Option<PxRect>,
    /// Прямоугольник окна у композитора до переноса.
    pub now: PxRect,
}

/// План следования за столом (решения D5, D6; спецификация ws-daemon,
/// «Следование общего окна за столом»): окна активного workspace стола `n`,
/// стоящие на других столах или на `special:pool`, переезжают на `n`
/// в состояние, запомненное для этого workspace. Прежде чем окно уйдёт
/// со стола другого своего workspace, раскладка того workspace снимается
/// с экрана (изменение live-layout, решение D4): место, которое пользователь
/// задал окну там, остаётся за ним. Скрытые окна не трогаются; стол без
/// активного workspace плана не даёт.
pub fn follow_plan(st: &mut State, cfg: &Config, clients: &[Client], n: u8, mon: (i32, i32)) -> Vec<Follow> {
    let active = actives(st);
    let Some(ws) = active.get(&n).cloned() else { return Vec::new() };
    let arriving: Vec<&Client> = ws_windows(clients, &ws).into_iter().filter(|c| !c.on_hidden() && c.desktop() != Some(n)).collect();
    let mut leaving: BTreeSet<(u8, String)> = BTreeSet::new();
    for c in &arriving {
        if let Some(k) = c.desktop()
            && let Some(v) = active.get(&k).filter(|v| **v != ws && c.in_ws(v))
        {
            leaving.insert((k, v.clone()));
        }
    }
    for (k, v) in &leaving {
        absorb_into(cfg, st, v, *k, clients, mon);
    }
    arriving.into_iter().map(|c| Follow { addr: c.address.clone(), rect: arrive_rect(st, cfg, &ws, c, mon), now: c.rect() }).collect()
}

/// Окна других workspace, которые поднятие `ws` забирает в общее
/// пользование (решение D3): для приложения, у которого после захвата
/// свободных окон нет ни одного окна в `ws`, — все его нескрытые окна
/// (по семейству — окна семейства, по варианту — окна варианта). Окна,
/// уже входящие в `ws`, другими не дополняются. Возвращает номер окна
/// в списке клиентов и приложение workspace.
pub fn share_plan(cfg: &Config, clients: &[Client], ws: &str, apps: &[String]) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    for app in apps {
        if !placed_windows(cfg, clients, ws, apps, app).is_empty() {
            continue;
        }
        for c in app_windows_in(cfg, clients, apps, app) {
            if c.on_hidden() || !c.has_ws() {
                continue;
            }
            if let Some(i) = clients.iter().position(|x| x.address == c.address)
                && !out.iter().any(|(j, _)| *j == i)
            {
                out.push((i, app.clone()));
            }
        }
    }
    out
}

/// Запись о месте приложения `app` для workspace, в который окно входит
/// без описания там (решение D8; спецификация ws-sessions, «Дополнительные
/// приложения сессии»): для приложения файла конфига — только место, для
/// приложения, известного лишь записи сессии другого workspace, — полная
/// запись с классом, командой и каталогом. `None` — приложения нет ни там,
/// ни там.
pub fn share_record(file: &Config, extra: &BTreeMap<String, BTreeMap<String, ExtraApp>>, app: &str, rect: PxRect) -> Option<ExtraApp> {
    if file.apps.contains_key(app) {
        return Some(ExtraApp { rect, ..ExtraApp::default() });
    }
    extra.values().find_map(|m| m.get(app).filter(|e| !e.cmd.is_empty())).map(|e| ExtraApp { rect, ..e.clone() })
}

/// Куда ведёт клавиша приложения (решение D7; спецификация ws-daemon,
/// «Цепочка приложения»).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppRoute {
    /// Приложение описано в активном workspace текущего стола: цикл в нём.
    Cycle { ws: String, app: String },
    /// Перетащить окна приложения в активный workspace текущего стола.
    Pull { ws: String, app: String },
    /// Поднять запущенный workspace с приложением: `desktop` — стол, где он
    /// в списке, если это не текущий стол.
    Raise { ws: String, app: String, desktop: Option<u8> },
    /// Открыть приложение в активном workspace текущего стола.
    Open { ws: String, app: String },
    /// Активного workspace на столе нет: цикл по окнам приложения вне workspace.
    Free { app: String },
}

/// Запущенные workspace в порядке поиска: список текущего стола по порядку,
/// затем списки столов по возрастанию номера. Второе значение — стол, на
/// который нужно перейти, если workspace не в списке текущего стола.
/// Запущенным считается workspace из списка какого-либо стола, в том числе
/// ожидающий ленивого поднятия; workspace вне списков клавиша приложения
/// не поднимает.
fn running(desktops: &BTreeMap<u8, Desktop>, current: u8) -> Vec<(String, Option<u8>)> {
    let mut out: Vec<(String, Option<u8>)> = Vec::new();
    let here = desktops.get(&current).map(|d| d.workspaces.clone()).unwrap_or_default();
    for ws in here {
        if !out.iter().any(|(w, _)| *w == ws) {
            out.push((ws, None));
        }
    }
    for (k, d) in desktops.iter().filter(|(k, _)| **k != current) {
        for ws in &d.workspaces {
            if !out.iter().any(|(w, _)| w == ws) {
                out.push((ws.clone(), Some(*k)));
            }
        }
    }
    out
}

/// Путь клавиши приложения по нажатой цепочке `chain` (изменение
/// workspace-overrides, решения D4 и D16; спецификация ws-daemon, «Цепочка
/// приложения»). `Own` — приложения с собственной клавишей `chain` по именам,
/// кандидаты — `Own` и приложения, которым `chain` назначена ключом в каком-либо
/// workspace конфига (`Config::key_candidates`), `W` — активный workspace
/// текущего стола, `tagged` — приложения окон, несущих тег состава `W`:
///
/// 1. клавиша ведёт к окну там, где оно сейчас есть (решение D16): `W`
///    откликается на цепочку своей записью — цикл отозвавшегося приложения
///    в `W`; иначе кандидат, который входит в `W` (описан в нём, в том числе
///    через семейство, входит дополнительным приложением сессии или его окно
///    несёт тег состава `W`), — цикл его в `W`; из нескольких таких — тот,
///    чья запись стоит в разделе `W` раньше, а приложения, входящие в `W`
///    только тегом окна, — после описанных;
/// 2. запущенный `V` откликается на цепочку либо описывает приложение из
///    `Own`, не переопределяя его клавишу (решение D13), — поднять `V`;
/// 3. приложение из `Own` описано только в запущенных workspace, которые
///    переопределили его клавишу, — перетащить его окна в `W` (случай «в»),
///    а без `W` поднять первый такой workspace;
/// 4. иначе первое приложение из `Own`, а без собственной клавиши — первое
///    по имени, которому цепочка назначена ключом в каком-либо workspace:
///    открыть в `W` (случай «а») или цикл вне workspace.
///
/// `None` — на цепочку не отзывается ни одно приложение.
pub fn key_route(cfg: &Config, desktops: &BTreeMap<u8, Desktop>, current: u8, chain: &str, tagged: &[String]) -> Option<AppRoute> {
    let own: Vec<String> = cfg.apps.iter().filter(|(_, a)| a.chain.as_deref().is_some_and(|c| same_chain(c, chain))).map(|(n, _)| n.clone()).collect();
    let active = desktops.get(&current).and_then(|d| d.active.clone());
    if let Some(w) = &active {
        if let Some(app) = cfg.responds(w, chain) {
            return Some(AppRoute::Cycle { ws: w.clone(), app });
        }
        if let Some(app) = present_candidate(cfg, w, &cfg.key_candidates(chain), tagged) {
            return Some(AppRoute::Cycle { ws: w.clone(), app });
        }
    }
    let run = running(desktops, current);
    for (v, desktop) in &run {
        let app = cfg.responds(v, chain).or_else(|| own.iter().find(|x| ws_has_app(cfg, v, x) && !cfg.overrides_key(v, x)).cloned());
        if let Some(app) = app {
            return Some(AppRoute::Raise { ws: v.clone(), app, desktop: *desktop });
        }
    }
    for x in &own {
        if let Some((v, desktop)) = run.iter().find(|(v, _)| ws_has_app(cfg, v, x)) {
            return Some(match &active {
                Some(w) => AppRoute::Pull { ws: w.clone(), app: x.clone() },
                None => AppRoute::Raise { ws: v.clone(), app: x.clone(), desktop: *desktop },
            });
        }
    }
    let app = own.first().cloned().or_else(|| {
        cfg.apps
            .keys()
            .find(|a| cfg.workspaces.iter().any(|(w, x)| x.apps.get(*a).and_then(|e| e.chain.as_deref()).is_some_and(|c| same_chain(c, chain)) && cfg.overrides_key(w, a)))
            .cloned()
    })?;
    Some(match active {
        Some(ws) => AppRoute::Open { ws, app },
        None => AppRoute::Free { app },
    })
}

/// Кандидат клавиши, входящий в workspace `ws` (решение D16): описанный
/// в нём, в том числе через семейство или дополнительным приложением сессии,
/// либо приложение, окно которого несёт тег состава `ws` (`tagged` —
/// приложения таких окон; окно варианта считается и окном семейства).
/// Из нескольких — тот, чья запись стоит в разделе `ws` раньше; входящие
/// только тегом окна идут после описанных, по порядку кандидатов.
fn present_candidate(cfg: &Config, ws: &str, candidates: &[String], tagged: &[String]) -> Option<String> {
    let entries: Vec<String> = cfg.workspaces.get(ws).map(|w| w.apps.keys().cloned().collect()).unwrap_or_default();
    let rank = |x: &String| -> Option<usize> {
        entries.iter().position(|e| cfg.app_is(x, e)).or_else(|| tagged.iter().any(|t| cfg.app_is(t, x)).then_some(entries.len()))
    };
    candidates.iter().filter_map(|x| rank(x).map(|r| (r, x))).min_by_key(|(r, _)| *r).map(|(_, x)| x.clone())
}

/// Путь команды `workspaced app <имя>` — собственной клавиши приложения
/// (решение D5): те же шаги, что у `key_route`, где `Own` — это приложение,
/// а workspace откликается, если описывает его и не переопределяет его
/// клавишу. Приложение, окно которого входит в активный workspace тегом
/// состава (`tagged`), ведёт цикл там же (решение D16). `pull` перетаскивает
/// окна в активный workspace, если приложение в нём не описано, раньше
/// цикла по окну с тегом и раньше поднятия. `apps` — кандидаты (прежняя привязка `workspaced app a b c`):
/// действует тот, чей путь найдётся на более раннем шаге, при равенстве —
/// первый по порядку.
pub fn app_route(cfg: &Config, desktops: &BTreeMap<u8, Desktop>, current: u8, apps: &[String], pull: bool, tagged: &[String]) -> AppRoute {
    let active = desktops.get(&current).and_then(|d| d.active.clone());
    let run = running(desktops, current);
    let one = |x: &String| -> (u8, AppRoute) {
        if let Some(w) = &active
            && ws_has_app(cfg, w, x)
        {
            let step = if cfg.overrides_key(w, x) { 2 } else { 1 };
            return (step, AppRoute::Cycle { ws: w.clone(), app: x.clone() });
        }
        if pull && let Some(w) = &active {
            return (2, AppRoute::Pull { ws: w.clone(), app: x.clone() });
        }
        if let Some(w) = &active
            && tagged.iter().any(|t| cfg.app_is(t, x))
        {
            return (2, AppRoute::Cycle { ws: w.clone(), app: x.clone() });
        }
        if let Some((v, desktop)) = run.iter().find(|(v, _)| ws_has_app(cfg, v, x) && !cfg.overrides_key(v, x)) {
            return (3, AppRoute::Raise { ws: v.clone(), app: x.clone(), desktop: *desktop });
        }
        if let Some((v, desktop)) = run.iter().find(|(v, _)| ws_has_app(cfg, v, x)) {
            return (4, match &active {
                Some(w) => AppRoute::Pull { ws: w.clone(), app: x.clone() },
                None => AppRoute::Raise { ws: v.clone(), app: x.clone(), desktop: *desktop },
            });
        }
        (5, match &active {
            Some(w) => AppRoute::Open { ws: w.clone(), app: x.clone() },
            None => AppRoute::Free { app: x.clone() },
        })
    };
    let mut best: Option<(u8, AppRoute)> = None;
    for x in apps {
        let r = one(x);
        if best.as_ref().is_none_or(|(b, _)| r.0 < *b) {
            best = Some(r);
        }
    }
    best.map(|(_, r)| r).unwrap_or(AppRoute::Free { app: String::new() })
}

/// Теги состава, которые пора снять (решение D2): workspace исчез
/// из эффективного конфига или не описывает приложение окна (само
/// приложение либо его семейство). Окно без приложения не входит ни в один
/// workspace. Возвращает адрес окна и workspace.
pub fn stale_ws_tags(cfg: &Config, clients: &[Client]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for c in clients {
        let app = c.app();
        for w in c.workspaces() {
            if !app.as_deref().is_some_and(|a| ws_has_app(cfg, &w, a)) {
                out.push((c.address.clone(), w));
            }
        }
    }
    out
}

/// Состав для окон прежней версии демона (решение D2): окну с тегом
/// экземпляра без тегов состава. Окно на столе, где активен workspace,
/// описывающий его приложение, входит в этот workspace; окно на
/// `special:pool` — во все неактивные workspace из списков столов,
/// описывающие его приложение; иначе окно остаётся свободным. Возвращает
/// адрес окна и workspace.
pub fn derive_membership(cfg: &Config, st: &State, clients: &[Client]) -> Vec<(String, String)> {
    let active = actives(st);
    let mut listed: Vec<String> = st.desktops.values().flat_map(|d| d.workspaces.iter().cloned()).collect();
    listed.sort();
    listed.dedup();
    let mut out = Vec::new();
    for c in clients.iter().filter(|c| !c.has_ws()) {
        let Some(app) = c.app() else { continue };
        if let Some(n) = c.desktop() {
            if let Some(w) = active.get(&n).filter(|w| ws_has_app(cfg, w, &app)) {
                out.push((c.address.clone(), w.clone()));
            }
        } else if c.on_pool() {
            for w in listed.iter().filter(|w| !active.values().any(|a| a == *w) && ws_has_app(cfg, w, &app)) {
                out.push((c.address.clone(), w.clone()));
            }
        }
    }
    out
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
                        w.apps.insert(name.clone(), WsApp::at(Placement::Rect { rect: e.rect.to_rect() }));
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

/// Раскладки, переживающие перечитывание конфига (спецификация ws-config,
/// «Слежение за конфигом»; изменение live-layout, решение D12): у workspace,
/// чей раздел в файле изменился или исчез, снимаются назначение мест, главное
/// приложение, изменённые места и запомненные прямоугольники окон — раскладка
/// соберётся из нового файла. Раскладки остальных workspace остаются, поэтому
/// правка одного workspace не сбрасывает обмен мест и сдвинутые окна в другом.
/// Возвращает изменённые workspace, которые есть в новом файле: их окна
/// на столе, где workspace активен, встают на новые места сразу.
pub fn keep_layout(old: &Config, new: &Config, st: &mut State) -> Vec<String> {
    let mut names: BTreeSet<String> = old.workspaces.keys().chain(new.workspaces.keys()).cloned().collect();
    names.extend(st.cells.keys().chain(st.moved.keys()).chain(st.main.keys()).chain(st.geom.keys()).cloned());
    let mut out = Vec::new();
    for ws in names {
        let (o, n) = (old.workspaces.get(&ws), new.workspaces.get(&ws));
        if o == n && n.is_some() {
            continue;
        }
        st.reset_layout(&ws);
        if n.is_some() {
            out.push(ws);
        }
    }
    out
}

/// Вернуть к описанию изменённые места, снятые с окна `addr` (изменение
/// live-layout, решение D9): окно закрыто, отделено командой отделения или
/// снято сверкой состава. Если в workspace остались окна записи, владельцем
/// места становится первое из них (нескрытое, если такое есть), иначе
/// изменённое место снимается, и следующее окно приложения встанет
/// на исходное. Места без владельца (раскладка из снимка) не трогаются.
/// `clients` — окна после ухода `addr`: закрытого окна в них нет, у окна,
/// ушедшего из workspace, нет его тега состава, а в остальных своих
/// workspace оно остаётся владельцем. Возвращает снятые места как
/// «workspace, запись».
pub fn drop_moved(st: &mut State, cfg: &Config, addr: &str, clients: &[Client]) -> Vec<(String, String)> {
    let mut dropped = Vec::new();
    for (ws, entries) in st.moved.iter_mut() {
        let apps: Vec<String> = cfg.workspaces.get(ws).map(|w| w.apps.keys().cloned().collect()).unwrap_or_default();
        entries.retain(|entry, m| {
            if m.owner.as_deref() != Some(addr) {
                return true;
            }
            let rest = placed_windows(cfg, clients, ws, &apps, entry);
            match rest.iter().find(|c| !c.on_hidden()).or(rest.first()) {
                Some(c) => {
                    m.owner = Some(c.address.clone());
                    true
                }
                None => {
                    dropped.push((ws.clone(), entry.clone()));
                    false
                }
            }
        });
    }
    st.moved.retain(|_, m| !m.is_empty());
    dropped
}

/// Окно `addr` встало на место приложения `app` в `ws`: изменённое место
/// без владельца (раскладка из снимка сессии) получает его владельцем
/// (решение D9), чтобы закрытие окна вернуло место к описанию.
pub fn claim_owner(st: &mut State, cfg: &Config, ws: &str, app: &str, addr: &str) {
    let Some((entry, _)) = cfg.ws_entry(ws, app) else { return };
    if let Some(m) = st.moved.get_mut(ws).and_then(|m| m.get_mut(entry))
        && m.owner.is_none()
    {
        m.owner = Some(addr.to_string());
    }
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
/// без последнего шага: место приложения в раскладке активного workspace
/// стола (изменённый прямоугольник, а без него ячейка или `rect` записи),
/// то же у его семейства, `rect` самого приложения. `None` означает,
/// что места нет ни там, ни там: окно остаётся, где его открыл композитор,
/// и демон его не центрирует.
pub fn open_rect(st: &mut State, cfg: &Config, ws: Option<&str>, app: &str, mon: (i32, i32)) -> Option<PxRect> {
    match ws.filter(|w| cfg.workspaces.contains_key(*w)) {
        Some(w) => st.rect_for(cfg, w, app, mon),
        None => State::app_rect(cfg, app, mon),
    }
}

/// Почему окно осталось свободным.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Free {
    /// Класс окна перечислен в `ignore_classes`.
    Ignored,
    /// Заголовок окна подходит `dialog_title` названного приложения.
    Dialog(String),
    /// На столе окна нет активного workspace.
    NoWorkspace,
    /// Командной строки процесса получить не удалось: запускать окно заново
    /// было бы нечем, и записывать его в состав workspace незачем.
    NoCommand,
    /// У окна нет класса: чьё оно, не определить (изменение
    /// classless-windows-stay-free, решение D1).
    NoClass,
}

impl Free {
    /// Причина для журнала.
    pub fn text(&self) -> String {
        match self {
            Free::Ignored => "класс окна в списке ignore_classes".to_string(),
            Free::Dialog(app) => format!("заголовок подходит dialog_title приложения {app}"),
            Free::NoWorkspace => "на столе нет активного workspace".to_string(),
            Free::NoCommand => "у окна нет командной строки".to_string(),
            Free::NoClass => "у окна нет класса".to_string(),
        }
    }
}

/// Что делать с окном без тега, появившимся в системе (решения D1 и D2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Join {
    /// Окно приложения эффективного конфига, которое уже входит в workspace
    /// стола (или стол без активного workspace): только тег и место.
    App(String),
    /// Окно приложения конфига, которого в активном workspace нет: кроме тега
    /// и места, приложение записывается дополнительным приложением сессии.
    AppPlace(String),
    /// Новое дополнительное приложение сессии с этим именем.
    Extra(String),
    /// Окно остаётся свободным.
    Free(Free),
}

/// Имена, которые не может занять новое дополнительное приложение сессии:
/// приложения эффективного конфига и дополнительные приложения всех workspace.
pub fn taken_names(cfg: &Config, extra: &BTreeMap<String, BTreeMap<String, ExtraApp>>) -> Vec<String> {
    let mut v: Vec<String> = cfg.apps.keys().cloned().collect();
    v.extend(extra.values().flat_map(|m| m.keys().cloned()));
    v.sort();
    v.dedup();
    v
}

/// Имя дополнительного приложения по классу окна, не совпадающее ни с одним
/// из занятых.
pub fn app_name(class: &str, taken: &[String]) -> String {
    let base: String = class.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect::<String>().trim_matches('-').to_string();
    let base = if base.is_empty() { "app".to_string() } else { base };
    if !taken.contains(&base) {
        return base;
    }
    (2..).map(|i| format!("{base}-{i}")).find(|n| !taken.contains(n)).unwrap()
}

/// Входит ли приложение в workspace: само или как вариант своего семейства.
pub fn ws_has_app(cfg: &Config, ws: &str, app: &str) -> bool {
    cfg.workspaces.get(ws).is_some_and(|w| w.apps.keys().any(|x| cfg.app_is(app, x)))
}

/// Решение о принятии окна без тега (спецификация ws-daemon, «Принятие окон
/// в workspace»). `cfg` — эффективный конфиг, `file` — конфиг файла (по нему
/// видно, известно ли приложение помимо записей сессии), `ws` — workspace,
/// активный на столе окна, `cmd` — командная строка процесса окна (пустая,
/// если её получить не удалось), `taken` — занятые имена приложений.
///
/// Порядок правил: окно без класса и класс из `ignore_classes` не
/// принимаются никогда; окно,
/// подходящее приложению конфига, — окно этого приложения, а его диалог
/// (`dialog_title`) остаётся свободным; на столе без активного workspace
/// принимать некуда.
pub fn join_window(cfg: &Config, file: &Config, ws: Option<&str>, c: &Client, cmd: &[String], taken: &[String]) -> Join {
    if classless(c) {
        return Join::Free(Free::NoClass);
    }
    let has_cmd = !cmd.is_empty();
    if cfg.ignored_class(&c.class) {
        return Join::Free(Free::Ignored);
    }
    if let Some(app) = app_for_window(cfg, c) {
        if cfg.apps.get(&app).is_some_and(|a| a.is_dialog(&c.title)) {
            return Join::Free(Free::Dialog(app));
        }
        let Some(w) = ws else { return Join::App(app) };
        if ws_has_app(cfg, w, &app) {
            return Join::App(app);
        }
        if file.apps.contains_key(&app) {
            return Join::AppPlace(app);
        }
        // Приложение известно только сессии другого workspace: запись о месте
        // без команды запуска там не выживет, поэтому заводится полная запись.
        // Имя прежней записи окно получает, только если это то же приложение —
        // тот же исполняемый файл; окно другой программы с тем же классом
        // заводит новое приложение со своим именем (изменение
        // classless-windows-stay-free, решение D3).
        if !has_cmd {
            return Join::Free(Free::NoCommand);
        }
        return if same_program(cfg, &app, cmd) { Join::Extra(app) } else { Join::Extra(app_name(&c.class, taken)) };
    }
    if ws.is_none() {
        return Join::Free(Free::NoWorkspace);
    }
    if !has_cmd {
        return Join::Free(Free::NoCommand);
    }
    Join::Extra(app_name(&c.class, taken))
}

/// Окно с командной строкой `cmd` принадлежит той же программе, что
/// приложение `app` эффективного конфига: совпадает исполняемый файл.
fn same_program(cfg: &Config, app: &str, cmd: &[String]) -> bool {
    cfg.apps.get(app).and_then(|a| a.cmd.as_deref()).is_some_and(|x| cmd.first().map(String::as_str) == Some(x))
}

/// Что делает команда отделения окна.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetachStep {
    /// Окно входит ещё в другие workspace: снять тег состава и увести окно
    /// по правилу размещения; `drop_extra` — снять запись дополнительного
    /// приложения сессии в этом workspace, потому что из него ушло последнее
    /// окно приложения.
    Leave { app: String, home: Home, drop_extra: bool },
    /// Workspace был у окна последним: закрыть окно. `drop_extra` — то же,
    /// что у `Leave`; `last` — других окон у приложения нет нигде, и запись
    /// с командой снимается во всех workspace.
    Close { app: String, drop_extra: bool, last: bool },
    /// Ничего не делать, причина для журнала.
    Skip(&'static str),
}

/// Решение команды `workspaced detach` (спецификация ws-daemon, «Отделение
/// окна»; изменение shared-windows, решение D9): активное окно текущего стола
/// убирается из активного workspace этого стола `ws`. Окно, входящее ещё
/// в другие workspace, уходит по правилу размещения (`active` — активные
/// workspace столов после снятия тега, `current` — стол композитора);
/// окно, для которого `ws` был последним, закрывается.
pub fn detach_step(cfg: &Config, clients: &[Client], ws: Option<&str>, extra: &BTreeMap<String, ExtraApp>, active: Option<&Client>, desks: &BTreeMap<u8, String>, current: u8) -> DetachStep {
    let Some(c) = active else { return DetachStep::Skip("на текущем столе нет активного окна") };
    let Some(w) = ws else { return DetachStep::Skip("на столе нет активного workspace") };
    let ws_apps: Vec<String> = cfg.workspaces.get(w).map(|x| x.apps.keys().cloned().collect()).unwrap_or_default();
    if ws_app_of(cfg, w, &ws_apps, c).is_none() {
        return DetachStep::Skip("активное окно не входит в активный workspace стола");
    }
    let Some(app) = c.app() else { return DetachStep::Skip("у активного окна нет приложения") };
    let own = |x: &&Client| x.address != c.address && x.app().as_deref() == Some(app.as_str());
    let in_ws = clients.iter().filter(own).filter(|x| x.in_ws(w)).count();
    let anywhere = clients.iter().filter(own).count();
    let drop_extra = extra.contains_key(&app) && in_ws == 0;
    let rest: Vec<String> = c.workspaces().into_iter().filter(|x| x != w).collect();
    if rest.is_empty() {
        return DetachStep::Close { app, drop_extra, last: anywhere == 0 };
    }
    let home = window_home(&rest, desks, current, Spot::of(c));
    DetachStep::Leave { app, home, drop_extra }
}

/// Снять запись дополнительного приложения сессии `app` после того, как
/// из workspace `ws` ушло последнее окно приложения (спецификация ws-daemon,
/// «Отделение окна»; изменение shared-windows, решение D9). Запись снимается
/// в `ws`; запись с командой запуска снимается и во всех остальных
/// workspace, если `last` — окно закрыто и других окон у приложения нет
/// нигде: программа уходит из сессии (изменение classless-windows-stay-free,
/// решение D3). Пока окна приложения остаются в других workspace, их записи
/// остаются. Вместе с записью снимается место в раскладке. Возвращает
/// workspace, где запись снята.
pub fn drop_extra_app(st: &mut State, ws: &str, app: &str, last: bool) -> Vec<String> {
    let everywhere = last && st.extra.get(ws).and_then(|m| m.get(app)).is_some_and(|e| !e.cmd.is_empty());
    let from: Vec<String> = st.extra.iter().filter(|(w, m)| m.contains_key(app) && (everywhere || w.as_str() == ws)).map(|(w, _)| w.clone()).collect();
    for w in &from {
        if let Some(m) = st.extra.get_mut(w) {
            m.remove(app);
        }
        if let Some(cells) = st.cells.get_mut(w) {
            cells.remove(app);
        }
        if let Some(m) = st.moved.get_mut(w) {
            m.remove(app);
        }
    }
    st.moved.retain(|_, m| !m.is_empty());
    st.extra.retain(|_, m| !m.is_empty());
    from
}

/// Стол `n` принимает workspace (спецификация ws-daemon, «Поднятие workspace
/// на столе», решение D16). Workspace числится ровно на одном столе, поэтому
/// из списков остальных столов он убирается; на столе, где он был активным,
/// активного workspace здесь не остаётся, преемника поднимает команда
/// переноса (`successor`). Ленивое поднятие, назначенное этому
/// workspace на прежнем столе, тоже снимается: там его больше нет, и первый
/// переход на тот стол не должен возвращать workspace туда.
pub fn assign_desktop(st: &mut State, ws: &str, n: u8) {
    for (k, d) in st.desktops.iter_mut() {
        if *k == n {
            continue;
        }
        d.workspaces.retain(|x| x != ws);
        if d.active.as_deref() == Some(ws) {
            d.active = None;
        }
    }
    st.lazy.retain(|k, w| *k == n || w != ws);
    let d = st.desktop(n);
    if !d.workspaces.iter().any(|x| x == ws) {
        d.workspaces.push(ws.to_string());
    }
    // История активности стола: вытесненный workspace становится прежним
    // активным. Стол, с которого workspace ушёл, историю не меняет: там она
    // по-прежнему называет workspace, активный до ушедшего.
    let old = d.active.replace(ws.to_string());
    if let Some(old) = old.filter(|o| o != ws) {
        st.prev_active.insert(n, old);
    }
}

/// Преемник на столе, с которого workspace `moved` перенесён на другой стол
/// (спецификация ws-daemon, «Перенос workspace на стол»). `list` — список
/// стола до переноса, `prev` — workspace, активный на столе до перенесённого.
/// Преемник — `prev`, если он ещё в списке; иначе workspace, стоящий в списке
/// перед перенесённым, а для первого в списке — последний (по кругу). Кроме
/// перенесённого, в списке никого нет — преемника нет.
pub fn successor(list: &[String], moved: &str, prev: Option<&str>) -> Option<String> {
    if let Some(p) = prev.filter(|p| *p != moved && list.iter().any(|w| w == p)) {
        return Some(p.to_string());
    }
    let rest: Vec<&String> = list.iter().filter(|w| *w != moved).collect();
    if rest.is_empty() {
        return None;
    }
    let idx = list.iter().position(|w| w == moved).unwrap_or(0);
    // Workspace перед перенесённым; перенесённый первый — последний в списке.
    let pick = if idx > 0 { &list[idx - 1] } else { rest[rest.len() - 1] };
    Some(pick.clone())
}

/// Следующий workspace в списке стола (спецификация ws-daemon, сценарий
/// «Следующий workspace на столе»): тот, что идёт за активным по кругу,
/// а без активного — первый в списке. Список пуст — следующего нет.
pub fn next_ws(d: &Desktop) -> Option<String> {
    if d.workspaces.is_empty() {
        return None;
    }
    let idx = d.active.as_ref().and_then(|a| d.workspaces.iter().position(|w| w == a)).map(|i| (i + 1) % d.workspaces.len()).unwrap_or(0);
    Some(d.workspaces[idx].clone())
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

/// Куда поднятие workspace ставит окно его приложения (спецификация ws-daemon,
/// «Поднятие workspace на столе», «Перенос workspace на стол», «Расстановка
/// окон»; изменение live-layout, решение D8). `carried` — окно едет вместе
/// с workspace с прежнего стола при переносе; `moving` — окно приходит
/// на стол (с `special:pool`, с другого стола или из вытесняемого
/// workspace); `kept` — прямоугольник, запомненный для окна в этом
/// workspace; `place` — место приложения в раскладке; `current` —
/// прямоугольник окна у композитора до поднятия. `None` — окно не двигается.
///
/// Режим workspace роли не играет: переносимое окно встаёт ровно туда, где
/// стояло; приходящее — в запомненный прямоугольник, а без него на место
/// приложения; окно, уже стоящее на столе, поднятие не двигает.
pub fn raise_target(carried: bool, moving: bool, kept: Option<PxRect>, place: Option<PxRect>, current: PxRect) -> Option<PxRect> {
    if carried {
        Some(current)
    } else if moving {
        kept.or(place)
    } else {
        None
    }
}

/// Порядок, в котором окна workspace поднимаются наверх при его поднятии
/// (спецификация ws-daemon, «Поднятие workspace на столе»). На вход идут адреса
/// окон workspace в нынешнем порядке по глубине — снизу вверх, как их
/// перечисляет композитор, — адрес главного окна и признак, поднимать ли
/// главное окно наверх. Возвращается тот же порядок снизу вверх: подняв окна
/// наверх по очереди, демон получает ровно его. При поднятии главное окно
/// становится верхним, при переносе на другой стол порядок остаётся прежним.
pub fn depth_plan(order: &[String], main: Option<&str>, main_on_top: bool) -> Vec<String> {
    let mut plan = order.to_vec();
    if main_on_top && let Some(i) = main.and_then(|m| plan.iter().position(|a| a == m)) {
        let top = plan.remove(i);
        plan.push(top);
    }
    plan
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

/// Расстановка по команде с возвратом раскладки к описанию (изменение
/// live-layout, решение D10): изменённые места `ws` снимаются, окна стола
/// встают на исходные места нынешнего назначения (`arrange_plan`),
/// а запомненные прямоугольники окон `ws` заменяются местами плана у окон
/// этого стола и забываются у остальных. Назначение мест и главное
/// приложение не меняются.
pub fn arrange_layout(st: &mut State, cfg: &Config, ws: Option<&str>, windows: &[&Client], mon: (i32, i32)) -> Vec<(String, PxRect)> {
    if let Some(w) = ws {
        st.moved.remove(w);
    }
    let plan = arrange_plan(st, cfg, ws, windows, mon);
    if let Some(w) = ws {
        let ws_apps: Vec<String> = cfg.workspaces.get(w).map(|x| x.apps.keys().cloned().collect()).unwrap_or_default();
        let g = st.geom.entry(w.to_string()).or_default();
        g.clear();
        for (addr, r) in &plan {
            if windows.iter().any(|c| c.address == *addr && ws_app_of(cfg, w, &ws_apps, c).is_some()) {
                g.insert(addr.clone(), *r);
            }
        }
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

// ---- Восстановление экземпляров из снимка ------------------------------------

/// Workspace записи снимка, которые описывают её приложение в эффективном
/// конфиге: только в них окно может входить тегом состава (решение D7).
pub fn entry_members(cfg: &Config, e: &RestoreEntry) -> Vec<String> {
    e.workspaces.iter().filter(|w| ws_has_app(cfg, w, &e.app)).cloned().collect()
}

/// Место окна, восстановленного по записи снимка (изменение session-instances,
/// решения D7, D13): по правилу размещения (`window_home`) для нынешних
/// активных workspace столов (`actives`, `current` — стол композитора).
/// На столе, где активен его workspace, окно встаёт в прямоугольник записи
/// для этого workspace, а без него — на место своего приложения в раскладке,
/// одинаково в обоих режимах (изменение live-layout, решение D13); без
/// активного workspace — на `special:pool`. Скрытое при снимке окно восстанавливается
/// видимым: стол снимка для окна с workspace не учитывается. Запись без
/// workspace (свободное окно приложения) встаёт на свой стол в свой
/// прямоугольник; с `pool` — на `special:pool`; с `hidden` — на текущий стол.
pub fn restore_place(st: &mut State, cfg: &Config, e: &RestoreEntry, actives: &BTreeMap<u8, String>, current: u8, mon: (i32, i32)) -> (Home, Option<PxRect>) {
    let members = entry_members(cfg, e);
    if members.is_empty() {
        return match e.desktop.parse::<u8>() {
            Ok(n) if (1..=8).contains(&n) => (Home::Desktop(n), Some(e.rect)),
            _ if e.desktop == "pool" => (Home::Pool, None),
            _ => (Home::Desktop(current), Some(e.rect)),
        };
    }
    match window_home(&members, actives, current, Spot::Pool) {
        Home::Desktop(k) => {
            let Some(w) = actives.get(&k) else { return (Home::Desktop(k), None) };
            (Home::Desktop(k), e.rects.get(w).copied().or_else(|| st.rect_for(cfg, w, &e.app, mon)))
        }
        h => (h, None),
    }
}

/// Запись плана для появившегося окна (решение D5). По `ancestors` (pid окна
/// и его предки) ищется запись с командной строкой, для которой запущен один
/// из этих процессов. По `app` — ожидающая запись этого приложения без
/// командной строки: с наименьшим номером среди тех, у которых в списке
/// есть `workspace` (для него приложение запущено), а без таких — с наименьшим
/// номером вообще. Совпадение по pid проверяется первым.
pub fn match_restore(entries: &[RestoreEntry], app: Option<&str>, ancestors: &[i32], workspace: Option<&str>) -> Option<usize> {
    if let Some(i) = entries.iter().position(|e| matches!(e.state, RestoreState::Launched(pid) if ancestors.contains(&(pid as i32)))) {
        return Some(i);
    }
    let app = app?;
    let open: Vec<(usize, &RestoreEntry)> = entries.iter().enumerate().filter(|(_, e)| e.state == RestoreState::Waiting && e.cmd.is_empty() && e.app == app).collect();
    let by_ws = open.iter().filter(|(_, e)| workspace.is_some_and(|w| e.workspaces.iter().any(|x| x == w))).min_by_key(|(_, e)| e.instance);
    by_ws.or_else(|| open.iter().min_by_key(|(_, e)| e.instance)).map(|(i, _)| *i)
}

/// Что делает поднятие workspace для приложения без окон в нём (решение D11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaiseStep {
    /// Как прежде: окна других workspace в общее пользование, без них запуск.
    Normal,
    /// Первый экземпляр приложения из снимка входит в workspace и ещё
    /// не восстановлен: запустить приложение его командой, окон других
    /// workspace не отдавать.
    LaunchFirst,
    /// Окно экземпляра, запущенного по записи с командной строкой, ещё
    /// не появилось: ни отдачи окон, ни запуска.
    Wait,
}

/// Шаг поднятия `ws` для приложения `app` (решение D11). `has_windows` —
/// у приложения уже есть окна в `ws`, тогда восстановление ничего не меняет.
/// Первый экземпляр — запись с наименьшим номером среди записей приложения
/// в снимке, в том числе исполненных.
pub fn raise_restore_step(cfg: &Config, entries: &[RestoreEntry], app: &str, ws: &str, has_windows: bool) -> RaiseStep {
    if has_windows {
        return RaiseStep::Normal;
    }
    let in_ws = |e: &RestoreEntry| e.workspaces.iter().any(|w| w == ws);
    let first = entries.iter().filter(|e| e.app == app).min_by_key(|e| e.instance);
    if first.is_some_and(|e| e.state == RestoreState::Waiting && e.cmd.is_empty() && in_ws(e)) {
        return RaiseStep::LaunchFirst;
    }
    if entries.iter().any(|e| matches!(e.state, RestoreState::Launched(_)) && cfg.app_is(&e.app, app) && in_ws(e)) {
        return RaiseStep::Wait;
    }
    RaiseStep::Normal
}

/// Снять ожидания плана восстановления (решение D10); возвращает снятые
/// записи как `имя#номер` для журнала. `all` — весь план (загрузка сессии).
/// С `app` — ожидание окон этого приложения без командной строки (клавиша
/// приложения, `workspaced app`). Без `app` — все ожидания окон (команда
/// сохранения): записи, чей процесс запущен, и записи без командной строки
/// приложений из `restore_apps`. Записи, которые ждут поднятия своего
/// workspace, команда сохранения не снимает.
pub fn end_wait(entries: &mut [RestoreEntry], restore_apps: &BTreeSet<String>, app: Option<&str>, all: bool) -> Vec<String> {
    let mut out = Vec::new();
    for e in entries.iter_mut().filter(|e| e.open()) {
        let hit = all
            || match app {
                Some(a) => e.app == a && e.cmd.is_empty(),
                None => matches!(e.state, RestoreState::Launched(_)) || (e.cmd.is_empty() && restore_apps.contains(&e.app)),
            };
        if hit {
            e.state = RestoreState::Done;
            out.push(format!("{}#{}", e.app, e.instance));
        }
    }
    out
}

/// Номер экземпляра окна, восстановленного по записи с номером `num`
/// (решение D6): номер записи, если его нет среди живых окон приложения,
/// иначе наименьший свободный.
pub fn restored_number(clients: &[Client], app: &str, num: u32) -> u32 {
    let used = clients.iter().filter_map(|c| c.app_instance()).any(|(a, n)| a == app && n == num);
    if used { free_instance(clients, app) } else { num }
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

/// Захват окна: номер окна в списке клиентов, приложение и номер экземпляра;
/// `retag` — окну ставится новый тег экземпляра, иначе у окна уже есть свой
/// тег экземпляра и оно только входит в workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    pub idx: usize,
    pub app: String,
    pub num: u32,
    pub retag: bool,
}

/// Что захватить (спецификация ws-daemon, «Захват открытых окон
/// приложения»). Кандидат — окно не на `special:hidden`, чей класс
/// и заголовок подходят приложению, без тега или с тегом приложения,
/// которого нет в конфиге. Сопоставление идёт сначала по вариантам, затем
/// по семействам: более точное совпадение побеждает. Окна обычных столов
/// разбираются раньше окон на `special:pool`, поэтому первый экземпляр — окно
/// на столе. С `members` захват идёт для workspace, и кандидатом становится
/// ещё и свободное окно приложения — с тегом экземпляра одного из `apps`
/// (или варианта описанного семейства), но без тегов состава (изменение
/// shared-windows, решение D3): оно сохраняет свой тег экземпляра.
fn adopt_plan(cfg: &Config, clients: &[Client], apps: &[String], members: bool) -> Result<Vec<Capture>> {
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
        if c.on_hidden() {
            continue;
        }
        if let Some((own, num)) = c.app_instance().filter(|(a, _)| cfg.apps.contains_key(a)) {
            if members && !c.has_ws() && apps.iter().any(|x| cfg.app_is(&own, x)) {
                plan.push(Capture { idx: i, app: own, num, retag: false });
            }
            continue;
        }
        if cfg.ignored_class(&c.class) {
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
        plan.push(Capture { idx: i, app, num, retag: true });
    }
    Ok(plan)
}

/// Приложение workspace `ws`, которому принадлежит окно: само приложение,
/// если оно описано в workspace, иначе его семейство. Окно, не входящее
/// в `ws` по тегу состава, и окно чужого приложения дают `None`.
fn ws_app_of(cfg: &Config, ws: &str, ws_apps: &[String], c: &Client) -> Option<String> {
    if !c.in_ws(ws) {
        return None;
    }
    let own = c.app()?;
    if ws_apps.contains(&own) {
        return Some(own);
    }
    ws_apps.iter().find(|x| cfg.app_is(&own, x)).cloned()
}

/// Режим цикла клавиши приложения `app` в workspace `ws` и имя записи,
/// которую переносит обмен мест (изменение workspace-overrides, решения D6
/// и D7): режим — `Config::app_mode`, запись — собственная запись приложения,
/// а у варианта без неё — запись семейства.
pub fn cycle_mode(cfg: &Config, ws: &str, app: &str) -> (Mode, String) {
    let entry = cfg.ws_entry(ws, app).map(|(n, _)| n.to_string()).unwrap_or_else(|| app.to_string());
    (cfg.app_mode(ws, app), entry)
}

/// Чем начат цикл клавиши приложения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// Обычное нажатие: шаг цикла выводится из активного окна.
    Continue,
    /// Workspace поднят этой же цепочкой: цикл начинается заново.
    Raised,
    /// Приложение открыто или перетащено в workspace этой же цепочкой: цикл
    /// начинается заново, а первый экземпляр выбирается без обмена мест
    /// (решение D15).
    Joined,
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

/// Окно, к которому вернёт конец цикла (prev). В режиме обмена мест это
/// первое окно главного приложения на начало цикла (`main_window`): конец
/// цикла обязан вернуть расстановку. В режиме `stack` — активное окно
/// workspace, а если активно чужое окно, то последнее окно workspace,
/// получавшее фокус. Экземпляры самого вызванного приложения prev не бывают.
pub fn anchor_window(cfg: &Config, clients: &[Client], ws: &str, main_window: Option<&str>, order: &[String], active: Option<&str>, focus: Option<&str>) -> Option<String> {
    let ws_apps: Vec<String> = cfg.workspaces.get(ws).map(|w| w.apps.keys().cloned().collect()).unwrap_or_default();
    if let Some(m) = main_window {
        return Some(m.to_string());
    }
    let fits = |a: &str| {
        !order.iter().any(|x| x == a) && clients.iter().any(|c| c.address == a && !c.on_hidden() && ws_app_of(cfg, ws, &ws_apps, c).is_some())
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
        .find_map(|name| placed_windows(cfg, clients, ws, &names, name).into_iter().find(|c| !c.on_hidden()).map(|c| c.address.clone()))
}

/// Обмен мест в раскладке workspace (изменение live-layout, решения D5, D9):
/// вызванное приложение `app` и главное `main` меняются местами целиком —
/// исходным местом вместе с изменённым прямоугольником, — и главным
/// становится `app`. Без главного приложения `app` получает ячейку `main`
/// шаблона (`main_cell`). Изменённый прямоугольник, доставшийся приложению без
/// окон в workspace (`has_windows`), снимается: он принадлежал окну, а не
/// месту, и окно, которое появится, встанет на исходное место.
pub fn swap_places(st: &mut State, ws: &str, main: Option<&str>, app: &str, main_cell: &str, has_windows: &dyn Fn(&str) -> bool) {
    let cells = st.cells.entry(ws.to_string()).or_default();
    let app_place = cells.get(app).cloned();
    let main_place = main.and_then(|m| cells.get(m).cloned());
    if let Some(m) = main {
        match app_place {
            Some(p) => {
                cells.insert(m.to_string(), p);
            }
            None => {
                cells.remove(m);
            }
        }
    }
    cells.insert(app.to_string(), main_place.unwrap_or_else(|| Place::Cell(main_cell.to_string())));
    let moved = st.moved.entry(ws.to_string()).or_default();
    let app_moved = moved.remove(app);
    let main_moved = main.and_then(|m| moved.remove(m));
    if let (Some(m), Some(x)) = (main, app_moved) {
        moved.insert(m.to_string(), x);
    }
    if let Some(x) = main_moved {
        moved.insert(app.to_string(), x);
    }
    for name in [main, Some(app)].into_iter().flatten() {
        if !has_windows(name) {
            moved.remove(name);
        }
    }
    st.moved.retain(|_, m| !m.is_empty());
    st.main.insert(ws.to_string(), app.to_string());
}

/// Нажатие клавиши неглавного приложения в режиме обмена мест (изменение
/// live-layout, решение D7): цикл начинается заново, и выбирается активное
/// окно, если оно экземпляр вызванного приложения, иначе первый экземпляр.
/// Окно, которое пользователь только что двигал мышью, активно, и клавиша
/// ставит его на главное место, а не заканчивает цикл. `None` — приложение
/// уже главное (или окон нет), и шаг выводится по общему правилу
/// (`cycle_step`).
pub fn swap_start(order: &[String], active: Option<&str>, is_main: bool) -> Option<String> {
    if is_main {
        return None;
    }
    let first = order.first()?;
    Some(active.filter(|a| order.iter().any(|x| x == a)).unwrap_or(first).to_string())
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

    /// План захвата в виде «номер окна, приложение, номер экземпляра».
    fn triples(plan: Vec<Capture>) -> Vec<(usize, String, u32)> {
        plan.into_iter().map(|c| (c.idx, c.app, c.num)).collect()
    }

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

    /// Тот же конфиг с диалогами и списком исключённых классов, а также
    /// с дополнительным приложением сессии `alacritty` в workspace `work`.
    fn cfg_with_dialogs() -> Config {
        let text = format!("ignore_classes = [\"zenity\", \"pinentry.*\"]\n{CFG}").replace(
            "[apps.chromium]\ncmd = \"chromium\"\n",
            "[apps.chromium]\ncmd = \"chromium\"\ndialog_title = \"^(Диспетчер задач|Task Manager)$\"\n",
        );
        Config::parse(&text).unwrap()
    }

    #[test]
    fn join_takes_new_window_into_the_active_workspace() {
        let cfg = Config::parse(CFG).unwrap();
        let taken: Vec<String> = cfg.apps.keys().cloned().collect();
        let join = |ws, c: &Client, has_cmd: bool| {
            let cmd: Vec<String> = if has_cmd { vec!["/usr/bin/prog".into()] } else { Vec::new() };
            join_window(&cfg, &cfg, ws, c, &cmd, &taken)
        };

        // Окно приложения, уже входящего в workspace: только тег и место.
        let chromium = client("0x1", "chromium", "Новости", "1", &[]);
        assert_eq!(join(Some("work"), &chromium, true), Join::App("chromium".into()));

        // Приложение конфига, которого в workspace нет: к тегу добавляется
        // запись дополнительного приложения сессии — только о месте.
        let calc = client("0x2", "qalculate-gtk", "Калькулятор", "1", &[]);
        assert_eq!(join(Some("work"), &calc, true), Join::AppPlace("calc".into()));

        // Приложения в конфиге нет: новое дополнительное приложение по классу.
        let term = client("0x3", "Alacritty", "mne@dev-lab", "1", &[]);
        assert_eq!(join(Some("work"), &term, true), Join::Extra("alacritty".into()));

        // На столе без активного workspace окно остаётся свободным, а окно
        // приложения конфига всё равно захватывается (шаг 2).
        assert_eq!(join(None, &term, true), Join::Free(Free::NoWorkspace));
        assert_eq!(join(None, &chromium, true), Join::App("chromium".into()));

        // Окно без командной строки принимать некуда: восстановить его нечем.
        assert_eq!(join(Some("work"), &term, false), Join::Free(Free::NoCommand));
    }

    #[test]
    fn dialogs_and_ignored_classes_stay_free() {
        let cfg = cfg_with_dialogs();
        let taken: Vec<String> = cfg.apps.keys().cloned().collect();
        let join = |c: &Client| join_window(&cfg, &cfg, Some("work"), c, &["/usr/bin/prog".to_string()], &taken);

        // Диалог приложения приходит с классом самого приложения и отличается
        // только заголовком.
        let dialog = client("0x1", "chromium", "Диспетчер задач", "1", &[]);
        assert_eq!(join(&dialog), Join::Free(Free::Dialog("chromium".into())));
        let page = client("0x2", "chromium", "Новости", "1", &[]);
        assert_eq!(join(&page), Join::App("chromium".into()));

        // Диалог с отдельным классом снимается списком ignore_classes.
        let ask = client("0x3", "zenity", "Пароль", "1", &[]);
        assert_eq!(join(&ask), Join::Free(Free::Ignored));
        let pin = client("0x4", "pinentry-gtk", "Пароль", "1", &[]);
        assert_eq!(join(&pin), Join::Free(Free::Ignored));
    }

    #[test]
    fn classless_windows_stay_free() {
        // Диспетчер задач Chromium и Яндекс.Браузера приходит без класса.
        let cfg = cfg_with_dialogs();
        let taken: Vec<String> = cfg.apps.keys().cloned().collect();
        let cmd = vec!["/usr/lib/chromium/chromium".to_string()];
        let tm = client("0x1", "", "Диспетчер задач – Chromium", "1", &[]);
        assert_eq!(join_window(&cfg, &cfg, Some("work"), &tm, &cmd, &taken), Join::Free(Free::NoClass));
        assert_eq!(join_window(&cfg, &cfg, None, &tm, &cmd, &taken), Join::Free(Free::NoClass));
        let blank = client("0x2", "  ", "", "1", &[]);
        assert_eq!(join_window(&cfg, &cfg, Some("work"), &blank, &cmd, &taken), Join::Free(Free::NoClass));

        // Выражение, подходящее любому классу, окна без класса не берёт:
        // ни при сопоставлении, ни при захвате открытых окон.
        let any = regex::Regex::new("^(?:.*)$").unwrap();
        assert!(!matcher_fits(Some(&any), None, &tm));
        let mut extra: BTreeMap<String, BTreeMap<String, ExtraApp>> = BTreeMap::new();
        extra.entry("work".into()).or_default().insert("app".into(), ExtraApp { class: Some(String::new()), cmd: cmd.clone(), cwd: None, rect: PxRect::default() });
        let (merged, _) = merge_extra(&cfg, &extra);
        assert_eq!(app_for_window(&merged, &tm), None);
        let plan = triples(adopt_plan(&merged, std::slice::from_ref(&tm), &["app".to_string()], true).unwrap());
        assert!(plan.is_empty());
    }

    #[test]
    fn same_class_of_another_program_gets_its_own_name() {
        let text = format!("{CFG}\n[workspaces.surf]\ntemplate = \"thirds\"\napps = {{ chromium = \"center\" }}\n");
        let file = Config::parse(&text).unwrap();
        let mut extra: BTreeMap<String, BTreeMap<String, ExtraApp>> = BTreeMap::new();
        extra.entry("work".into()).or_default().insert("alacritty".into(), ExtraApp { class: Some("Alacritty".into()), cmd: vec!["/usr/bin/alacritty".into()], cwd: None, rect: PxRect::default() });
        let (cfg, _) = merge_extra(&file, &extra);
        let taken = taken_names(&cfg, &extra);
        let term = client("0x1", "Alacritty", "mne@dev-lab", "2", &[]);
        // Та же программа в другом workspace — то же приложение сессии.
        assert_eq!(join_window(&cfg, &file, Some("surf"), &term, &["/usr/bin/alacritty".to_string()], &taken), Join::Extra("alacritty".into()));
        // Другая программа с тем же классом чужой записи не получает.
        assert_eq!(join_window(&cfg, &file, Some("surf"), &term, &["/opt/other/alacritty".to_string()], &taken), Join::Extra("alacritty-2".into()));
    }

    #[test]
    fn detach_drops_session_app_in_every_workspace() {
        let mut st = State::default();
        let full = ExtraApp { class: Some("wev".into()), cmd: vec!["/usr/bin/wev".into()], cwd: None, rect: PxRect::default() };
        for w in ["work", "surf"] {
            st.extra.entry(w.into()).or_default().insert("wev".into(), full.clone());
            st.cells.entry(w.into()).or_default().insert("wev".into(), Place::Rect { rect: PxRect::default() });
        }
        st.extra.get_mut("surf").unwrap().insert("calc".into(), ExtraApp::default());
        st.extra.entry("work".into()).or_default().insert("calc".into(), ExtraApp::default());
        // Запись с командой снимается везде, где есть, вместе с местом.
        assert_eq!(drop_extra_app(&mut st, "surf", "wev", true), vec!["surf".to_string(), "work".to_string()]);
        assert!(st.extra.values().all(|m| !m.contains_key("wev")));
        assert!(st.cells.values().all(|m| !m.contains_key("wev")));
        // Запись только о месте приложения конфига снимается в одном workspace.
        assert_eq!(drop_extra_app(&mut st, "surf", "calc", true), vec!["surf".to_string()]);
        assert!(st.extra["work"].contains_key("calc"));
        assert!(!st.extra.contains_key("surf"));
    }

    #[test]
    fn launchable_command_from_proc() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        // Chromium переписывает командную строку: одна строка с пробелами.
        assert_eq!(launchable(&s(&["/bin/sh -c true"]), None, None), Some(s(&["/bin/sh", "-c", "true"])));
        // Имя в PATH остаётся как есть.
        assert_eq!(launchable(&s(&["sh", "-c", "true"]), None, None), Some(s(&["sh", "-c", "true"])));
        // Относительный путь — от рабочего каталога процесса.
        let dir = std::env::temp_dir().join(format!("workspaced-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("helper"), "").unwrap();
        let d = dir.to_string_lossy().into_owned();
        let abs = dir.join("helper").to_string_lossy().into_owned();
        assert_eq!(launchable(&s(&["./helper", "-x"]), Some(&d), None), Some(vec![abs, "-x".to_string()]));
        // Файл не нашёлся — берётся /proc/<pid>/exe, без него команды нет.
        assert_eq!(launchable(&s(&["./gone", "-x"]), Some(&d), Some(Path::new("/bin/sh"))), Some(s(&["/bin/sh", "-x"])));
        assert_eq!(launchable(&s(&["./gone"]), Some(&d), None), None);
        assert_eq!(launchable(&s(&["no-such-program-xyz"]), None, None), None);
        assert_eq!(launchable(&s(&["/no/such/file"]), None, Some(Path::new("/no/such/exe"))), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn launch_failure_names_command_and_directory() {
        let home = Path::new("/tmp");
        assert_eq!(launch_dir(Some("/"), home), (PathBuf::from("/"), None));
        let (dir, warn) = launch_dir(Some("/no/such/dir"), home);
        assert_eq!(dir, PathBuf::from("/tmp"));
        assert!(warn.unwrap().contains("/no/such/dir"));
        assert_eq!(launch_dir(None, home), (PathBuf::from("/tmp"), None));

        let err = format!("{:#}", start("/no/such/program", &["-a".to_string()], Some("/"), &BTreeMap::new()).unwrap_err());
        assert!(err.contains("/no/such/program") && err.contains("-a") && err.contains("каталоге /") && err.contains("os error 2"), "{err}");
        // Исчезнувший рабочий каталог запуск не срывает.
        let mut child = start("true", &[], Some("/no/such/dir"), &BTreeMap::new()).unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn extra_app_names_do_not_repeat() {
        let cfg = Config::parse(CFG).unwrap();
        let mut extra: BTreeMap<String, BTreeMap<String, ExtraApp>> = BTreeMap::new();
        extra.entry("work".into()).or_default().insert("alacritty".into(), ExtraApp::default());
        let taken = taken_names(&cfg, &extra);
        assert!(taken.contains(&"chromium".to_string()) && taken.contains(&"alacritty".to_string()));
        // Имя строится из класса; занятое дополняется номером.
        assert_eq!(app_name("Alacritty", &taken), "alacritty-2");
        assert_eq!(app_name("org.telegram.desktop", &taken), "org-telegram-desktop");
        assert_eq!(app_name("!!!", &taken), "app");
    }

    /// Активные workspace столов для тестов: `work` на столе 1, `surf` на столе 2.
    fn desks(pairs: &[(u8, &str)]) -> BTreeMap<u8, String> {
        pairs.iter().map(|(n, w)| (*n, w.to_string())).collect()
    }

    #[test]
    fn detach_closes_last_membership() {
        let cfg = Config::parse(CFG).unwrap();
        let clients = vec![
            client("0x1", "wezterm-herdr", "herdr · dev-lab", "1", &["app:herdr#1", "ws:work"]),
            client("0x2", "chromium", "Новости", "1", &["app:chromium#1", "ws:work"]),
            client("0x3", "Alacritty", "mne@dev-lab", "1", &["app:alacritty#1"]),
            client("0x4", "firefox", "Видео", "1", &[]),
        ];
        let extra: BTreeMap<String, ExtraApp> = BTreeMap::new();
        let d = desks(&[(1, "work")]);
        let step = |active: Option<&Client>, extra: &BTreeMap<String, ExtraApp>| detach_step(&cfg, &clients, Some("work"), extra, active, &d, 1);

        // Окно входит только в `work`: оно закрывается, других окон нет нигде.
        assert_eq!(step(Some(&clients[1]), &extra), DetachStep::Close { app: "chromium".into(), drop_extra: false, last: true });
        // Свободное окно ни в какой workspace не входит, трогать его незачем.
        assert!(matches!(step(Some(&clients[3]), &extra), DetachStep::Skip(_)));
        // Окно с тегом экземпляра, но без тега состава, — тоже свободное.
        assert!(matches!(step(Some(&clients[2]), &extra), DetachStep::Skip(_)));
        // Без активного окна и без активного workspace команда ничего не делает.
        assert!(matches!(step(None, &extra), DetachStep::Skip(_)));
        assert!(matches!(detach_step(&cfg, &clients, None, &extra, Some(&clients[1]), &d, 1), DetachStep::Skip(_)));

        // Окно чужого workspace командой не задевается, даже если его
        // приложение описано в активном.
        let alien = client("0x5", "neovide", "[Scratch]", "1", &["app:neovide#1", "ws:dev-front"]);
        let others = vec![alien.clone()];
        assert!(matches!(detach_step(&cfg, &others, Some("work"), &extra, Some(&alien), &d, 1), DetachStep::Skip(_)));

        // Второе окно того же приложения в другом workspace: это окно
        // закрывается, но оно не последнее у приложения.
        let two = vec![clients[1].clone(), client("0x6", "chromium", "Документы", "3", &["app:chromium#2", "ws:dev-front"])];
        assert_eq!(detach_step(&cfg, &two, Some("work"), &extra, Some(&two[0]), &d, 1), DetachStep::Close { app: "chromium".into(), drop_extra: false, last: false });
    }

    #[test]
    fn detach_leaves_shared_window() {
        let cfg = Config::parse(CFG).unwrap();
        let extra: BTreeMap<String, ExtraApp> = [("chromium".to_string(), ExtraApp::default())].into();
        let shared = client("0x1", "chromium", "Новости", "1", &["app:chromium#1", "ws:work", "ws:surf"]);
        let clients = vec![shared.clone()];
        // Другой workspace окна активен на столе 2: окно уходит туда открытым,
        // а запись о месте снимается — из `work` ушло последнее окно приложения.
        let d = desks(&[(1, "work"), (2, "surf")]);
        assert_eq!(detach_step(&cfg, &clients, Some("work"), &extra, Some(&shared), &d, 1), DetachStep::Leave { app: "chromium".into(), home: Home::Desktop(2), drop_extra: true });
        // Другой workspace не активен нигде: окно уходит на special:pool.
        let d = desks(&[(1, "work")]);
        assert_eq!(detach_step(&cfg, &clients, Some("work"), &extra, Some(&shared), &d, 1), DetachStep::Leave { app: "chromium".into(), home: Home::Pool, drop_extra: true });
        // Второе окно приложения остаётся в `work`: запись о месте не снимается.
        let two = vec![shared.clone(), client("0x2", "chromium", "Почта", "1", &["app:chromium#2", "ws:work"])];
        assert!(matches!(detach_step(&cfg, &two, Some("work"), &extra, Some(&shared), &d, 1), DetachStep::Leave { drop_extra: false, .. }));
    }

    #[test]
    fn drop_extra_keeps_records_of_other_members() {
        let full = ExtraApp { class: Some("Alacritty".into()), cmd: vec!["/usr/bin/alacritty".into()], cwd: None, rect: PxRect::default() };
        let fresh = || {
            let mut st = State::default();
            for w in ["work", "dev-front"] {
                st.extra.entry(w.into()).or_default().insert("alacritty".into(), full.clone());
                st.cells.entry(w.into()).or_default().insert("alacritty".into(), Place::Rect { rect: PxRect::default() });
            }
            st
        };
        // Окно ушло из `work`, но осталось в `dev-front`: запись снимается только в `work`.
        let mut st = fresh();
        assert_eq!(drop_extra_app(&mut st, "work", "alacritty", false), vec!["work".to_string()]);
        assert!(st.extra["dev-front"].contains_key("alacritty"));
        assert!(st.cells["dev-front"].contains_key("alacritty") && !st.cells["work"].contains_key("alacritty"));
        // Окно закрыто, и других окон у приложения нет нигде: запись с командой
        // снимается во всех workspace.
        let mut st = fresh();
        assert_eq!(drop_extra_app(&mut st, "work", "alacritty", true), vec!["dev-front".to_string(), "work".to_string()]);
        assert!(st.extra.is_empty());
    }

    /// Конфиг с тремя workspace: `work` (обмен мест), `surf` (`stack`)
    /// и `dev-back`; `chrome-ai` описан в `surf` и `dev-back`.
    fn shared_cfg() -> Config {
        let text = format!(
            "{CFG}\n[apps.chrome-ai]\ncmd = \"google-chrome\"\nclass = \"^google-chrome-ai$\"\n\n[workspaces.surf]\ntemplate = \"thirds\"\nmode = \"stack\"\napps = {{ chrome-ai = \"right\" }}\n\n[workspaces.dev-back]\ntemplate = \"thirds\"\napps = {{ chrome-ai = \"center\", neovide = \"right\" }}\n\n[workspaces.chat]\ntemplate = \"thirds\"\napps = {{ calc = \"center\" }}\n"
        );
        Config::parse(&text).unwrap()
    }

    #[test]
    fn ws_windows_by_tags() {
        let cfg = shared_cfg();
        let clients = vec![
            client("0x1", "chromium", "Новости", "1", &["app:chromium#1", "ws:work"]),
            // Окно приложения `work`, убранное из него: в состав не входит.
            client("0x2", "chromium", "Почта", "3", &["app:chromium#2"]),
            client("0x3", "google-chrome-ai", "ИИ", "2", &["app:chrome-ai#1", "ws:surf", "ws:work"]),
        ];
        let addrs = |ws: &str| ws_windows(&clients, ws).iter().map(|c| c.address.clone()).collect::<Vec<_>>();
        assert_eq!(addrs("work"), vec!["0x1", "0x3"]);
        assert_eq!(addrs("surf"), vec!["0x3"]);
        assert!(addrs("dev-back").is_empty());
        // Цикл и расстановка берут окна приложения только из своего workspace.
        let apps: Vec<String> = vec!["herdr".into(), "chromium".into(), "neovide".into()];
        assert_eq!(placed_windows(&cfg, &clients, "work", &apps, "chromium").len(), 1);
        // Приложение workspace узнаётся только у окна, которое в него входит.
        assert_eq!(ws_app_of(&cfg, "work", &apps, &clients[0]).as_deref(), Some("chromium"));
        assert_eq!(ws_app_of(&cfg, "work", &apps, &clients[1]), None);
        // Следующее приложение workspace — тоже по составу.
        assert_eq!(next_app_window(&cfg, &clients, "work", "herdr").as_deref(), Some("0x1"));
    }

    #[test]
    fn joined_window_gets_ws_tag() {
        // Окно, запущенное для workspace или появившееся на столе с активным
        // workspace, получает тег экземпляра и тег состава.
        let ex = entry_tags("0x1", "chromium", 2, Some("work"));
        assert_eq!(ex, vec![hypr::d_tag("0x1", "app:chromium#2"), hypr::d_tag("0x1", "ws:work")]);
        // На столе без активного workspace окно остаётся свободным окном приложения.
        assert_eq!(entry_tags("0x1", "calc", 1, None), vec![hypr::d_tag("0x1", "app:calc#1")]);
        // Захват для workspace: свободное окно с тегом экземпляра сохраняет
        // номер и только входит в workspace, окно без тега получает номер.
        let cfg = shared_cfg();
        let apps: Vec<String> = vec!["herdr".into(), "chromium".into(), "neovide".into()];
        let clients = vec![
            client("0x1", "chromium", "Новости", "1", &["app:chromium#1", "ws:work"]),
            client("0x2", "chromium", "Почта", "4", &["app:chromium#2"]),
            client("0x3", "chromium", "Документы", "4", &[]),
            // Окно чужого приложения с тегом экземпляра не захватывается.
            client("0x4", "google-chrome-ai", "ИИ", "4", &["app:chrome-ai#1"]),
        ];
        let plan = adopt_plan(&cfg, &clients, &apps, true).unwrap();
        assert_eq!(plan, vec![Capture { idx: 1, app: "chromium".into(), num: 2, retag: false }, Capture { idx: 2, app: "chromium".into(), num: 3, retag: true }]);
        // Вне workspace окна с тегом экземпляра не захватываются.
        assert_eq!(triples(adopt_plan(&cfg, &clients, &apps, false).unwrap()), vec![(2, "chromium".to_string(), 3)]);
    }

    #[test]
    fn stale_ws_tags_follow_effective_config() {
        let cfg = shared_cfg();
        let clients = vec![
            // chrome-ai перетащен в work, а записи о месте в снимке нет.
            client("0x1", "google-chrome-ai", "ИИ", "2", &["app:chrome-ai#1", "ws:surf", "ws:work"]),
            // Workspace из конфига исчез.
            client("0x2", "chromium", "Новости", "1", &["app:chromium#1", "ws:work", "ws:gone"]),
            // Окно варианта входит туда, где описано семейство.
            client("0x3", "wezterm-herdr", "herdr · dev-lab", "1", &["app:herdr#1", "ws:work"]),
            // Окно без приложения не входит никуда.
            client("0x4", "firefox", "Видео", "4", &["ws:work"]),
        ];
        let stale = stale_ws_tags(&cfg, &clients);
        assert_eq!(stale, vec![("0x1".to_string(), "work".to_string()), ("0x2".to_string(), "gone".to_string()), ("0x4".to_string(), "work".to_string())]);
        // Запись о месте в эффективном конфиге делает тег законным.
        let mut extra: BTreeMap<String, BTreeMap<String, ExtraApp>> = BTreeMap::new();
        extra.entry("work".into()).or_default().insert("chrome-ai".into(), ExtraApp::default());
        let (merged, _) = merge_extra(&cfg, &extra);
        assert!(!stale_ws_tags(&merged, &clients).iter().any(|(a, _)| a == "0x1"));
    }

    #[test]
    fn membership_is_derived_for_old_windows() {
        let cfg = shared_cfg();
        let mut st = State::default();
        st.desktops.insert(1, Desktop { workspaces: vec!["surf".into(), "work".into()], active: Some("work".into()) });
        st.desktops.insert(3, Desktop { workspaces: vec!["dev-back".into()], active: None });
        let clients = vec![
            client("0x1", "chromium", "Новости", "1", &["app:chromium#1"]),
            // Окно на pool: во все неактивные workspace списков, описывающие приложение.
            client("0x2", "google-chrome-ai", "ИИ", "special:pool", &["app:chrome-ai#1"]),
            // Окно на столе без активного workspace остаётся свободным.
            client("0x3", "neovide", "[Scratch]", "4", &["app:neovide#1"]),
            // Окно с тегом состава не трогается.
            client("0x4", "neovide", "[Scratch]", "1", &["app:neovide#2", "ws:work"]),
            // Скрытое окно и свободное окно без тега — тоже.
            client("0x5", "neovide", "[Scratch]", "special:hidden", &["app:neovide#3"]),
            client("0x6", "firefox", "Видео", "1", &[]),
            // Приложение активного workspace стола не описано в нём: окно свободно.
            client("0x7", "google-chrome-ai", "ИИ", "1", &["app:chrome-ai#2"]),
        ];
        let got = derive_membership(&cfg, &st, &clients);
        let p = |a: &str, w: &str| (a.to_string(), w.to_string());
        assert_eq!(got, vec![p("0x1", "work"), p("0x2", "dev-back"), p("0x2", "surf")]);
    }

    #[test]
    fn window_home_rules() {
        let m = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let d = desks(&[(1, "work"), (2, "surf"), (4, "chat")]);
        // 1. Стол композитора, если активный там workspace содержит окно.
        assert_eq!(window_home(&m(&["surf", "work"]), &d, 1, Spot::Desktop(2)), Home::Desktop(1));
        assert_eq!(window_home(&m(&["surf", "work"]), &d, 2, Spot::Desktop(1)), Home::Desktop(2));
        // 2. Пользователь на столе, где окна нет: окно остаётся на своём столе.
        assert_eq!(window_home(&m(&["surf", "work"]), &d, 4, Spot::Desktop(2)), Home::Desktop(2));
        // 3. Своего стола нет: стол с наименьшим номером, где активен его workspace.
        assert_eq!(window_home(&m(&["surf", "work"]), &d, 4, Spot::Pool), Home::Desktop(1));
        assert_eq!(window_home(&m(&["surf"]), &d, 5, Spot::Desktop(1)), Home::Desktop(2));
        // 4. Ни один workspace окна не активен: special:pool.
        assert_eq!(window_home(&m(&["dev-back"]), &d, 1, Spot::Desktop(1)), Home::Pool);
        // Скрытое окно правило не трогает никогда.
        assert_eq!(window_home(&m(&["work"]), &d, 1, Spot::Hidden), Home::Keep);
    }

    #[test]
    fn raise_shares_windows_only_when_missing() {
        let cfg = shared_cfg();
        let apps: Vec<String> = vec!["chrome-ai".into(), "neovide".into()];
        let clients = vec![
            client("0x1", "google-chrome-ai", "ИИ", "2", &["app:chrome-ai#1", "ws:surf"]),
            client("0x2", "neovide", "[Scratch]", "1", &["app:neovide#1", "ws:work"]),
            client("0x3", "neovide", "[Scratch]", "5", &["app:neovide#2", "ws:dev-back"]),
            client("0x4", "google-chrome-ai", "ИИ", "special:hidden", &["app:chrome-ai#2", "ws:surf"]),
        ];
        // У chrome-ai окна в dev-back нет: его нескрытое окно из surf становится
        // общим. У neovide окно в dev-back есть: окно work к нему не добавляется.
        assert_eq!(share_plan(&cfg, &clients, "dev-back", &apps), vec![(0, "chrome-ai".to_string())]);
        // Скрытое окно workspace считается его окном: второе не добавляется.
        let hidden = vec![client("0x4", "google-chrome-ai", "ИИ", "special:hidden", &["app:chrome-ai#2", "ws:dev-back"]), clients[0].clone()];
        assert!(share_plan(&cfg, &hidden, "dev-back", &["chrome-ai".to_string()]).is_empty());
        // Свободные окна (без состава) отдаёт не этот план, а захват.
        let free = vec![client("0x5", "google-chrome-ai", "ИИ", "4", &["app:chrome-ai#1"])];
        assert!(share_plan(&cfg, &free, "dev-back", &["chrome-ai".to_string()]).is_empty());
    }

    #[test]
    fn displaced_shared_window_goes_to_its_other_desktop() {
        // chrome-ai входит в work (стол 1) и dev-back (стол 3), на столе 1
        // поднят chat: столы после поднятия — chat на 1, dev-back на 3.
        let after = desks(&[(1, "chat"), (3, "dev-back")]);
        let shared = client("0x1", "google-chrome-ai", "ИИ", "1", &["app:chrome-ai#1", "ws:work", "ws:dev-back"]);
        assert_eq!(window_home(&shared.workspaces(), &after, 1, Spot::of(&shared)), Home::Desktop(3));
        // Окно только из work уходит на special:pool.
        let own = client("0x2", "chromium", "Новости", "1", &["app:chromium#1", "ws:work"]);
        assert_eq!(window_home(&own.workspaces(), &after, 1, Spot::of(&own)), Home::Pool);
        // При переходе в сам dev-back окно встаёт на месте его приложения там.
        let cfg = shared_cfg();
        let mut st = State::default();
        assert_eq!(arrive_rect(&mut st, &cfg, "dev-back", &shared, (3840, 2160)), Some(PxRect { x: 1125, y: 10, w: 1920, h: 2140 }));
        // Окно, не входящее в workspace, места там не получает и не двигается.
        assert_eq!(arrive_rect(&mut st, &cfg, "dev-back", &own, (3840, 2160)), None);
    }

    #[test]
    fn shared_window_arrives_into_raised_workspace() {
        let shared = client("0x1", "google-chrome-ai", "ИИ", "1", &["app:chrome-ai#1", "ws:surf", "ws:work"]);
        // На столе 1 был активен work, поднимается surf: общее окно приходит
        // в surf и встаёт в его прямоугольник, хотя стол не меняет.
        assert!(arrives(&shared, 1, &desks(&[(1, "work")]), "surf"));
        // Повторное поднятие surf на том же столе окно не двигает.
        assert!(!arrives(&shared, 1, &desks(&[(1, "surf")]), "surf"));
        // Окно с другого стола приходит всегда.
        assert!(arrives(&shared, 2, &desks(&[(2, "surf")]), "surf"));
        // Окно стола, не входившее в вытесняемый workspace, не приходит.
        let own = client("0x2", "google-chrome", "Новости", "1", &["app:chrome#1", "ws:surf"]);
        assert!(!arrives(&own, 1, &desks(&[(1, "work")]), "surf"));
        // Вместе с raise_target: пришедшее окно встаёт в запомненный
        // прямоугольник, иначе — на место из раскладки.
        let kept = PxRect { x: 1700, y: 60, w: 1920, h: 2140 };
        let cell = PxRect { x: 1910, y: 10, w: 1920, h: 2140 };
        assert_eq!(raise_target(false, true, Some(kept), Some(cell), shared.rect()), Some(kept));
        assert_eq!(raise_target(false, false, Some(kept), Some(cell), shared.rect()), None);
    }

    #[test]
    fn follow_plan_moves_shared_windows() {
        let cfg = shared_cfg();
        let mon = (3840, 2160);
        let mut st = State::default();
        st.desktops.insert(1, Desktop { workspaces: vec!["work".into()], active: Some("work".into()) });
        st.desktops.insert(2, Desktop { workspaces: vec!["surf".into()], active: Some("surf".into()) });
        st.extra.entry("work".into()).or_default().insert("chrome-ai".into(), ExtraApp { rect: PxRect { x: 1000, y: 500, w: 1920, h: 1080 }, ..ExtraApp::default() });
        let (cfg, _) = merge_extra(&cfg, &st.extra);
        let moved = PxRect { x: 1900, y: 100, w: 1920, h: 2000 };
        let mut ai = client("0x1", "google-chrome-ai", "ИИ", "2", &["app:chrome-ai#1", "ws:surf", "ws:work"]);
        ai.at = (moved.x, moved.y);
        ai.size = (moved.w, moved.h);
        let clients = vec![
            ai.clone(),
            client("0x2", "chromium", "Новости", "1", &["app:chromium#1", "ws:work"]),
            client("0x3", "neovide", "[Scratch]", "special:hidden", &["app:neovide#1", "ws:work"]),
        ];
        // Переход на стол 1: общее окно приходит на место chrome-ai из записи
        // work, а раскладка surf, со стола которого окно уходит, снимается:
        // прямоугольник, где окно оставили, запоминается. Окна, уже стоящие
        // на столе, и скрытые не трогаются.
        let plan = follow_plan(&mut st, &cfg, &clients, 1, mon);
        assert_eq!(plan, vec![Follow { addr: "0x1".into(), rect: Some(PxRect { x: 1000, y: 500, w: 1920, h: 1080 }), now: moved }]);
        assert_eq!(st.geom["surf"]["0x1"], moved);
        // Окно, пришедшее в workspace режима обмена мест, встаёт
        // в запомненный для него прямоугольник, а не на место записи.
        let kept = PxRect { x: 300, y: 200, w: 1500, h: 1200 };
        let mut st2 = State { desktops: st.desktops.clone(), ..State::default() };
        st2.geom.entry("work".into()).or_default().insert("0x1".into(), kept);
        assert_eq!(follow_plan(&mut st2, &cfg, &clients, 1, mon)[0].rect, Some(kept));
        // Обратно на стол 2: окно встаёт в запомненный для surf прямоугольник.
        let mut back = ai.clone();
        back.workspace.name = "1".into();
        let plan = follow_plan(&mut st, &cfg, std::slice::from_ref(&back), 2, mon);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].rect, Some(moved));
        // Стол без активного workspace и стол, где окна нет: ничего не переносится.
        assert!(follow_plan(&mut st, &cfg, &clients, 5, mon).is_empty());
        st.desktops.insert(4, Desktop { workspaces: vec!["chat".into()], active: Some("chat".into()) });
        assert!(follow_plan(&mut st, &cfg, &clients, 4, mon).is_empty());
    }

    #[test]
    fn follow_plan_absorbs_leaving_workspace() {
        // Окно chrome-ai входит в work (режим обмена мест, стол 1, запись
        // о месте в центре экрана) и в surf (стол 2). На столе 1 пользователь
        // сдвинул его в 400,300 1400×1000 и перешёл на стол 2.
        let cfg = shared_cfg();
        let mon = (3840, 2160);
        let mut st = State::default();
        st.desktops.insert(1, Desktop { workspaces: vec!["work".into()], active: Some("work".into()) });
        st.desktops.insert(2, Desktop { workspaces: vec!["surf".into()], active: Some("surf".into()) });
        let record = PxRect { x: 960, y: 540, w: 1920, h: 1080 };
        st.extra.entry("work".into()).or_default().insert("chrome-ai".into(), ExtraApp { rect: record, ..ExtraApp::default() });
        let (cfg, _) = merge_extra(&cfg, &st.extra);
        let r = PxRect { x: 400, y: 300, w: 1400, h: 1000 };
        let mut ai = client("0x1", "google-chrome-ai", "ИИ", "1", &["app:chrome-ai#1", "ws:surf", "ws:work"]);
        ai.at = (r.x, r.y);
        ai.size = (r.w, r.h);
        let plan = follow_plan(&mut st, &cfg, std::slice::from_ref(&ai), 2, mon);
        assert_eq!(plan.len(), 1);
        // work сохранил изменённое место chrome-ai с владельцем и прямоугольник окна.
        assert_eq!(st.moved["work"]["chrome-ai"], Moved { rect: r, owner: Some("0x1".into()) });
        assert_eq!(st.geom["work"]["0x1"], r);
        // Возврат на стол 1: окно встаёт в сдвинутый прямоугольник, а не в запись.
        let mut on_two = ai.clone();
        on_two.workspace.name = "2".into();
        on_two.at = (1910, 10);
        let plan = follow_plan(&mut st, &cfg, std::slice::from_ref(&on_two), 1, mon);
        assert_eq!(plan[0].rect, Some(r));
        // Место chrome-ai в раскладке work от положения на столе 2 не меняется.
        assert_eq!(st.moved["work"]["chrome-ai"].rect, r);
    }

    #[test]
    fn absorb_takes_first_instance_on_its_desktop() {
        let cfg = Config::parse(CFG).unwrap();
        let mon = (3840, 2160);
        let mut st = State::default();
        let at = |mut c: Client, r: PxRect| {
            c.at = (r.x, r.y);
            c.size = (r.w, r.h);
            c
        };
        let center = PxRect { x: 1125, y: 10, w: 1920, h: 2140 };
        let moved = PxRect { x: 100, y: 200, w: 800, h: 600 };
        let other = PxRect { x: 500, y: 500, w: 700, h: 700 };
        let mut full = at(client("0x5", "neovide", "[Scratch]", "1", &["app:neovide#1", "ws:work"]), PxRect { x: 0, y: 0, w: 3840, h: 2160 });
        full.fullscreen = 2;
        let clients = vec![
            // Второй экземпляр chromium стоит раньше в списке композитора,
            // но место даёт первый.
            at(client("0x2", "chromium", "Почта", "1", &["app:chromium#2", "ws:work"]), other),
            at(client("0x1", "chromium", "Новости", "1", &["app:chromium#1", "ws:work"]), moved),
            // herdr описан в work сам и стоит ровно в своей ячейке.
            at(client("0x3", "wezterm-herdr", "herdr · dev-lab", "1", &["app:herdr#1", "ws:work"]), center),
            // Полноэкранное окно место не даёт.
            full,
        ];
        let a = absorb_plan(&cfg, &mut st, "work", 1, &clients, mon);
        let moved_of = |app: &str| a.moved.iter().find(|(x, _)| x == app).map(|(_, m)| m.clone());
        assert_eq!(moved_of("chromium"), Some(Some(Moved { rect: moved, owner: Some("0x1".into()) })));
        // Равный ячейке прямоугольник изменённого места не даёт.
        assert_eq!(moved_of("herdr"), Some(None));
        // Запись без окон на столе (только полноэкранное) не меняется.
        assert_eq!(moved_of("neovide"), None);
        assert_eq!(a.geom.len(), 3);
        assert!(!a.geom.iter().any(|(x, _)| x == "0x5"));
        // Окно на другом столе и на special:pool не учитывается.
        let away = vec![at(client("0x1", "chromium", "Новости", "2", &["app:chromium#1", "ws:work"]), moved), at(client("0x2", "chromium", "Почта", "special:pool", &["app:chromium#2", "ws:work"]), other)];
        assert_eq!(absorb_plan(&cfg, &mut st, "work", 1, &away, mon), Absorb::default());
        // Первый экземпляр на столе 1 — второй, если первый ушёл на другой стол.
        let split = vec![at(client("0x1", "chromium", "Новости", "2", &["app:chromium#1", "ws:work"]), moved), at(client("0x2", "chromium", "Почта", "1", &["app:chromium#2", "ws:work"]), other)];
        let a = absorb_plan(&cfg, &mut st, "work", 1, &split, mon);
        assert_eq!(a.moved, vec![("chromium".to_string(), Some(Moved { rect: other, owner: Some("0x2".into()) }))]);
        // Окно варианта без своей записи даёт место записи семейства.
        let fam = Config::parse(&CFG.replace("main = \"herdr\"\napps = { herdr = \"center\", chromium = \"left\", neovide = \"right\" }", "main = \"wezterm\"\napps = { wezterm = \"center\", chromium = \"left\", neovide = \"right\" }")).unwrap();
        let mut st = State::default();
        let wins = vec![at(client("0x3", "wezterm-herdr", "herdr · dev-lab", "1", &["app:herdr#1", "ws:work"]), moved)];
        let a = absorb_plan(&fam, &mut st, "work", 1, &wins, mon);
        assert_eq!(a.moved, vec![("wezterm".to_string(), Some(Moved { rect: moved, owner: Some("0x3".into()) }))]);
        // absorb_into пишет изменённые места только для workspace, активного на столе.
        st.desktops.insert(1, Desktop { workspaces: vec!["work".into()], active: Some("work".into()) });
        absorb_into(&fam, &mut st, "work", 2, &wins, mon);
        assert!(st.moved.is_empty());
        absorb_into(&fam, &mut st, "work", 1, &wins, mon);
        assert_eq!(st.moved["work"]["wezterm"].rect, moved);
        assert_eq!(st.rect_for(&fam, "work", "herdr", mon), Some(moved));
        // Окно вернули ровно в ячейку: изменённое место снимается.
        let back = vec![at(client("0x3", "wezterm-herdr", "herdr · dev-lab", "1", &["app:herdr#1", "ws:work"]), center)];
        absorb_into(&fam, &mut st, "work", 1, &back, mon);
        assert!(st.moved.is_empty());
    }

    #[test]
    fn app_route_cases() {
        let cfg = shared_cfg();
        let mut d: BTreeMap<u8, Desktop> = BTreeMap::new();
        d.insert(1, Desktop { workspaces: vec!["chat".into(), "work".into()], active: Some("work".into()) });
        d.insert(2, Desktop { workspaces: vec!["surf".into()], active: Some("surf".into()) });
        let a = |v: &str| vec![v.to_string()];
        let s = |v: &str| v.to_string();
        // 1. Приложение описано в активном workspace: цикл, признак pull ничего не меняет.
        assert_eq!(app_route(&cfg, &d, 1, &a("chromium"), false, &[]), AppRoute::Cycle { ws: s("work"), app: s("chromium") });
        assert_eq!(app_route(&cfg, &d, 1, &a("chromium"), true, &[]), AppRoute::Cycle { ws: s("work"), app: s("chromium") });
        // 2. Перетаскивание в активный workspace.
        assert_eq!(app_route(&cfg, &d, 1, &a("chrome-ai"), true, &[]), AppRoute::Pull { ws: s("work"), app: s("chrome-ai") });
        // 3. Приложение в запущенном workspace: в списке текущего стола, затем другого.
        assert_eq!(app_route(&cfg, &d, 1, &a("calc"), false, &[]), AppRoute::Raise { ws: s("chat"), app: s("calc"), desktop: None });
        assert_eq!(app_route(&cfg, &d, 1, &a("chrome-ai"), false, &[]), AppRoute::Raise { ws: s("surf"), app: s("chrome-ai"), desktop: Some(2) });
        // 4. Workspace приложения не запущен: открыть в активном workspace стола,
        // а не поднимать чужой workspace (прежняя ветвь «3б» снята).
        d.remove(&2);
        assert_eq!(app_route(&cfg, &d, 1, &a("chrome-ai"), false, &[]), AppRoute::Open { ws: s("work"), app: s("chrome-ai") });
        // 5. На столе нет активного workspace: цикл вне workspace.
        d.insert(4, Desktop::default());
        assert_eq!(app_route(&cfg, &d, 4, &a("chrome-ai"), false, &[]), AppRoute::Free { app: s("chrome-ai") });
        assert_eq!(app_route(&cfg, &d, 4, &a("chrome-ai"), true, &[]), AppRoute::Free { app: s("chrome-ai") });
    }

    /// Конфиг сессии в миниатюре (изменение workspace-overrides, решение
    /// D10): у браузеров собственные клавиши Shift+Super+буква, `surf`
    /// переопределяет их на Super+буква; `ai` описывает `chrome-ai` без
    /// переопределения. Семейство `chrome` с вариантом `chrome-ai` — в
    /// `family_cfg`.
    fn overrides_cfg() -> Config {
        Config::parse(&overrides_text()).unwrap()
    }

    fn overrides_text() -> String {
        CFG.replace("[apps.calc]", "[apps.chrome]\ncmd = \"chrome\"\nchain = \"SUPER+SHIFT+B\"\n\n[apps.chrome-ai]\ncmd = \"chrome\"\nchain = \"SUPER+SHIFT+V\"\n\n[apps.yandex]\ncmd = \"yandex\"\nchain = \"SUPER+SHIFT+Y\"\n\n[apps.calc]")
            .replace("[apps.herdr]\n", "[apps.herdr]\nchain = \"SUPER+T\"\n")
            + "\n[workspaces.surf]\ntemplate = \"thirds\"\nmode = \"stack\"\n[workspaces.surf.apps]\nchrome = { cell = \"left\", chain = \"SUPER+B\" }\nyandex = { cell = \"center\", chain = \"SUPER+Y\" }\nchrome-ai = { cell = \"right\", chain = \"SUPER+V\" }\n\n[workspaces.ai]\ntemplate = \"thirds\"\napps = { chrome-ai = \"center\" }\n"
    }

    fn family_cfg() -> Config {
        let text = CFG.replace("[apps.calc]", "[apps.chrome]\ncmd = \"chrome\"\nclass = \"^google-chrome$\"\nchain = \"SUPER+B\"\n\n[apps.chrome-ai]\nfamily = \"chrome\"\ncmd = \"chrome\"\nclass = \"^google-chrome-ai$\"\nchain = \"SUPER+V\"\n\n[apps.calc]")
            + "\n[workspaces.mix]\ntemplate = \"thirds\"\napps = { chrome = \"center\" }\n\n[workspaces.pair]\ntemplate = \"thirds\"\napps = { chrome = \"left\", chrome-ai = { cell = \"right\", mode = \"stack\" } }\n";
        Config::parse(&text).unwrap()
    }

    fn desk(ws: &[&str], active: Option<&str>) -> Desktop {
        Desktop { workspaces: ws.iter().map(|w| w.to_string()).collect(), active: active.map(String::from) }
    }

    #[test]
    fn key_route_cases() {
        let cfg = overrides_cfg();
        let s = |v: &str| v.to_string();
        let mut d: BTreeMap<u8, Desktop> = BTreeMap::new();
        d.insert(1, desk(&["work"], Some("work")));
        d.insert(2, desk(&["surf"], Some("surf")));
        // Шаг 1: в surf обе клавиши ведут цикл chrome-ai.
        assert_eq!(key_route(&cfg, &d, 2, "SUPER+V", &[]), Some(AppRoute::Cycle { ws: s("surf"), app: s("chrome-ai") }));
        assert_eq!(key_route(&cfg, &d, 2, "super+shift+v", &[]), Some(AppRoute::Cycle { ws: s("surf"), app: s("chrome-ai") }));
        // Шаг 1 в work: собственная клавиша приложения work.
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+T", &[s("chrome-ai")]), Some(AppRoute::Cycle { ws: s("work"), app: s("herdr") }));
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+T", &[]), Some(AppRoute::Cycle { ws: s("work"), app: s("herdr") }));
        // Шаг 2: клавиша surf поднимает surf на его столе, пока окна chrome-ai
        // в work нет.
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+V", &[]), Some(AppRoute::Raise { ws: s("surf"), app: s("chrome-ai"), desktop: Some(2) }));
        // Шаг 3, случай «в»: собственная клавиша перетаскивает окна в work.
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+SHIFT+V", &[]), Some(AppRoute::Pull { ws: s("work"), app: s("chrome-ai") }));
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+SHIFT+Y", &[]), Some(AppRoute::Pull { ws: s("work"), app: s("yandex") }));
        // Смешанный случай: ai описывает chrome-ai без переопределения — случай «б».
        d.insert(3, desk(&["ai"], Some("ai")));
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+SHIFT+V", &[]), Some(AppRoute::Raise { ws: s("ai"), app: s("chrome-ai"), desktop: Some(3) }));
        d.remove(&3);
        // Без активного workspace: обе клавиши поднимают surf.
        d.insert(4, Desktop::default());
        assert_eq!(key_route(&cfg, &d, 4, "SUPER+V", &[]), Some(AppRoute::Raise { ws: s("surf"), app: s("chrome-ai"), desktop: Some(2) }));
        assert_eq!(key_route(&cfg, &d, 4, "SUPER+SHIFT+V", &[]), Some(AppRoute::Raise { ws: s("surf"), app: s("chrome-ai"), desktop: Some(2) }));
        // Шаг 4: surf не запущен — обе клавиши открывают chrome-ai в work,
        // а на столе без workspace — цикл вне workspace.
        d.remove(&2);
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+V", &[]), Some(AppRoute::Open { ws: s("work"), app: s("chrome-ai") }));
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+SHIFT+V", &[]), Some(AppRoute::Open { ws: s("work"), app: s("chrome-ai") }));
        assert_eq!(key_route(&cfg, &d, 4, "SUPER+V", &[]), Some(AppRoute::Free { app: s("chrome-ai") }));
        // На цепочку никто не отзывается.
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+F12", &[]), None);

        // Семейство (решение D6): mix описывает только семейство, pair — оба.
        let cfg = family_cfg();
        let mut d: BTreeMap<u8, Desktop> = BTreeMap::new();
        d.insert(1, desk(&["mix"], Some("mix")));
        d.insert(2, desk(&["pair"], Some("pair")));
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+B", &[]), Some(AppRoute::Cycle { ws: s("mix"), app: s("chrome") }));
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+V", &[]), Some(AppRoute::Cycle { ws: s("mix"), app: s("chrome-ai") }));
        assert_eq!(key_route(&cfg, &d, 2, "SUPER+B", &[]), Some(AppRoute::Cycle { ws: s("pair"), app: s("chrome") }));
        assert_eq!(key_route(&cfg, &d, 2, "SUPER+V", &[]), Some(AppRoute::Cycle { ws: s("pair"), app: s("chrome-ai") }));
        // Вариант, описанный в запущенном mix только через семейство, клавишу
        // не переопределяет: из work mix поднимается (решение D13).
        d.insert(3, desk(&["work"], Some("work")));
        d.remove(&2);
        assert_eq!(key_route(&cfg, &d, 3, "SUPER+V", &[]), Some(AppRoute::Raise { ws: s("mix"), app: s("chrome-ai"), desktop: Some(1) }));
    }

    #[test]
    fn key_route_follows_window_in_active_ws() {
        // Решение D16: клавиша ведёт к окну там, где оно сейчас есть.
        let cfg = overrides_cfg();
        let s = |v: &str| v.to_string();
        let mut d: BTreeMap<u8, Desktop> = BTreeMap::new();
        d.insert(1, desk(&["work"], Some("work")));
        d.insert(2, desk(&["surf"], Some("surf")));
        // Общее окно chrome-ai входит в work тегом: клавиша переопределения
        // surf ведёт цикл в work, стол не меняется.
        let ai = [s("chrome-ai")];
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+V", &ai), Some(AppRoute::Cycle { ws: s("work"), app: s("chrome-ai") }));
        // Собственная клавиша — так же, без перетаскивания.
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+SHIFT+V", &ai), Some(AppRoute::Cycle { ws: s("work"), app: s("chrome-ai") }));
        // Окна в work нет — прежние шаги: поднять surf, перетащить собственной
        // клавишей.
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+V", &[s("herdr")]), Some(AppRoute::Raise { ws: s("surf"), app: s("chrome-ai"), desktop: Some(2) }));
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+SHIFT+V", &[]), Some(AppRoute::Pull { ws: s("work"), app: s("chrome-ai") }));
        // Окно другого приложения клавишу не перехватывает.
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+B", &ai), Some(AppRoute::Raise { ws: s("surf"), app: s("chrome"), desktop: Some(2) }));

        // Два кандидата: surf назначает Super+V приложению chrome-ai, web —
        // приложению chrome. В both описаны оба: действует запись, стоящая
        // раньше; описанное приложение идёт раньше входящего только тегом.
        let text = format!(
            "{}\n[workspaces.web]\ntemplate = \"thirds\"\napps = {{ chrome = {{ cell = \"left\", chain = \"SUPER+V\" }} }}\n\n[workspaces.both]\ntemplate = \"thirds\"\n[workspaces.both.apps]\nyandex = \"left\"\nchrome-ai = \"center\"\nchrome = \"right\"\n",
            overrides_text()
        );
        let cfg = Config::parse(&text).unwrap();
        assert_eq!(cfg.key_candidates("SUPER+V"), vec![s("chrome"), s("chrome-ai")]);
        d.insert(3, desk(&["both"], Some("both")));
        assert_eq!(key_route(&cfg, &d, 3, "SUPER+V", &[]), Some(AppRoute::Cycle { ws: s("both"), app: s("chrome-ai") }));
        // В work оба входят только тегом: первый по порядку кандидатов.
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+V", &[s("chrome-ai"), s("chrome")]), Some(AppRoute::Cycle { ws: s("work"), app: s("chrome") }));
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+V", &ai), Some(AppRoute::Cycle { ws: s("work"), app: s("chrome-ai") }));
        // Ни одного окна в work: поднимается первый откликнувшийся запущенный
        // workspace, как раньше.
        assert_eq!(key_route(&cfg, &d, 1, "SUPER+V", &[]), Some(AppRoute::Raise { ws: s("surf"), app: s("chrome-ai"), desktop: Some(2) }));
    }

    #[test]
    fn family_key_cycles_variant_windows() {
        // Задача 3.6: в mix клавиша семейства перебирает окна обоих профилей,
        // клавиша варианта — только его окна; в pair у каждого свои окна.
        let cfg = family_cfg();
        let clients = vec![
            crate::hypr::test_client("0x1", "google-chrome", "A", "1", &["app:chrome#1", "ws:mix"]),
            crate::hypr::test_client("0x2", "google-chrome-ai", "B", "1", &["app:chrome-ai#1", "ws:mix"]),
            crate::hypr::test_client("0x3", "google-chrome", "C", "1", &["app:chrome#2", "ws:mix"]),
        ];
        let addrs = |v: Vec<&Client>| v.into_iter().map(|c| c.address.clone()).collect::<Vec<_>>();
        let mix: Vec<String> = cfg.workspaces["mix"].apps.keys().cloned().collect();
        assert_eq!(addrs(placed_windows(&cfg, &clients, "mix", &mix, "chrome")), vec!["0x1", "0x2", "0x3"]);
        assert_eq!(addrs(placed_windows(&cfg, &clients, "mix", &mix, "chrome-ai")), vec!["0x2"]);
        let pair_clients: Vec<Client> = clients
            .iter()
            .map(|c| {
                let mut c = c.clone();
                c.tags = c.tags.iter().map(|t| t.replace("ws:mix", "ws:pair")).collect();
                c
            })
            .collect();
        let pair: Vec<String> = cfg.workspaces["pair"].apps.keys().cloned().collect();
        assert_eq!(addrs(placed_windows(&cfg, &pair_clients, "pair", &pair, "chrome")), vec!["0x1", "0x3"]);
        assert_eq!(addrs(placed_windows(&cfg, &pair_clients, "pair", &pair, "chrome-ai")), vec!["0x2"]);
        // Режим: в mix вариант берёт запись семейства (обмен мест переносит
        // стопку семейства), в pair у chrome-ai свой mode = "stack".
        assert_eq!(cycle_mode(&cfg, "mix", "chrome-ai"), (Mode::Swap, "chrome".to_string()));
        assert_eq!(cycle_mode(&cfg, "pair", "chrome-ai"), (Mode::Stack, "chrome-ai".to_string()));
        assert_eq!(cycle_mode(&cfg, "pair", "chrome"), (Mode::Swap, "chrome".to_string()));
    }

    #[test]
    fn app_route_follows_own_key() {
        // Команда `app` — собственная клавиша приложения (решение D5).
        let cfg = overrides_cfg();
        let s = |v: &str| v.to_string();
        let a = |v: &str| vec![v.to_string()];
        let mut d: BTreeMap<u8, Desktop> = BTreeMap::new();
        d.insert(1, desk(&["work"], Some("work")));
        d.insert(2, desk(&["surf"], Some("surf")));
        // Случай «в» и без --pull: surf переопределил клавишу chrome-ai.
        assert_eq!(app_route(&cfg, &d, 1, &a("chrome-ai"), false, &[]), AppRoute::Pull { ws: s("work"), app: s("chrome-ai") });
        assert_eq!(app_route(&cfg, &d, 2, &a("chrome-ai"), false, &[]), AppRoute::Cycle { ws: s("surf"), app: s("chrome-ai") });
        // Окно chrome-ai входит в work тегом: цикл там же (решение D16).
        assert_eq!(app_route(&cfg, &d, 1, &a("chrome-ai"), false, &a("chrome-ai")), AppRoute::Cycle { ws: s("work"), app: s("chrome-ai") });
        assert_eq!(app_route(&cfg, &d, 1, &a("chrome-ai"), true, &a("chrome-ai")), AppRoute::Pull { ws: s("work"), app: s("chrome-ai") });
        // Workspace без переопределения поднимается.
        d.insert(3, desk(&["ai"], Some("ai")));
        assert_eq!(app_route(&cfg, &d, 1, &a("chrome-ai"), false, &[]), AppRoute::Raise { ws: s("ai"), app: s("chrome-ai"), desktop: Some(3) });
        // Кандидаты: действует тот, чей путь найден на более раннем шаге.
        assert_eq!(app_route(&cfg, &d, 1, &[s("yandex"), s("herdr")], false, &[]), AppRoute::Cycle { ws: s("work"), app: s("herdr") });
        assert_eq!(app_route(&cfg, &d, 1, &[s("yandex"), s("chrome-ai")], false, &[]), AppRoute::Raise { ws: s("ai"), app: s("chrome-ai"), desktop: Some(3) });
    }

    #[test]
    fn cycle_mode_of_app() {
        // Приложение без ячейки берёт режим workspace (изменение live-layout,
        // решение D6: исключение для записи с rect снято).
        let text = format!("{CFG}\n[workspaces.dev]\ntemplate = \"thirds\"\nmain = \"herdr\"\n[workspaces.dev.apps]\nherdr = \"center\"\nchromium = {{ cell = \"left\", mode = \"stack\" }}\nneovide = \"right\"\ncalc = {{ rect = {{ x = 2600, y = 1500, w = 600, h = 400 }} }}\n");
        let cfg = Config::parse(&text).unwrap();
        assert_eq!(cycle_mode(&cfg, "dev", "calc"), (Mode::Swap, "calc".to_string()));
        // Запись с rect и mode = "stack" выбирается подъёмом.
        let stack = Config::parse(&text.replace("calc = { rect = { x = 2600, y = 1500, w = 600, h = 400 } }", "calc = { rect = { x = 2600, y = 1500, w = 600, h = 400 }, mode = \"stack\" }")).unwrap();
        assert_eq!(cycle_mode(&stack, "dev", "calc"), (Mode::Stack, "calc".to_string()));
        // Режим stack у приложения с ячейкой: обмена нет.
        assert_eq!(cycle_mode(&cfg, "dev", "chromium"), (Mode::Stack, "chromium".to_string()));
        assert_eq!(cycle_mode(&cfg, "dev", "neovide"), (Mode::Swap, "neovide".to_string()));
        // Вариант без своей записи: режим и обмен — по записи семейства.
        assert_eq!(cycle_mode(&cfg, "dev", "chromium-mail"), (Mode::Stack, "chromium".to_string()));
        assert_eq!(cycle_mode(&cfg, "work", "herdr"), (Mode::Swap, "herdr".to_string()));
        // Явный mode = "swap" у записи с rect: обмен с главным местом, прежнее
        // главное встаёт в прямоугольник калькулятора.
        let swap = text.replace("calc = { rect = { x = 2600, y = 1500, w = 600, h = 400 } }", "calc = { rect = { x = 2600, y = 1500, w = 600, h = 400 }, mode = \"swap\" }");
        let cfg = Config::parse(&swap).unwrap();
        assert_eq!(cycle_mode(&cfg, "dev", "calc"), (Mode::Swap, "calc".to_string()));
        let mut st = State::default();
        let mon = (3840, 2160);
        let main = st.main_app(&cfg, "dev", mon);
        assert_eq!(main.as_deref(), Some("herdr"));
        swap_places(&mut st, "dev", main.as_deref(), "calc", "center", &|_| true);
        assert_eq!(st.main_app(&cfg, "dev", mon).as_deref(), Some("calc"));
        assert_eq!(st.rect_for(&cfg, "dev", "herdr", mon), Some(PxRect { x: 2600, y: 1500, w: 600, h: 400 }));
        assert_eq!(st.rect_for(&cfg, "dev", "calc", mon), Some(PxRect { x: 1125, y: 10, w: 1920, h: 2140 }));
    }

    #[test]
    fn pull_record_kind() {
        let cfg = shared_cfg();
        let r = PxRect { x: 960, y: 540, w: 1920, h: 1080 };
        let mut extra: BTreeMap<String, BTreeMap<String, ExtraApp>> = BTreeMap::new();
        // Приложение файла конфига: запись только о месте.
        assert_eq!(share_record(&cfg, &extra, "chrome-ai", r), Some(ExtraApp { rect: r, ..ExtraApp::default() }));
        // Приложение, известное только записи сессии другого workspace: полная
        // запись под тем же именем.
        let full = ExtraApp { class: Some("Alacritty".into()), cmd: vec!["/usr/bin/alacritty".into()], cwd: Some("/home/mne".into()), rect: PxRect::default() };
        extra.entry("work".into()).or_default().insert("alacritty".into(), full.clone());
        assert_eq!(share_record(&cfg, &extra, "alacritty", r), Some(ExtraApp { rect: r, ..full }));
        // Описать нечем.
        assert_eq!(share_record(&cfg, &extra, "nothing", r), None);
    }

    #[test]
    fn successor_keeps_shared_window_with_user() {
        // В списке стола 1 surf и work, work перенесён на стол 5, композитор
        // там же; преемник surf поднимается на столе 1 без перехода.
        let after = desks(&[(1, "surf"), (5, "work")]);
        let shared = client("0x1", "google-chrome-ai", "ИИ", "5", &["app:chrome-ai#1", "ws:surf", "ws:work"]);
        // Общее окно остаётся на столе 5 с пользователем.
        assert_eq!(window_home(&shared.workspaces(), &after, 5, Spot::of(&shared)), Home::Desktop(5));
        // Окно только из surf возвращается на стол 1.
        let own = client("0x2", "google-chrome", "Новости", "special:pool", &["app:chrome#1", "ws:surf"]);
        assert_eq!(window_home(&own.workspaces(), &after, 5, Spot::of(&own)), Home::Desktop(1));
        // Пользователь перешёл на стол 1: общее окно приходит туда.
        assert_eq!(window_home(&shared.workspaces(), &after, 1, Spot::of(&shared)), Home::Desktop(1));
    }

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
            client("0x1", "chromium", "Новости", "1", &["app:chromium#1", "ws:work"]),
            client("0x2", "chromium", "Gmail — Входящие", "1", &["app:chromium-mail#1", "ws:work"]),
            // Окно семейства, не входящее в workspace, им не расставляется.
            client("0x3", "chromium", "Документы", "1", &["app:chromium#2", "ws:dev-front"]),
        ];
        let addrs = |apps: &[String], app: &str| placed_windows(&cfg, &clients, "work", apps, app).iter().map(|c| c.address.clone()).collect::<Vec<_>>();
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
        let plan = triples(adopt_plan(&cfg, &clients, &apps, false).unwrap());
        assert_eq!(plan, vec![(0, "chromium".to_string(), 1), (2, "chromium".to_string(), 2), (1, "chromium".to_string(), 3)]);

        // Захват работает и у приложения, окна которого уже есть.
        let with_window = vec![client("0x1", "chromium", "Новости", "1", &["app:chromium#1"]), client("0x2", "chromium", "Документы", "1", &[])];
        let plan = triples(adopt_plan(&cfg, &with_window, &apps, false).unwrap());
        assert_eq!(plan, vec![(1, "chromium".to_string(), 2)]);

        // Окно с тегом приложения, которого нет в конфиге, переходит к приложению.
        let stale = vec![client("0x1", "neovide", "[Scratch]", "1", &["app:editor-dots#1"])];
        assert_eq!(triples(adopt_plan(&cfg, &stale, &apps, false).unwrap()), vec![(0, "neovide".to_string(), 1)]);
    }

    #[test]
    fn adopt_plan_prefers_variant_over_family() {
        let cfg = Config::parse(CFG).unwrap();
        let clients = vec![client("0x1", "chromium", "Gmail — Входящие", "1", &[]), client("0x2", "chromium", "Новости", "1", &[])];
        // Workspace описывает и семейство, и вариант.
        let apps: Vec<String> = ["chromium".to_string(), "chromium-mail".to_string()].into();
        let plan = triples(adopt_plan(&cfg, &clients, &apps, false).unwrap());
        assert_eq!(plan, vec![(0, "chromium-mail".to_string(), 1), (1, "chromium".to_string(), 1)]);
        // Workspace описывает только семейство: окно варианта всё равно достаётся
        // варианту и попадает в ячейку семейства как окно своего семейства.
        let plan = triples(adopt_plan(&cfg, &clients, &["chromium".to_string()], false).unwrap());
        assert_eq!(plan, vec![(0, "chromium-mail".to_string(), 1), (1, "chromium".to_string(), 1)]);
        // Окно, подходящее двум вариантам одного семейства, достаётся первому по имени.
        let two = format!("{CFG}\n[apps.chromium-docs]\nfamily = \"chromium\"\ncmd = \"chromium\"\nclass = \"(?i)^chromium(-browser)?$\"\ntitle = \"Gmail\"\n");
        let cfg = Config::parse(&two).unwrap();
        let plan = triples(adopt_plan(&cfg, &clients[..1], &["chromium".to_string()], false).unwrap());
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
        swap_places(&mut st, "work", Some("herdr"), "chromium", "center", &|_| true);
        assert_eq!(open_rect(&mut st, &cfg, Some("work"), "chromium", mon), Some(PxRect { x: 1125, y: 10, w: 1920, h: 2140 }));
        // Изменённое место приложения побеждает исходное (изменение live-layout).
        let moved = PxRect { x: 100, y: 200, w: 800, h: 600 };
        st.moved.entry("work".into()).or_default().insert("chromium".into(), Moved { rect: moved, owner: None });
        assert_eq!(open_rect(&mut st, &cfg, Some("work"), "chromium", mon), Some(moved));
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
        assert_eq!(eff.workspaces["work"].apps["galculator"].place, Placement::Rect { rect: rect.to_rect() });
        // Запись только о месте описание приложения конфига не подменяет,
        // а лишь добавляет его в этот workspace.
        assert_eq!(eff.apps["chromium-mail"].title.as_deref(), Some("^Gmail"));
        assert_eq!(eff.workspaces["work"].apps["chromium-mail"].place, Placement::Rect { rect: rect.to_rect() });
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
        assert_eq!(eff.workspaces["work"].apps["herdr"].place, Placement::Cell("center".into()));
    }

    #[test]
    fn keep_layout_resets_changed_workspace() {
        let cfg = Config::parse(CFG).unwrap();
        let changed = Config::parse(&CFG.replace("neovide = \"right\"", "neovide = { rect = { x = 0, y = 0, w = 10, h = 10 } }")).unwrap();
        let added = Config::parse(&format!("{CFG}\n[workspaces.surf]\ntemplate = \"thirds\"\napps = {{ neovide = \"right\" }}\n")).unwrap();
        let r = PxRect { x: 1, y: 2, w: 3, h: 4 };
        let state = || {
            let mut st = State::default();
            st.cells.insert("work".into(), BTreeMap::from([("chromium".to_string(), Place::Cell("center".into()))]));
            st.moved.entry("work".into()).or_default().insert("neovide".into(), Moved { rect: r, owner: None });
            st.main.insert("work".into(), "chromium".into());
            st.geom.entry("work".into()).or_default().insert("0x1".into(), r);
            st
        };
        // Раздел workspace изменился: раскладка и память окон снимаются
        // и соберутся из файла; workspace возвращается для применения.
        let mut st = state();
        assert_eq!(keep_layout(&cfg, &changed, &mut st), vec!["work".to_string()]);
        assert!(st.cells.is_empty() && st.moved.is_empty() && st.main.is_empty() && st.geom.is_empty());
        // Правка соседнего workspace раскладку work не сбрасывает; новый
        // workspace возвращается, раскладки у него ещё нет.
        let mut st = state();
        assert_eq!(keep_layout(&cfg, &added, &mut st), vec!["surf".to_string()]);
        assert_eq!(st.cells["work"]["chromium"], Place::Cell("center".into()));
        assert_eq!(st.moved["work"]["neovide"].rect, r);
        assert_eq!(st.main["work"], "chromium");
        // Workspace из конфига исчез: раскладка снята, применять нечего.
        let mut st = state();
        assert!(keep_layout(&cfg, &Config::default(), &mut st).is_empty());
        assert!(st.cells.is_empty() && st.moved.is_empty());
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

    /// Окна `work`: herdr, chromium, neovide и одно свободное.
    fn work_clients() -> Vec<Client> {
        vec![
            client("0x1", "wezterm-herdr", "herdr · dev-lab", "1", &["app:herdr#1", "ws:work"]),
            client("0x2", "chromium", "Новости", "1", &["app:chromium#1", "ws:work"]),
            client("0x3", "neovide", "[Scratch]", "1", &["app:neovide#1", "ws:work"]),
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
        let clients = work_clients();
        // Цикл идёт по единственному окну chromium.
        let order = vec!["0x2".to_string()];
        let anchor = |main: Option<&str>, active: Option<&str>, focus: Option<&str>| anchor_window(&cfg, &clients, "work", main, &order, active, focus).unwrap_or_default();
        // Режим обмена мест: возврат к окну прежнего главного приложения,
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
    fn swap_places_exchanges_stacks() {
        let cfg = Config::parse(CFG).unwrap();
        let mon = (3840, 2160);
        let mut st = State::default();
        let main = st.main_app(&cfg, "work", mon);
        swap_places(&mut st, "work", main.as_deref(), "chromium", "center", &|_| true);
        // Стопки меняются местами целиком, третье приложение не двигается.
        assert_eq!(st.rect_for(&cfg, "work", "chromium", mon), Some(PxRect { x: 1125, y: 10, w: 1920, h: 2140 }));
        assert_eq!(st.rect_for(&cfg, "work", "herdr", mon), Some(PxRect { x: -805, y: 10, w: 1920, h: 2140 }));
        assert_eq!(st.rect_for(&cfg, "work", "neovide", mon), Some(PxRect { x: 3055, y: 10, w: 1920, h: 2140 }));
        assert_eq!(st.main_app(&cfg, "work", mon).as_deref(), Some("chromium"));
        // Приложение вне раскладки: прежнее главное остаётся без места.
        let mut st = State::default();
        st.cells_of(&cfg, "work", mon);
        swap_places(&mut st, "work", Some("herdr"), "wezterm", "center", &|_| true);
        assert_eq!(st.cells_of(&cfg, "work", mon).get("herdr"), None);
        assert_eq!(st.cells_of(&cfg, "work", mon).get("wezterm"), Some(&Place::Cell("center".into())));
        // Без главного приложения вызванное получает ячейку main шаблона.
        let mut st = State::default();
        st.cells_of(&cfg, "work", mon).remove("herdr");
        swap_places(&mut st, "work", None, "neovide", "center", &|_| true);
        assert_eq!(st.rect_for(&cfg, "work", "neovide", mon), Some(PxRect { x: 1125, y: 10, w: 1920, h: 2140 }));
        // У семейства без cmd запускать нечего: место остаётся пустым, демон пишет в журнал.
        assert!(cfg.app_command(None, &cfg.apps["wezterm"]).is_none());
    }

    /// Пример пользователя: chrome-ai перетащен в work записью сессии
    /// с прямоугольником в центре экрана.
    fn user_example() -> (Config, State) {
        let mut st = State::default();
        let record = PxRect { x: 960, y: 540, w: 1920, h: 1080 };
        st.extra.entry("work".into()).or_default().insert("chrome-ai".into(), ExtraApp { rect: record, ..ExtraApp::default() });
        let (cfg, _) = merge_extra(&shared_cfg(), &st.extra);
        (cfg, st)
    }

    #[test]
    fn swap_places_exchanges_moved_rects() {
        let (cfg, mut st) = user_example();
        let mon = (3840, 2160);
        let center = PxRect { x: 1125, y: 10, w: 1920, h: 2140 };
        let r = PxRect { x: 400, y: 300, w: 1400, h: 1000 };
        st.cells_of(&cfg, "work", mon);
        st.moved.entry("work".into()).or_default().insert("chrome-ai".into(), Moved { rect: r, owner: Some("0xai".into()) });
        assert_eq!(st.rect_for(&cfg, "work", "chrome-ai", mon), Some(r));
        let main = st.main_app(&cfg, "work", mon);
        assert_eq!(main.as_deref(), Some("herdr"));
        swap_places(&mut st, "work", main.as_deref(), "chrome-ai", "center", &|_| true);
        // chrome-ai главное и в center, herdr — в сдвинутом прямоугольнике.
        assert_eq!(st.main_app(&cfg, "work", mon).as_deref(), Some("chrome-ai"));
        assert_eq!(st.rect_for(&cfg, "work", "chrome-ai", mon), Some(center));
        assert_eq!(st.rect_for(&cfg, "work", "herdr", mon), Some(r));
        assert_eq!(st.cells["work"]["chrome-ai"], Place::Cell("center".into()));
        assert_eq!(st.cells["work"]["herdr"], Place::Rect { rect: PxRect { x: 960, y: 540, w: 1920, h: 1080 } });
        // Обратный обмен возвращает всё как было.
        swap_places(&mut st, "work", Some("chrome-ai"), "herdr", "center", &|_| true);
        assert_eq!(st.main_app(&cfg, "work", mon).as_deref(), Some("herdr"));
        assert_eq!(st.rect_for(&cfg, "work", "herdr", mon), Some(center));
        assert_eq!(st.rect_for(&cfg, "work", "chrome-ai", mon), Some(r));
        // Главное тоже растянуто: его прямоугольник достаётся вызванному.
        let wide = PxRect { x: 1125, y: 10, w: 2400, h: 2140 };
        st.moved.get_mut("work").unwrap().insert("herdr".into(), Moved { rect: wide, owner: Some("0xh".into()) });
        swap_places(&mut st, "work", Some("herdr"), "chrome-ai", "center", &|_| true);
        assert_eq!(st.rect_for(&cfg, "work", "chrome-ai", mon), Some(wide));
        assert_eq!(st.rect_for(&cfg, "work", "herdr", mon), Some(r));
    }

    #[test]
    fn swap_places_drops_moved_of_app_without_windows() {
        let cfg = Config::parse(CFG).unwrap();
        let mon = (3840, 2160);
        let mut st = State::default();
        st.cells_of(&cfg, "work", mon);
        let wide = PxRect { x: 1125, y: 10, w: 2400, h: 2140 };
        st.moved.entry("work".into()).or_default().insert("herdr".into(), Moved { rect: wide, owner: Some("0xh".into()) });
        // У neovide окон нет (запуск по клавише): растянутый прямоугольник
        // herdr ему не достаётся, окно появится в ячейке center.
        swap_places(&mut st, "work", Some("herdr"), "neovide", "center", &|a| a != "neovide");
        assert_eq!(st.rect_for(&cfg, "work", "neovide", mon), Some(PxRect { x: 1125, y: 10, w: 1920, h: 2140 }));
        assert_eq!(st.rect_for(&cfg, "work", "herdr", mon), Some(PxRect { x: 3055, y: 10, w: 1920, h: 2140 }));
        assert!(st.moved.is_empty());
    }

    #[test]
    fn swap_press_on_non_main_app_selects_active() {
        let a = |s: &str| s.to_string();
        let one = vec![a("0xai")];
        // Окно неглавного приложения активно (его только что двигали мышью):
        // выбирается оно, с обменом мест, а не конец цикла.
        assert_eq!(swap_start(&one, Some("0xai"), false), Some(a("0xai")));
        // Активно чужое окно — первый экземпляр.
        assert_eq!(swap_start(&one, Some("0xh"), false), Some(a("0xai")));
        // Из нескольких экземпляров выбирается активный.
        let three = vec![a("0x1"), a("0x2"), a("0x3")];
        assert_eq!(swap_start(&three, Some("0x2"), false), Some(a("0x2")));
        // Приложение уже главное: прежний шаг цикла, за последним — конец цикла.
        assert_eq!(swap_start(&one, Some("0xai"), true), None);
        assert_eq!(cycle_step(&one, Some("0xai"), false, Some("0xh"), None), CycleStep::Back(a("0xh")));
        // Три экземпляра главного приложения и клик по второму — третий.
        assert_eq!(swap_start(&three, Some("0x2"), true), None);
        assert_eq!(cycle_step(&three, Some("0x2"), false, Some("0xh"), None), CycleStep::Select(a("0x3")));
        // Окон нет — решает запуск.
        assert_eq!(swap_start(&[], Some("0xh"), false), None);
    }

    #[test]
    fn closing_last_window_drops_moved_place() {
        let cfg = Config::parse(CFG).unwrap();
        let mon = (3840, 2160);
        let r = PxRect { x: 2600, y: 300, w: 1800, h: 1500 };
        let mut st = State::default();
        st.moved.entry("work".into()).or_default().insert("neovide".into(), Moved { rect: r, owner: Some("0x3".into()) });
        // Закрыто единственное окно neovide: место возвращается к ячейке right.
        assert_eq!(drop_moved(&mut st, &cfg, "0x3", &[]), vec![("work".to_string(), "neovide".to_string())]);
        assert_eq!(st.rect_for(&cfg, "work", "neovide", mon), Some(PxRect { x: 3055, y: 10, w: 1920, h: 2140 }));
        // У chromium второе окно: место остаётся и меняет владельца.
        let moved = PxRect { x: 100, y: 200, w: 800, h: 600 };
        st.moved.entry("work".into()).or_default().insert("chromium".into(), Moved { rect: moved, owner: Some("0x1".into()) });
        let rest = vec![client("0x2", "chromium", "Почта", "1", &["app:chromium#2", "ws:work"])];
        assert!(drop_moved(&mut st, &cfg, "0x1", &rest).is_empty());
        assert_eq!(st.moved["work"]["chromium"], Moved { rect: moved, owner: Some("0x2".into()) });
        // Место из снимка без владельца переживает закрытие окна другого приложения.
        st.moved.get_mut("work").unwrap().insert("herdr".into(), Moved { rect: r, owner: None });
        assert!(drop_moved(&mut st, &cfg, "0x9", &rest).is_empty());
        assert_eq!(st.moved["work"]["herdr"].rect, r);
        // Окно, ушедшее из work в другой workspace, место в work отдаёт.
        let left = vec![client("0x2", "chromium", "Почта", "1", &["app:chromium#2", "ws:surf"])];
        assert_eq!(drop_moved(&mut st, &cfg, "0x2", &left), vec![("work".to_string(), "chromium".to_string())]);
        // Окно, вставшее на место без владельца, становится владельцем.
        claim_owner(&mut st, &cfg, "work", "herdr", "0x7");
        assert_eq!(st.moved["work"]["herdr"].owner.as_deref(), Some("0x7"));
    }

    #[test]
    fn arrange_resets_moved_keeps_swap() {
        let (cfg, mut st) = user_example();
        let mon = (3840, 2160);
        let record = PxRect { x: 960, y: 540, w: 1920, h: 1080 };
        let center = PxRect { x: 1125, y: 10, w: 1920, h: 2140 };
        let r = PxRect { x: 400, y: 300, w: 1400, h: 1000 };
        st.cells_of(&cfg, "work", mon);
        st.moved.entry("work".into()).or_default().insert("chrome-ai".into(), Moved { rect: r, owner: Some("0xai".into()) });
        swap_places(&mut st, "work", Some("herdr"), "chrome-ai", "center", &|_| true);
        // Окно neovide растянуто, у chromium окно ушло на special:pool.
        st.moved.get_mut("work").unwrap().insert("neovide".into(), Moved { rect: PxRect { x: 2000, y: 10, w: 3000, h: 2140 }, owner: Some("0x3".into()) });
        st.geom.entry("work".into()).or_default().insert("0x2".into(), r);
        let clients = [
            client("0x1", "wezterm-herdr", "herdr · dev-lab", "1", &["app:herdr#1", "ws:work"]),
            client("0xai", "google-chrome-ai", "ИИ", "1", &["app:chrome-ai#1", "ws:work"]),
            client("0x3", "neovide", "[Scratch]", "1", &["app:neovide#1", "ws:work"]),
        ];
        let windows: Vec<&Client> = clients.iter().collect();
        let plan = arrange_layout(&mut st, &cfg, Some("work"), &windows, mon);
        let place = |addr: &str| plan.iter().find(|(a, _)| a == addr).map(|(_, r)| *r).unwrap();
        // Исходные места нынешнего назначения: обмен сохранён.
        assert_eq!(place("0xai"), center);
        assert_eq!(place("0x1"), record);
        assert_eq!(place("0x3"), PxRect { x: 3055, y: 10, w: 1920, h: 2140 });
        assert_eq!(st.main_app(&cfg, "work", mon).as_deref(), Some("chrome-ai"));
        assert!(st.moved.is_empty());
        // Запомненные прямоугольники — места плана; окно не на столе забыто.
        assert_eq!(st.geom["work"]["0x1"], record);
        assert!(!st.geom["work"].contains_key("0x2"));
    }

    #[test]
    fn depth_plan_keeps_order_and_lifts_main() {
        let order: Vec<String> = ["0x1", "0x2", "0x3"].iter().map(|s| s.to_string()).collect();
        // Поднятие workspace: главное окно становится верхним, остальные
        // сохраняют порядок между собой.
        assert_eq!(depth_plan(&order, Some("0x2"), true), vec!["0x1", "0x3", "0x2"]);
        // Главное окно уже наверху — порядок не меняется.
        assert_eq!(depth_plan(&order, Some("0x3"), true), vec!["0x1", "0x2", "0x3"]);
        // Перенос на другой стол: порядок по глубине остаётся прежним.
        assert_eq!(depth_plan(&order, Some("0x2"), false), vec!["0x1", "0x2", "0x3"]);
        // Главного окна среди окон workspace нет (скрыто или ещё запускается).
        assert_eq!(depth_plan(&order, None, true), vec!["0x1", "0x2", "0x3"]);
        assert_eq!(depth_plan(&order, Some("0x9"), true), vec!["0x1", "0x2", "0x3"]);
        // Окон нет — поднимать нечего.
        assert!(depth_plan(&[], Some("0x1"), true).is_empty());
    }

    #[test]
    fn raise_target_keeps_rect_on_move_in_both_modes() {
        let r = |x, y, w, h| PxRect { x, y, w, h };
        let place = Some(r(340, 10, 1200, 1400));
        let kept = Some(r(500, 50, 1000, 900));
        // Окно сдвинуто и растянуто мышью.
        let cur = r(420, 120, 1500, 1100);
        // Перенос на другой стол: окно встаёт туда, где стояло, а не на место
        // и не в запомненный прямоугольник.
        assert_eq!(raise_target(true, true, None, place, cur), Some(cur));
        assert_eq!(raise_target(true, true, kept, place, cur), Some(cur));
        // Окно, уже стоящее на столе, поднятие не двигает — режима у функции
        // больше нет, в режиме обмена мест тоже.
        assert_eq!(raise_target(false, false, kept, place, cur), None);
        assert_eq!(raise_target(false, false, None, place, cur), None);
        // Приходящее окно — в запомненный прямоугольник, без него — на место
        // приложения в раскладке; места нет — окно не двигается.
        assert_eq!(raise_target(false, true, kept, place, cur), kept);
        assert_eq!(raise_target(false, true, None, place, cur), place);
        assert_eq!(raise_target(false, true, None, None, cur), None);
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
    fn assign_desktop_keeps_workspace_on_one_desktop() {
        let mut st = State::default();
        st.desktops.insert(1, Desktop { workspaces: vec!["work".into(), "surf".into()], active: Some("surf".into()) });
        st.desktops.insert(2, Desktop { workspaces: vec!["chat".into()], active: Some("chat".into()) });
        st.lazy.insert(3, "surf".into());
        assign_desktop(&mut st, "surf", 2);
        // На прежнем столе surf не числится и активным там никто не стал:
        // преемника поднимает команда переноса, а не назначение стола.
        assert_eq!(st.desktops[&1].workspaces, vec!["work".to_string()]);
        assert_eq!(st.desktops[&1].active, None);
        // На новом столе surf встал в конец списка и стал активным.
        assert_eq!(st.desktops[&2].workspaces, vec!["chat".to_string(), "surf".to_string()]);
        assert_eq!(st.desktops[&2].active.as_deref(), Some("surf"));
        // Ленивое поднятие, назначенное surf на столе 3, снято.
        assert!(st.lazy.is_empty());

        // Повторное поднятие на том же столе список не удлиняет.
        assign_desktop(&mut st, "chat", 2);
        assert_eq!(st.desktops[&2].workspaces, vec!["chat".to_string(), "surf".to_string()]);
        assert_eq!(st.desktops[&2].active.as_deref(), Some("chat"));
    }

    #[test]
    fn assign_desktop_records_previous_active() {
        let mut st = State::default();
        assign_desktop(&mut st, "surf", 1);
        // Первый активный на столе: прежнего нет.
        assert!(!st.prev_active.contains_key(&1));
        assign_desktop(&mut st, "work", 1);
        assert_eq!(st.prev_active.get(&1).map(String::as_str), Some("surf"));
        // Повторное поднятие того же workspace историю не портит.
        assign_desktop(&mut st, "work", 1);
        assert_eq!(st.prev_active.get(&1).map(String::as_str), Some("surf"));
        // Уход work на другой стол историю стола 1 не меняет: преемник — surf.
        assign_desktop(&mut st, "work", 5);
        assert_eq!(st.prev_active.get(&1).map(String::as_str), Some("surf"));
        assert!(!st.prev_active.contains_key(&5));
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn successor_prefers_previous_active() {
        let list = names(&["a", "b", "c", "d"]);
        // История называет workspace из списка: он и становится активным.
        assert_eq!(successor(&list, "c", Some("a")).as_deref(), Some("a"));
        assert_eq!(successor(&list, "a", Some("d")).as_deref(), Some("d"));
    }

    #[test]
    fn successor_without_history_takes_previous_in_list() {
        let list = names(&["a", "b", "c"]);
        // Средний — предыдущий по списку.
        assert_eq!(successor(&list, "b", None).as_deref(), Some("a"));
        assert_eq!(successor(&list, "c", None).as_deref(), Some("b"));
        // Первый — по кругу последний.
        assert_eq!(successor(&list, "a", None).as_deref(), Some("c"));
        // Двое на столе: остаётся второй, с какой стороны ни считать.
        assert_eq!(successor(&names(&["a", "b"]), "a", None).as_deref(), Some("b"));
        assert_eq!(successor(&names(&["a", "b"]), "b", None).as_deref(), Some("a"));
    }

    #[test]
    fn successor_ignores_stale_history() {
        let list = names(&["a", "b", "c"]);
        // Workspace из истории уже не на этом столе: правило списка.
        assert_eq!(successor(&list, "b", Some("x")).as_deref(), Some("a"));
        // История называет сам перенесённый workspace: тоже правило списка.
        assert_eq!(successor(&list, "a", Some("a")).as_deref(), Some("c"));
    }

    #[test]
    fn successor_none_for_single_workspace() {
        assert_eq!(successor(&names(&["a"]), "a", None), None);
        assert_eq!(successor(&names(&["a"]), "a", Some("b")), None);
        assert_eq!(successor(&[], "a", None), None);
    }

    #[test]
    fn next_workspace_does_not_show_the_one_moved_away() {
        let mut st = State::default();
        st.desktops.insert(1, Desktop { workspaces: vec!["work".into(), "surf".into()], active: Some("work".into()) });
        // Пока оба workspace на столе 1, за work идёт surf.
        assert_eq!(next_ws(&st.desktops[&1]), Some("surf".to_string()));
        // После переноса surf на стол 2 следующим на столе 1 остаётся work.
        assign_desktop(&mut st, "surf", 2);
        assert_eq!(next_ws(&st.desktops[&1]), Some("work".to_string()));
        assert_eq!(next_ws(&st.desktops[&2]), Some("surf".to_string()));
        // На столе без workspace следующего нет.
        assert_eq!(next_ws(&Desktop::default()), None);
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

    // ---- Восстановление экземпляров (изменение session-instances) ----------

    fn restore_cfg() -> Config {
        let text = format!("{CFG}\n[apps.chrome-ai]\ncmd = \"google-chrome\"\nclass = \"^google-chrome-ai$\"\n\n[workspaces.surf]\ntemplate = \"thirds\"\nmode = \"stack\"\napps = {{ chrome-ai = \"right\", chromium = \"left\" }}\n");
        Config::parse(&text).unwrap()
    }

    fn entry(app: &str, num: u32, ws: &[&str], cmd: &[&str]) -> RestoreEntry {
        RestoreEntry {
            app: app.into(),
            instance: num,
            workspaces: ws.iter().map(|w| w.to_string()).collect(),
            desktop: "1".into(),
            rect: PxRect { x: 100, y: 100, w: 800, h: 600 },
            rects: BTreeMap::new(),
            cmd: cmd.iter().map(|c| c.to_string()).collect(),
            cwd: None,
            state: RestoreState::Waiting,
        }
    }

    #[test]
    fn raise_waits_for_restored_instance() {
        let cfg = restore_cfg();
        // neovide: первый экземпляр в work, второй — с командой — в surf.
        let mut e = vec![entry("neovide", 1, &["work"], &[]), entry("neovide", 2, &["surf"], &["neovide", "notes.md"]), entry("chromium", 1, &["work"], &[]), entry("chromium", 2, &["surf", "work"], &[])];
        // Первый экземпляр ждёт запуска: work запускает neovide его командой.
        assert_eq!(raise_restore_step(&cfg, &e, "neovide", "work", false), RaiseStep::LaunchFirst);
        // Окна уже есть — восстановление ничего не решает.
        assert_eq!(raise_restore_step(&cfg, &e, "neovide", "work", true), RaiseStep::Normal);
        // Запись с командой в surf ещё не запущена — как прежде.
        assert_eq!(raise_restore_step(&cfg, &e, "neovide", "surf", false), RaiseStep::Normal);
        // Запущена, окна нет: surf не получает окно neovide из work и не запускает neovide.
        e[1].state = RestoreState::Launched(4242);
        assert_eq!(raise_restore_step(&cfg, &e, "neovide", "surf", false), RaiseStep::Wait);
        // Первый экземпляр исполнен: запись без команды (второе окно chromium)
        // решению D3 шага 4 не мешает.
        e[2].state = RestoreState::Done;
        assert_eq!(raise_restore_step(&cfg, &e, "chromium", "surf", false), RaiseStep::Normal);
        // Первый экземпляр chromium ждёт, но в surf не входит.
        e[2].state = RestoreState::Waiting;
        assert_eq!(raise_restore_step(&cfg, &e, "chromium", "surf", false), RaiseStep::Normal);
        assert_eq!(raise_restore_step(&cfg, &e, "chromium", "work", false), RaiseStep::LaunchFirst);
    }

    #[test]
    fn restore_match_prefers_pid() {
        let mut e = vec![entry("neovide", 1, &["work"], &[]), entry("neovide", 2, &["work"], &["neovide", "notes.md"])];
        e[1].state = RestoreState::Launched(500);
        // Окно процесса 501, потомка запущенного 500: запись с командой,
        // хотя по приложению подошла бы и запись номер 1.
        assert_eq!(match_restore(&e, Some("neovide"), &[501, 500, 1], Some("work")), Some(1));
        // Чужой процесс: запись без команды с наименьшим номером.
        assert_eq!(match_restore(&e, Some("neovide"), &[777], Some("work")), Some(0));
        // Без приложения — только по pid.
        assert_eq!(match_restore(&e, None, &[777], None), None);
        assert_eq!(match_restore(&e, None, &[500], None), Some(1));
    }

    #[test]
    fn restore_match_by_number_and_workspace() {
        let mut e = vec![entry("chromium", 1, &["work"], &[]), entry("chromium", 2, &["surf", "work"], &[]), entry("chromium", 3, &["surf"], &[])];
        // Для work — наименьший номер среди записей с work.
        assert_eq!(match_restore(&e, Some("chromium"), &[], Some("work")), Some(0));
        // Для surf — наименьший среди записей с surf.
        assert_eq!(match_restore(&e, Some("chromium"), &[], Some("surf")), Some(1));
        // Workspace не задан или ни в одной записи — наименьший вообще.
        assert_eq!(match_restore(&e, Some("chromium"), &[], None), Some(0));
        assert_eq!(match_restore(&e, Some("chromium"), &[], Some("chat")), Some(0));
        // Исполненные записи не участвуют.
        e[0].state = RestoreState::Done;
        assert_eq!(match_restore(&e, Some("chromium"), &[], Some("work")), Some(1));
        // Записи другого приложения не подходят.
        assert_eq!(match_restore(&e, Some("neovide"), &[], Some("work")), None);
    }

    #[test]
    fn restored_window_keeps_number_and_membership() {
        let cfg = restore_cfg();
        // Окно второго экземпляра появилось раньше первого: номер 2 свободен.
        let clients = vec![hypr::test_client("0xa", "neovide", "", "1", &[])];
        assert_eq!(restored_number(&clients, "neovide", 2), 2);
        // Номер занят живым окном — наименьший свободный.
        let clients = vec![hypr::test_client("0xb", "neovide", "", "1", &["app:neovide#2"]), hypr::test_client("0xa", "neovide", "", "1", &[])];
        assert_eq!(restored_number(&clients, "neovide", 2), 1);
        // Состав — только workspace записи, описывающие приложение.
        let e = entry("chromium", 2, &["chat", "surf", "work"], &[]);
        assert_eq!(entry_members(&cfg, &e), vec!["surf".to_string(), "work".to_string()]);
        let e = entry("chrome-ai", 1, &["work"], &[]);
        assert!(entry_members(&cfg, &e).is_empty());
    }

    #[test]
    fn restored_window_goes_home() {
        let cfg = restore_cfg();
        let mon = (3840, 2160);
        let mut st = State::default();
        let act = desks(&[(1, "work"), (2, "surf")]);
        // Окно из surf при активном work на текущем столе уходит на стол surf
        // в прямоугольник, записанный для surf.
        let mut e = entry("chrome-ai", 1, &["surf"], &[]);
        let r = PxRect { x: 1700, y: 60, w: 1920, h: 2140 };
        e.rects.insert("surf".into(), r);
        assert_eq!(restore_place(&mut st, &cfg, &e, &act, 1, mon), (Home::Desktop(2), Some(r)));
        // Без записанного прямоугольника — место из конфига.
        e.rects.clear();
        assert_eq!(restore_place(&mut st, &cfg, &e, &act, 1, mon), (Home::Desktop(2), Some(PxRect { x: 3055, y: 10, w: 1920, h: 2140 })));
        // Общее окно surf и work при текущем столе 1 — место своего
        // приложения в раскладке work, прямоугольник surf не берётся.
        let mut e = entry("chromium", 2, &["surf", "work"], &[]);
        e.rects.insert("surf".into(), r);
        assert_eq!(restore_place(&mut st, &cfg, &e, &act, 1, mon), (Home::Desktop(1), Some(PxRect { x: -805, y: 10, w: 1920, h: 2140 })));
        // Окно workspace режима обмена мест встаёт в свой прямоугольник
        // из записи (изменение live-layout, решение D13).
        let moved = PxRect { x: 2600, y: 300, w: 1800, h: 1500 };
        e.rects.insert("work".into(), moved);
        assert_eq!(restore_place(&mut st, &cfg, &e, &act, 1, mon), (Home::Desktop(1), Some(moved)));
        // Без прямоугольника записи — изменённое место в раскладке из снимка.
        let mut n = entry("neovide", 1, &["work"], &[]);
        st.moved.entry("work".into()).or_default().insert("neovide".into(), Moved { rect: moved, owner: None });
        assert_eq!(restore_place(&mut st, &cfg, &n, &act, 1, mon), (Home::Desktop(1), Some(moved)));
        st.moved.clear();
        n.rects.clear();
        // Скрытое при снимке окно восстанавливается видимым на своём столе.
        let mut h = entry("neovide", 2, &["work"], &[]);
        h.desktop = "hidden".into();
        assert_eq!(restore_place(&mut st, &cfg, &h, &act, 2, mon), (Home::Desktop(1), Some(PxRect { x: 3055, y: 10, w: 1920, h: 2140 })));
        // Ни один workspace окна не активен — special:pool.
        let only = desks(&[(1, "work")]);
        assert_eq!(restore_place(&mut st, &cfg, &entry("chrome-ai", 1, &["surf"], &[]), &only, 1, mon), (Home::Pool, None));
        // Свободное окно приложения — свой стол и свой прямоугольник.
        let mut f = entry("calc", 1, &[], &[]);
        f.desktop = "3".into();
        assert_eq!(restore_place(&mut st, &cfg, &f, &act, 1, mon), (Home::Desktop(3), Some(f.rect)));
        f.desktop = "pool".into();
        assert_eq!(restore_place(&mut st, &cfg, &f, &act, 1, mon), (Home::Pool, None));
    }

    #[test]
    fn restore_wait_ends_on_key_and_save() {
        let mut e = vec![entry("chromium", 1, &["work"], &[]), entry("chromium", 2, &["work"], &[]), entry("neovide", 2, &["work"], &["neovide", "x"]), entry("chrome-ai", 1, &["surf"], &[])];
        e[0].state = RestoreState::Done;
        e[2].state = RestoreState::Launched(10);
        let apps: BTreeSet<String> = ["chromium".to_string()].into();
        // Клавиша chromium снимает ожидание его записей без команды.
        let mut k = e.clone();
        assert_eq!(end_wait(&mut k, &apps, Some("chromium"), false), vec!["chromium#2".to_string()]);
        assert_eq!(k[2].state, RestoreState::Launched(10));
        // Сохранение снимает ожидания окон: запущенный экземпляр и записи
        // приложения, запущенного при восстановлении; запись workspace, ещё
        // не поднятого (chrome-ai), остаётся ждать поднятия.
        let mut sv = e.clone();
        assert_eq!(end_wait(&mut sv, &apps, None, false), vec!["chromium#2".to_string(), "neovide#2".to_string()]);
        assert_eq!(sv[3].state, RestoreState::Waiting);
        // Загрузка снимает весь план.
        let mut ld = e.clone();
        assert_eq!(end_wait(&mut ld, &apps, None, true).len(), 3);
        assert!(ld.iter().all(|x| !x.open()));
    }
}
