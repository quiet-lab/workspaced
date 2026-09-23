//! Модель демона: столы, назначение ячеек, посторонние окна — и формат сессии.

use std::collections::{BTreeMap, HashMap};

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
    /// workspace → приложение → место (текущее назначение, по умолчанию из конфига).
    pub cells: BTreeMap<String, BTreeMap<String, Place>>,
    /// workspace → незаконченный цикл клавиши приложения.
    pub cycle: BTreeMap<String, Cycle>,
    /// workspace → последнее его окно, получавшее фокус (событие `activewindow`).
    pub focus: BTreeMap<String, String>,
    /// workspace → адрес окна → прямоугольник, в котором окно оставили.
    /// Нужен режиму `stack`: вернувшееся на стол окно встаёт именно туда.
    /// Живёт до остановки демона, как геометрия окна до развёртывания.
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
                        .map(|(a, p)| {
                            let place = match p {
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
    /// («Расстановка окон»): ячейка или `rect`, записанные для приложения
    /// в workspace; то же, записанное для его семейства; `rect` самого
    /// приложения; иначе места нет и окно встаёт в центр экрана.
    /// Прямоугольник передаётся композитору как записан, без отступов от демона.
    pub fn rect_for(&mut self, cfg: &Config, ws: &str, app: &str, mon: (i32, i32)) -> Option<PxRect> {
        let cells = self.cells_of(cfg, ws, mon);
        let place = cells.get(app).cloned().or_else(|| cfg.family_of(app).and_then(|f| cells.get(f).cloned()));
        match place {
            Some(Place::Cell(c)) => {
                let t = cfg.templates.get(&cfg.workspaces.get(ws)?.template)?;
                Some(t.cells.get(&c)?.resolve(mon.0, mon.1))
            }
            Some(Place::Rect { rect }) => Some(rect),
            None => Self::app_rect(cfg, app, mon),
        }
    }

    /// Положение и размер окна приложения по умолчанию (`rect` у `[apps.<имя>]`).
    pub fn app_rect(cfg: &Config, app: &str, mon: (i32, i32)) -> Option<PxRect> {
        cfg.apps.get(app)?.rect.as_ref().map(|r| r.resolve(mon.0, mon.1))
    }

    /// Приложение, стоящее сейчас в главной ячейке workspace.
    pub fn main_app(&mut self, cfg: &Config, ws: &str, mon: (i32, i32)) -> Option<String> {
        let main_cell = cfg.templates.get(&cfg.workspaces.get(ws)?.template)?.main.clone();
        self.cells_of(cfg, ws, mon)
            .iter()
            .find(|(_, p)| matches!(p, Place::Cell(c) if *c == main_cell))
            .map(|(a, _)| a.clone())
    }
}

// ---- Сессия ------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionWorkspace {
    #[serde(default)]
    pub cells: BTreeMap<String, Place>,
    /// Дополнительные приложения сессии этого workspace. Поле необязательное:
    /// снимок прежнего формата читается без ошибки.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_apps: BTreeMap<String, ExtraApp>,
}

/// Окно в снимке: окно приложения либо постороннее (с командой и каталогом).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionWindow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
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
        // Ни места в workspace, ни rect приложения: места нет, окно идёт в центр экрана.
        st.cells_of(&cfg, "work", mon).clear();
        assert_eq!(st.rect_for(&cfg, "work", "wezterm", mon), None);
        // Вариант с собственным местом в workspace побеждает семейство.
        let mut st = State::default();
        st.cells_of(&cfg, "work", mon).insert("herdr".into(), Place::Cell("left".into()));
        assert_eq!(st.rect_for(&cfg, "work", "herdr", mon), Some(PxRect { x: -805, y: 10, w: 1920, h: 2140 }));
    }
}
