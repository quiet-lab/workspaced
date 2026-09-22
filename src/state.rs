//! Модель демона: столы, назначение ячеек, посторонние окна — и формат сессии.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::config::{Config, Placement, PxRect};

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

#[derive(Debug, Default)]
pub struct State {
    pub desktops: BTreeMap<u8, Desktop>,
    /// workspace → приложение → место (текущее назначение, по умолчанию из конфига).
    pub cells: BTreeMap<String, BTreeMap<String, Place>>,
    /// адрес окна → посторонняя привязка.
    pub foreign: HashMap<String, Foreign>,
    /// Столы, чей активный workspace поднимается при первом переходе (ленивое восстановление).
    pub lazy: BTreeMap<u8, String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionWorkspace {
    #[serde(default)]
    pub cells: BTreeMap<String, Place>,
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
