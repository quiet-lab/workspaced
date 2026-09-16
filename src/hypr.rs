//! Связь с Hyprland: запросы и диспетчеры по `.socket.sock`, события по `.socket2.sock`.
//!
//! С Lua-конфигом диспетчеры принимаются только выражениями Lua
//! (`dispatch hl.dsp.focus({ workspace = 3 })`); несколько шагов по порядку —
//! одним выражением-функцией, чтобы они не обгоняли друг друга.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::PxRect;

#[derive(Clone)]
pub struct Hypr {
    sock: PathBuf,
    sock2: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WsRef {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Client {
    pub address: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub title: String,
    pub workspace: WsRef,
    #[serde(default)]
    pub pid: i32,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub at: (i32, i32),
    #[serde(default)]
    pub size: (i32, i32),
    #[serde(default)]
    pub floating: bool,
    #[serde(default)]
    pub mapped: bool,
    /// Полноэкранный режим композитора: 0 — нет, 1 — maximized, 2 — fullscreen.
    #[serde(default)]
    pub fullscreen: i32,
}

impl Client {
    /// Имя приложения из тега `app:<имя>` (звёздочка правила отбрасывается).
    pub fn app(&self) -> Option<String> {
        self.tags.iter().find_map(|t| t.strip_prefix("app:").map(|s| s.trim_end_matches('*').to_string()))
    }
    pub fn rect(&self) -> PxRect {
        PxRect { x: self.at.0, y: self.at.1, w: self.size.0, h: self.size.1 }
    }
    /// Номер стола 1…8, если окно на обычном столе.
    pub fn desktop(&self) -> Option<u8> {
        self.workspace.name.parse::<u8>().ok().filter(|n| (1..=8).contains(n))
    }
    pub fn on_hidden(&self) -> bool {
        self.workspace.name == "special:hidden"
    }
    pub fn on_pool(&self) -> bool {
        self.workspace.name == "special:pool"
    }
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Monitor {
    pub name: String,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Зарезервированные слоями зоны: left, top, right, bottom.
    #[serde(default)]
    pub reserved: Vec<i32>,
    #[serde(default)]
    pub focused: bool,
}

impl Monitor {
    /// Рабочая область: монитор без зарезервированных зон.
    pub fn work_area(&self) -> PxRect {
        let r = |i: usize| self.reserved.get(i).copied().unwrap_or(0);
        PxRect { x: self.x + r(0), y: self.y + r(1), w: self.width - r(0) - r(2), h: self.height - r(1) - r(3) }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActiveWorkspace {
    pub name: String,
}

/// Событие композитора (разобранная строка `.socket2.sock`).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum Event {
    OpenWindow { addr: String, workspace: String },
    CloseWindow { addr: String },
    MoveWindow { addr: String, workspace: String },
    Workspace { name: String },
    ActiveWindow { addr: String },
    Other(String),
}

/// Адрес в виде `0x…` строчными буквами (события приходят без `0x`).
pub fn norm_addr(a: &str) -> String {
    let a = a.trim().to_ascii_lowercase();
    if a.starts_with("0x") { a } else { format!("0x{a}") }
}

impl Hypr {
    pub fn new() -> Result<Self> {
        let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").context("нет HYPRLAND_INSTANCE_SIGNATURE: демон запускается только в сессии Hyprland")?;
        let rt = std::env::var("XDG_RUNTIME_DIR").context("нет XDG_RUNTIME_DIR")?;
        let dir = PathBuf::from(rt).join("hypr").join(sig);
        Ok(Hypr { sock: dir.join(".socket.sock"), sock2: dir.join(".socket2.sock") })
    }

    pub fn request(&self, req: &str) -> Result<String> {
        let mut s = UnixStream::connect(&self.sock).context("сокет Hyprland недоступен")?;
        s.write_all(req.as_bytes())?;
        let mut out = String::new();
        s.read_to_string(&mut out)?;
        Ok(out)
    }

    pub fn clients(&self) -> Result<Vec<Client>> {
        let text = self.request("j/clients")?;
        serde_json::from_str(&text).context("разбор j/clients")
    }

    pub fn active_workspace(&self) -> Result<String> {
        let text = self.request("j/activeworkspace")?;
        let ws: ActiveWorkspace = serde_json::from_str(&text).context("разбор j/activeworkspace")?;
        Ok(ws.name)
    }

    /// Активный монитор (с фокусом, иначе первый в списке).
    pub fn active_monitor(&self) -> Result<Monitor> {
        let text = self.request("j/monitors")?;
        let mons: Vec<Monitor> = serde_json::from_str(&text).context("разбор j/monitors")?;
        mons.iter().find(|m| m.focused).or(mons.first()).cloned().context("нет мониторов")
    }

    /// Размер монитора (первого с фокусом, иначе первого в списке).
    pub fn monitor_size(&self) -> Result<(i32, i32)> {
        let m = self.active_monitor()?;
        Ok((m.width, m.height))
    }

    /// Активное окно; `None`, если фокуса нет (пустой стол).
    pub fn active_window(&self) -> Result<Option<Client>> {
        let text = self.request("j/activewindow")?;
        let v: serde_json::Value = serde_json::from_str(&text).context("разбор j/activewindow")?;
        if v.get("address").and_then(|a| a.as_str()).is_none_or(str::is_empty) {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(v).context("разбор j/activewindow")?))
    }

    /// Один диспетчер выражением Lua.
    pub fn dispatch(&self, expr: &str) -> Result<()> {
        let r = self.request(&format!("dispatch {expr}"))?;
        if r.trim() != "ok" {
            bail!("диспетчер отклонён: {} ← {expr}", r.trim());
        }
        Ok(())
    }

    /// Несколько диспетчеров по порядку одним выражением-функцией.
    pub fn dispatch_all(&self, exprs: &[String]) -> Result<()> {
        match exprs {
            [] => Ok(()),
            [one] => self.dispatch(one),
            _ => {
                let (last, head) = exprs.split_last().unwrap();
                let body: String = head.iter().map(|e| format!("hl.dispatch({e}); ")).collect();
                self.dispatch(&format!("(function() {body}return {last} end)()"))
            }
        }
    }

    /// Цикл чтения событий; завершается при обрыве сокета.
    pub fn events(&self, tx: Sender<Event>) -> Result<()> {
        let s = UnixStream::connect(&self.sock2).context("сокет событий Hyprland недоступен")?;
        let reader = BufReader::new(s);
        for line in reader.lines() {
            let line = line?;
            let Some((name, data)) = line.split_once(">>") else { continue };
            let parts: Vec<&str> = data.splitn(4, ',').collect();
            let ev = match name {
                "openwindow" => Event::OpenWindow { addr: norm_addr(parts[0]), workspace: parts.get(1).unwrap_or(&"").to_string() },
                "closewindow" => Event::CloseWindow { addr: norm_addr(parts[0]) },
                "movewindowv2" => Event::MoveWindow { addr: norm_addr(parts[0]), workspace: parts.get(2).unwrap_or(&"").to_string() },
                "workspacev2" => Event::Workspace { name: parts.get(1).unwrap_or(&"").to_string() },
                "activewindowv2" => Event::ActiveWindow { addr: norm_addr(parts[0]) },
                other => Event::Other(other.to_string()),
            };
            if tx.send(ev).is_err() {
                break;
            }
        }
        Ok(())
    }
}

// ---- Выражения диспетчеров ------------------------------------------------

fn win(addr: &str) -> String {
    format!("window = \"address:{addr}\"")
}

pub fn d_move_to(addr: &str, workspace: &str) -> String {
    format!("hl.dsp.window.move({{ workspace = \"{workspace}\", follow = false, {} }})", win(addr))
}

pub fn d_place(addr: &str, r: PxRect) -> [String; 2] {
    [
        format!("hl.dsp.window.resize({{ x = {}, y = {}, relative = false, {} }})", r.w, r.h, win(addr)),
        format!("hl.dsp.window.move({{ x = {}, y = {}, relative = false, {} }})", r.x, r.y, win(addr)),
    ]
}

/// Снять полноэкранный режим композитора с активного окна: `mode` — "fullscreen" (2)
/// или "maximized" (1); `unset` с другим режимом состояние не снимает.
pub fn d_fullscreen_unset(mode: &str) -> String {
    format!("hl.dsp.window.fullscreen({{ mode = \"{mode}\", action = \"unset\" }})")
}

pub fn d_float_on(addr: &str) -> String {
    format!("hl.dsp.window.float({{ action = \"on\", {} }})", win(addr))
}

pub fn d_focus_window(addr: &str) -> String {
    format!("hl.dsp.focus({{ {} }})", win(addr))
}

pub fn d_bring_to_top() -> String {
    "hl.dsp.window.bring_to_top()".to_string()
}

pub fn d_focus_desktop(n: u8) -> String {
    format!("hl.dsp.focus({{ workspace = {n} }})")
}

pub fn d_tag(addr: &str, tag: &str) -> String {
    format!("hl.dsp.window.tag({{ tag = \"{tag}\", {} }})", win(addr))
}

pub fn d_close(addr: &str) -> String {
    format!("hl.dsp.window.close({{ {} }})", win(addr))
}
