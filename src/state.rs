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

/// Постороннее окно: привязка к workspace и данные для восстановления.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Foreign {
    pub workspace: Option<String>,
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

    /// Прямоугольник окна приложения в workspace с учётом зазора для ячеек.
    pub fn rect_for(&mut self, cfg: &Config, ws: &str, app: &str, mon: (i32, i32)) -> Option<PxRect> {
        let place = self.cells_of(cfg, ws, mon).get(app)?.clone();
        match place {
            Place::Cell(c) => {
                let t = cfg.templates.get(&cfg.workspaces.get(ws)?.template)?;
                Some(t.cells.get(&c)?.resolve(mon.0, mon.1).inset(cfg.gap))
            }
            Place::Rect { rect } => Some(rect),
        }
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
