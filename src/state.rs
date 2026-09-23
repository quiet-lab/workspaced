//! Модель демона: столы, раскладки workspace, посторонние окна — и формат сессии.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::config::{App, Config, Placement, PxRect};

/// Место окна в workspace: ячейка шаблона или прямоугольник в пикселях.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Place {
    Cell(String),
    Rect { rect: PxRect },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Desktop {
    #[serde(default)]
    pub workspaces: Vec<String>,
    #[serde(default)]
    pub active: Option<String>,
}

/// Постороннее окно: положение и данные для восстановления. К workspace
/// демон его не привязывает: окно становится приложением workspace только
/// по команде сохранения.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Foreign {
    pub rect: PxRect,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Дополнительное приложение workspace, принятое командой сохранения сессии
/// (спецификация ws-sessions, «Дополнительные приложения сессии»). Запись
/// живёт только в сессии: конфиг она не меняет. Пустой `cmd` означает, что
/// запись называет приложение конфига и задаёт ему лишь место в этом
/// workspace; иначе запись описывает и само приложение.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ExtraApp {
    /// Класс окна как есть; в выражение захвата демон превращает его сам.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub rect: PxRect,
}

impl ExtraApp {
    /// Запись приложения для эффективного конфига: команда и каталог из
    /// процесса окна, класс — точное совпадение с классом окна.
    pub fn to_app(&self) -> App {
        let (cmd, args) = match self.cmd.split_first() {
            Some((c, rest)) => (Some(c.clone()), rest.to_vec()),
            None => (None, Vec::new()),
        };
        App { cmd, args, cwd: self.cwd.clone(), class: self.class.as_deref().map(regex::escape), ..App::default() }
    }
}

/// Изменённое место приложения в раскладке workspace (изменение live-layout,
/// решения D1, D9): прямоугольник, снятый с экрана, и адрес окна, с которого
/// он снят. По владельцу закрытие окна находит места, которые пора вернуть
/// к описанию; у места из снимка сессии владельца нет, пока окно не встанет
/// на это место или раскладку не снимут с экрана.
#[derive(Debug, Clone, PartialEq)]
pub struct Moved {
    pub rect: PxRect,
    pub owner: Option<String>,
}

/// Незаконченный цикл клавиши приложения в workspace (спецификация ws-daemon,
/// «Цепочка приложения»): чьи экземпляры перебираются и к какому окну вернёт
/// конец цикла (prev). Запись живёт в памяти демона: это след незаконченного
/// цикла, а не состояние окон, поэтому в снимок сессии она не попадает.
#[derive(Debug, Clone)]
pub struct Cycle {
    pub app: String,
    pub back: Option<String>,
}

#[derive(Debug, Default)]
pub struct State {
    pub desktops: BTreeMap<u8, Desktop>,
    /// workspace → приложение → исходное место в раскладке (ячейка или
    /// прямоугольник; при первом обращении — из конфига, после обмена мест —
    /// исходное место другого приложения).
    pub cells: BTreeMap<String, BTreeMap<String, Place>>,
    /// workspace → приложение → изменённое место: прямоугольник, который
    /// пользователь задал окну приложения (изменение live-layout, решение D1).
    /// Прямоугольник места — изменённый, а без него — исходный.
    pub moved: BTreeMap<String, BTreeMap<String, Moved>>,
    /// workspace → главное приложение (решение D5): обмен мест делает главным
    /// вызванное приложение, при первом обращении — поле `main` раздела или
    /// приложение, записанное в ячейку `main` шаблона.
    pub main: BTreeMap<String, String>,
    /// workspace → незаконченный цикл клавиши приложения.
    pub cycle: BTreeMap<String, Cycle>,
    /// workspace → последнее его окно, получавшее фокус (событие `activewindow`).
    pub focus: BTreeMap<String, String>,
    /// workspace → адрес окна → прямоугольник, в котором окно оставили на столе
    /// этого workspace, в обоих режимах (решение D8): вернувшееся на стол окно
    /// встаёт именно туда. Живёт до остановки демона, в снимок сессии попадает
    /// по команде сохранения.
    pub geom: BTreeMap<String, HashMap<String, PxRect>>,
    /// workspace → имя → дополнительное приложение сессии.
    pub extra: BTreeMap<String, BTreeMap<String, ExtraApp>>,
    /// адрес окна → посторонняя привязка.
    pub foreign: HashMap<String, Foreign>,
    /// Столы, чей активный workspace поднимается при первом переходе (ленивое восстановление).
    pub lazy: BTreeMap<u8, String>,
    /// стол → workspace, который был активен на нём до нынешнего активного
    /// (история активности стола). Из неё перенос workspace на другой стол
    /// выбирает преемника на прежнем столе (спецификация ws-daemon, «Перенос
    /// workspace на стол»). Живёт до остановки демона, в снимок сессии не
    /// попадает: это след работы пользователя, а не состояние окон.
    pub prev_active: BTreeMap<u8, String>,
    /// План восстановления экземпляров из снимка сессии (изменение
    /// session-instances, решения D4, D5, D8): записи окон приложений,
    /// ожидающие запуска или окна. Живёт только в памяти демона.
    pub restore: Vec<RestoreEntry>,
    /// Приложения, которые демон запустил при восстановлении: только их
    /// процесс может открыть свои окна заново, и только для них записи
    /// без командной строки ждут окон (решение D10).
    pub restore_apps: BTreeSet<String>,
}

impl State {
    pub fn desktop(&mut self, n: u8) -> &mut Desktop {
        self.desktops.entry(n).or_default()
    }

    /// Назначение ячеек workspace; при первом обращении берётся из конфига.
    pub fn cells_of(&mut self, cfg: &Config, ws: &str, mon: (i32, i32)) -> &mut BTreeMap<String, Place> {
        self.cells.entry(ws.to_string()).or_insert_with(|| {
            cfg.workspaces
                .get(ws)
                .map(|w| {
                    w.apps
                        .iter()
                        .map(|(a, e)| {
                            let place = match &e.place {
                                Placement::Cell(c) => Place::Cell(c.clone()),
                                Placement::Rect { rect } => Place::Rect { rect: rect.resolve(mon.0, mon.1) },
                            };
                            (a.clone(), place)
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// Место окна приложения по правилу из спецификации ws-daemon
    /// («Расстановка окон»): место приложения в раскладке workspace —
    /// изменённый прямоугольник, а без него исходное место; то же у его
    /// семейства; `rect` самого приложения; иначе места нет и окно встаёт
    /// в центр рабочей области. Прямоугольник передаётся композитору как записан,
    /// без отступов от демона.
    pub fn rect_for(&mut self, cfg: &Config, ws: &str, app: &str, mon: (i32, i32)) -> Option<PxRect> {
        let owner = if self.cells_of(cfg, ws, mon).contains_key(app) { Some(app) } else { cfg.family_of(app).filter(|f| self.cells.get(ws).is_some_and(|c| c.contains_key(*f))) };
        let Some(entry) = owner else { return Self::app_rect(cfg, app, mon) };
        if let Some(m) = self.moved.get(ws).and_then(|m| m.get(entry)) {
            return Some(m.rect);
        }
        self.base_rect(cfg, ws, entry, mon)
    }

    /// Исходное место записи `entry` workspace без изменённого прямоугольника:
    /// ячейка шаблона или прямоугольник записи. `None` — записи нет.
    pub fn base_rect(&mut self, cfg: &Config, ws: &str, entry: &str, mon: (i32, i32)) -> Option<PxRect> {
        match self.cells_of(cfg, ws, mon).get(entry).cloned()? {
            Place::Cell(c) => {
                let t = cfg.templates.get(&cfg.workspaces.get(ws)?.template)?;
                Some(t.cells.get(&c)?.resolve(mon.0, mon.1))
            }
            Place::Rect { rect } => Some(rect),
        }
    }

    /// Положение и размер окна приложения по умолчанию (`rect` у `[apps.<имя>]`).
    pub fn app_rect(cfg: &Config, app: &str, mon: (i32, i32)) -> Option<PxRect> {
        cfg.apps.get(app)?.rect.as_ref().map(|r| r.resolve(mon.0, mon.1))
    }

    /// Главное приложение workspace (изменение live-layout, решение D5):
    /// `State::main`, а при первом обращении — приложение из поля `main`
    /// раздела, без него — приложение, записанное в ячейку `main` шаблона;
    /// найденное запоминается в `State::main`. Приложение, которого
    /// в раскладке больше нет (снята запись сессии), главным не считается,
    /// и главное ищется заново.
    pub fn main_app(&mut self, cfg: &Config, ws: &str, mon: (i32, i32)) -> Option<String> {
        let cells = self.cells_of(cfg, ws, mon).clone();
        if let Some(m) = self.main.get(ws).filter(|m| cells.contains_key(*m)) {
            return Some(m.clone());
        }
        let w = cfg.workspaces.get(ws)?;
        let main_cell = cfg.templates.get(&w.template).map(|t| t.main.clone());
        let found = w
            .main
            .clone()
            .filter(|m| cells.contains_key(m))
            .or_else(|| cells.iter().find(|(_, p)| matches!((p, &main_cell), (Place::Cell(c), Some(mc)) if c == mc)).map(|(a, _)| a.clone()))?;
        self.main.insert(ws.to_string(), found.clone());
        Some(found)
    }

    /// Снять раскладку workspace в памяти (назначение мест, главное
    /// приложение, изменённые места) и запомненные прямоугольники его окон:
    /// раскладка соберётся заново из конфига при следующем обращении.
    pub fn reset_layout(&mut self, ws: &str) {
        self.cells.remove(ws);
        self.moved.remove(ws);
        self.main.remove(ws);
        self.geom.remove(ws);
    }
}

// ---- Восстановление экземпляров ----------------------------------------------

/// Состояние записи плана восстановления (решения D4, D10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreState {
    /// Ждёт: запись с командной строкой — запуска при поднятии своего
    /// workspace, запись без неё — окна, которое откроет процесс приложения
    /// (первый экземпляр запускается командой приложения).
    Waiting,
    /// Процесс запущен по командной строке записи; ждёт окна этого процесса.
    Launched(u32),
    /// Окно получено, либо ожидание снято.
    Done,
}

/// Запись плана восстановления: окно приложения из снимка сессии, которое
/// ждёт запуска или появления (изменение session-instances, решение D5).
#[derive(Debug, Clone, PartialEq)]
pub struct RestoreEntry {
    pub app: String,
    pub instance: u32,
    pub workspaces: Vec<String>,
    /// «1»…«8», «pool» или «hidden» на момент снимка.
    pub desktop: String,
    pub rect: PxRect,
    pub rects: BTreeMap<String, PxRect>,
    pub cmd: Vec<String>,
    pub cwd: Option<String>,
    pub state: RestoreState,
}

impl RestoreEntry {
    /// Запись по окну снимка нового формата; окно без номера экземпляра
    /// (прежний формат) и постороннее окно записи не дают.
    pub fn from_window(w: &SessionWindow) -> Option<RestoreEntry> {
        Some(RestoreEntry {
            app: w.app.clone()?,
            instance: w.instance?,
            workspaces: w.workspaces.clone(),
            desktop: w.desktop.clone(),
            rect: w.rect,
            rects: w.rects.clone(),
            cmd: w.cmd.clone(),
            cwd: w.cwd.clone(),
            state: RestoreState::Waiting,
        })
    }
    /// Запись ещё ждёт запуска или окна.
    pub fn open(&self) -> bool {
        self.state != RestoreState::Done
    }
}

// ---- Сессия ------------------------------------------------------------------

/// Раскладка workspace в снимке (изменение live-layout, решения D13, D14):
/// исходные места приложений, главное приложение, изменённые места
/// и дополнительные приложения сессии. Поля `main` и `moved` необязательные:
/// снимок прежнего формата читается без них, а прежняя версия демона их
/// пропускает.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionWorkspace {
    /// Главное приложение workspace. Простое поле идёт первым: TOML требует,
    /// чтобы вложенные таблицы шли после простых полей.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,
    #[serde(default)]
    pub cells: BTreeMap<String, Place>,
    /// Изменённые места: приложение → прямоугольник. Владелец места (адрес
    /// окна) в снимок не пишется: адрес не переживает перезагрузку.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub moved: BTreeMap<String, PxRect>,
    /// Дополнительные приложения сессии этого workspace. Поле необязательное:
    /// снимок прежнего формата читается без ошибки.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_apps: BTreeMap<String, ExtraApp>,
}

/// Окно в снимке: окно приложения либо постороннее (с командой и каталогом).
/// Окно приложения записывается экземпляром (изменение session-instances,
/// решение D1): номер, workspace по тегам состава и прямоугольники по всем
/// его workspace (изменение live-layout, решение D13); командная строка
/// у него есть только по решению D2. Поля
/// необязательные: снимок прежнего формата читается, а прежняя версия демона
/// пропускает новые поля.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionWindow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Номер экземпляра из тега `app:<имя>#<номер>`; признак нового формата.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<u32>,
    /// Workspace окна по тегам состава, по алфавиту.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<String>,
    /// Привязка постороннего окна из снимков прежних версий: читается ради
    /// совместимости, не учитывается и не записывается.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// «1»…«8», «pool» или «hidden».
    pub desktop: String,
    pub rect: PxRect,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// workspace окна → прямоугольник, в котором окно оставлено на столе
    /// этого workspace, в обоих режимах.
    /// Таблица пишется последней: TOML требует, чтобы вложенные таблицы
    /// шли после простых полей.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rects: BTreeMap<String, PxRect>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    pub saved: String,
    pub active_desktop: u8,
    #[serde(default)]
    pub desktops: BTreeMap<String, Desktop>,
    #[serde(default)]
    pub workspaces: BTreeMap<String, SessionWorkspace>,
    #[serde(default)]
    pub windows: Vec<SessionWindow>,
}

#[cfg(test)]
mod tests {
    use super::*;

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

[apps.neovide]
cmd = "neovide"
rect = { x = 2600, y = 1500, w = 600, h = 400 }

[workspaces.work]
template = "thirds"
main = "wezterm"
apps = { wezterm = "center" }
"#;

    #[test]
    fn snapshot_keeps_extra_apps_and_reads_old_format() {
        // Снимок прежнего формата: таблицы дополнительных приложений нет.
        let old = r#"
saved = "2026-09-22T11:34:44+03:00"
active_desktop = 1
[desktops.1]
workspaces = ["work"]
active = "work"
[workspaces.work.cells]
chromium = "left"
"#;
        let s: Session = toml::from_str(old).unwrap();
        assert!(s.workspaces["work"].extra_apps.is_empty());
        assert_eq!(s.workspaces["work"].cells["chromium"], Place::Cell("left".into()));

        // Запись и чтение дополнительного приложения: полная запись и запись
        // только о месте.
        let mut s = s;
        let rect = PxRect { x: 100, y: 200, w: 800, h: 600 };
        let extra = &mut s.workspaces.get_mut("work").unwrap().extra_apps;
        extra.insert("galculator".into(), ExtraApp { class: Some("Galculator".into()), cmd: vec!["galculator".into()], cwd: Some("/home/mne".into()), rect });
        extra.insert("chrome-ai".into(), ExtraApp { rect, ..ExtraApp::default() });
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Session = toml::from_str(&text).unwrap();
        let extra = &back.workspaces["work"].extra_apps;
        assert_eq!(extra["galculator"].cmd, vec!["galculator".to_string()]);
        assert_eq!(extra["galculator"].class.as_deref(), Some("Galculator"));
        assert_eq!(extra["galculator"].rect, rect);
        // Запись только о месте пишется без класса и команды.
        assert_eq!(extra["chrome-ai"], ExtraApp { rect, ..ExtraApp::default() });
        assert!(!text.contains("class = \"\""), "{text}");
    }

    #[test]
    fn extra_app_becomes_app_of_effective_config() {
        let e = ExtraApp { class: Some("Galculator".into()), cmd: vec!["galculator".into(), "--mode=paper".into()], cwd: Some("/tmp".into()), rect: PxRect { x: 0, y: 0, w: 10, h: 10 } };
        let app = e.to_app();
        assert_eq!(app.cmd.as_deref(), Some("galculator"));
        assert_eq!(app.args, vec!["--mode=paper".to_string()]);
        assert_eq!(app.cwd.as_deref(), Some("/tmp"));
        // Класс окна превращается в выражение точного совпадения.
        let (class_re, title_re) = app.matchers().unwrap().unwrap();
        assert!(class_re.is_match("Galculator") && !class_re.is_match("Galculator2"));
        assert!(title_re.is_none());
    }

    #[test]
    fn place_rule_four_steps() {
        let cfg = Config::parse(CFG).unwrap();
        let mon = (3840, 2160);
        let mut st = State::default();
        // Ячейка приложения в workspace.
        assert_eq!(st.rect_for(&cfg, "work", "wezterm", mon), Some(PxRect { x: 1125, y: 10, w: 1920, h: 2140 }));
        // Вариант в workspace не описан: место берётся у семейства.
        assert_eq!(st.rect_for(&cfg, "work", "herdr", mon), Some(PxRect { x: 1125, y: 10, w: 1920, h: 2140 }));
        // Ни приложения, ни семейства в workspace нет: rect приложения.
        assert_eq!(st.rect_for(&cfg, "work", "neovide", mon), Some(PxRect { x: 2600, y: 1500, w: 600, h: 400 }));
        // Ни места в workspace, ни rect приложения: места нет, окно идёт в центр рабочей области.
        st.cells_of(&cfg, "work", mon).clear();
        assert_eq!(st.rect_for(&cfg, "work", "wezterm", mon), None);
        // Вариант с собственным местом в workspace побеждает семейство.
        let mut st = State::default();
        st.cells_of(&cfg, "work", mon).insert("herdr".into(), Place::Cell("left".into()));
        assert_eq!(st.rect_for(&cfg, "work", "herdr", mon), Some(PxRect { x: -805, y: 10, w: 1920, h: 2140 }));
    }

    #[test]
    fn rect_for_prefers_moved_place() {
        let cfg = Config::parse(CFG).unwrap();
        let mon = (3840, 2160);
        let mut st = State::default();
        let moved = PxRect { x: 400, y: 300, w: 1400, h: 1000 };
        st.moved.entry("work".into()).or_default().insert("wezterm".into(), Moved { rect: moved, owner: None });
        // Изменённое место побеждает ячейку.
        assert_eq!(st.rect_for(&cfg, "work", "wezterm", mon), Some(moved));
        // Вариант без своей записи берёт изменённое место семейства.
        assert_eq!(st.rect_for(&cfg, "work", "herdr", mon), Some(moved));
        // Исходное место без изменённого.
        assert_eq!(st.base_rect(&cfg, "work", "wezterm", mon), Some(PxRect { x: 1125, y: 10, w: 1920, h: 2140 }));
        // Изменённое место у варианта со своей записью — его собственное.
        st.cells_of(&cfg, "work", mon).insert("herdr".into(), Place::Cell("left".into()));
        assert_eq!(st.rect_for(&cfg, "work", "herdr", mon), Some(PxRect { x: -805, y: 10, w: 1920, h: 2140 }));
        // Приложение вне workspace — свой rect, изменённые места не при чём.
        assert_eq!(st.rect_for(&cfg, "work", "neovide", mon), Some(PxRect { x: 2600, y: 1500, w: 600, h: 400 }));
    }

    #[test]
    fn main_app_is_explicit() {
        let cfg = Config::parse(&CFG.replace("apps = { wezterm = \"center\" }", "apps = { wezterm = \"center\", neovide = \"right\" }")).unwrap();
        let mon = (3840, 2160);
        let mut st = State::default();
        // Поле main раздела при первом обращении.
        assert_eq!(st.main_app(&cfg, "work", mon).as_deref(), Some("wezterm"));
        assert_eq!(st.main.get("work").map(String::as_str), Some("wezterm"));
        // После записи workspace место главного — rect, ячейки main нет
        // ни у кого, а главное приложение — по полю main.
        let text = CFG.replace("apps = { wezterm = \"center\" }", "apps = { wezterm = { rect = { x = 1125, y = 10, w = 2400, h = 2140 } }, neovide = \"right\" }");
        let cfg = Config::parse(&text).unwrap();
        let mut st = State::default();
        assert_eq!(st.main_app(&cfg, "work", mon).as_deref(), Some("wezterm"));
        // State::main главнее поля раздела.
        st.main.insert("work".into(), "neovide".into());
        assert_eq!(st.main_app(&cfg, "work", mon).as_deref(), Some("neovide"));
        // Приложения нет в раскладке — главное ищется заново.
        st.cells_of(&cfg, "work", mon).remove("neovide");
        assert_eq!(st.main_app(&cfg, "work", mon).as_deref(), Some("wezterm"));
        // Без поля main — приложение, записанное в ячейку main шаблона.
        let cfg = Config::parse(&CFG.replace("main = \"wezterm\"\n", "")).unwrap();
        let mut st = State::default();
        assert_eq!(st.main_app(&cfg, "work", mon).as_deref(), Some("wezterm"));
        // Снятие раскладки забывает и главное приложение.
        st.reset_layout("work");
        assert!(st.main.is_empty() && st.cells.is_empty());
    }

    #[test]
    fn session_workspace_layout_roundtrip() {
        let r = PxRect { x: 400, y: 300, w: 1400, h: 1000 };
        let mut s = Session { saved: "t".into(), active_desktop: 1, ..Session::default() };
        let w = s.workspaces.entry("work".into()).or_default();
        w.main = Some("chrome-ai".into());
        w.cells.insert("chrome-ai".into(), Place::Cell("center".into()));
        w.moved.insert("herdr".into(), r);
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Session = toml::from_str(&text).unwrap();
        assert_eq!(back.workspaces["work"].main.as_deref(), Some("chrome-ai"));
        assert_eq!(back.workspaces["work"].moved["herdr"], r);
        assert_eq!(back.workspaces["work"].cells["chrome-ai"], Place::Cell("center".into()));
        // Пустые поля не пишутся.
        let bare = Session { workspaces: BTreeMap::from([("surf".to_string(), SessionWorkspace::default())]), ..Session::default() };
        let text = toml::to_string_pretty(&bare).unwrap();
        assert!(!text.contains("main") && !text.contains("moved"), "{text}");
        // Снимок прежнего формата читается без них.
        let old = "saved = \"t\"\nactive_desktop = 1\n[workspaces.work.cells]\nchromium = \"left\"\n";
        let s: Session = toml::from_str(old).unwrap();
        assert!(s.workspaces["work"].main.is_none() && s.workspaces["work"].moved.is_empty());
    }

    #[test]
    fn session_window_instance_roundtrip() {
        // Новый формат: номер, состав, прямоугольники stack, команда экземпляра.
        let r = PxRect { x: 1700, y: 60, w: 1920, h: 2140 };
        let w = SessionWindow {
            app: Some("neovide".into()),
            instance: Some(2),
            workspaces: vec!["surf".into(), "work".into()],
            desktop: "1".into(),
            rect: PxRect { x: 3055, y: 10, w: 1920, h: 2140 },
            rects: BTreeMap::from([("surf".to_string(), r)]),
            cmd: vec!["neovide".into(), "notes.md".into()],
            cwd: Some("/home/mne/notes".into()),
            ..SessionWindow::default()
        };
        let s = Session { saved: "t".into(), active_desktop: 1, windows: vec![w.clone()], ..Session::default() };
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Session = toml::from_str(&text).unwrap();
        let b = &back.windows[0];
        assert_eq!(b.instance, Some(2));
        assert_eq!(b.workspaces, w.workspaces);
        assert_eq!(b.rects, w.rects);
        assert_eq!(b.cmd, w.cmd);
        let e = RestoreEntry::from_window(b).unwrap();
        assert_eq!((e.app.as_str(), e.instance, e.state), ("neovide", 2, RestoreState::Waiting));

        // Пустые поля не пишутся.
        let bare = SessionWindow { app: Some("chromium".into()), instance: Some(1), desktop: "1".into(), ..SessionWindow::default() };
        let text = toml::to_string_pretty(&Session { windows: vec![bare], ..Session::default() }).unwrap();
        assert!(!text.contains("workspaces = []") && !text.contains("rects") && !text.contains("cmd"), "{text}");

        // Прежний формат: номера нет, запись плана не получается.
        let old = r#"
saved = "2026-09-23T14:12:21+03:00"
active_desktop = 2
[[windows]]
app = "neovide"
desktop = "1"
[windows.rect]
x = 3055
y = 10
w = 1920
h = 2140
"#;
        let s: Session = toml::from_str(old).unwrap();
        assert_eq!(s.windows[0].instance, None);
        assert!(s.windows[0].workspaces.is_empty() && s.windows[0].rects.is_empty());
        assert!(RestoreEntry::from_window(&s.windows[0]).is_none());

        // Неизвестные поля (снимок более новой версии) разбору не мешают.
        let newer = format!("{old}future = 1\n");
        let s: Session = toml::from_str(&newer.replace("desktop = \"1\"", "desktop = \"1\"\nstate = \"x\"")).unwrap();
        assert_eq!(s.windows[0].app.as_deref(), Some("neovide"));
    }
}
