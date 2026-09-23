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
    /// Номер окна у композитора: чем он меньше, тем раньше окно появилось.
    #[serde(default, rename = "stableId")]
    pub stable_id: String,
}

/// Разбор тега окна `app:<имя>#<номер>`: имя приложения и номер экземпляра.
/// Тег `app:<имя>` без номера остался от прежней версии демона и читается как
/// экземпляр без номера; звёздочку в конце добавляет правило композитора.
pub fn parse_app_tag(tag: &str) -> Option<(String, Option<u32>)> {
    let rest = tag.strip_prefix("app:")?.trim_end_matches('*');
    if rest.is_empty() {
        return None;
    }
    match rest.rsplit_once('#') {
        Some((name, num)) if !name.is_empty() && num.parse::<u32>().is_ok() => Some((name.to_string(), num.parse().ok())),
        _ => Some((rest.to_string(), None)),
    }
}

/// Разбор тега состава `ws:<имя>`: имя workspace, в который входит окно
/// (изменение shared-windows, решение D1). Звёздочку в конце добавляет
/// правило композитора, она к имени не относится.
pub fn parse_ws_tag(tag: &str) -> Option<String> {
    let rest = tag.strip_prefix("ws:")?.trim_end_matches('*');
    (!rest.is_empty()).then(|| rest.to_string())
}

/// Тег состава workspace `ws`.
pub fn ws_tag(ws: &str) -> String {
    format!("ws:{ws}")
}

impl Client {
    /// Workspace, в которые входит окно, по тегам состава; по имени, без повторов.
    pub fn workspaces(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tags.iter().filter_map(|t| parse_ws_tag(t)).collect();
        v.sort();
        v.dedup();
        v
    }
    /// Входит ли окно в workspace `ws`.
    pub fn in_ws(&self, ws: &str) -> bool {
        self.tags.iter().any(|t| parse_ws_tag(t).as_deref() == Some(ws))
    }
    /// Входит ли окно хотя бы в один workspace.
    pub fn has_ws(&self) -> bool {
        self.tags.iter().any(|t| parse_ws_tag(t).is_some())
    }
    /// Имя приложения из тега экземпляра.
    pub fn app(&self) -> Option<String> {
        self.app_instance().map(|(a, _)| a)
    }
    /// Приложение и номер экземпляра; у тега прежней версии без номера — 1.
    pub fn app_instance(&self) -> Option<(String, u32)> {
        self.tags.iter().find_map(|t| parse_app_tag(t)).map(|(a, n)| (a, n.unwrap_or(1)))
    }
    /// Теги приложения, как они записаны у окна (нужны, чтобы снять прежний тег).
    pub fn app_tags(&self) -> impl Iterator<Item = &str> {
        self.tags.iter().filter(|t| t.starts_with("app:")).map(|t| t.trim_end_matches('*'))
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
    /// Порядок появления окна: номер `stableId` композитора. Окно без номера
    /// идёт последним, поэтому порядок остальных не меняется.
    pub fn stable(&self) -> u64 {
        self.stable_id.parse().unwrap_or(u64::MAX)
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

/// Поднять наверх стопки названное окно, не меняя фокус. У `bring_to_top` окно
/// не выбирается — он действует на активное, — поэтому порядок окон по глубине
/// задаётся `alter_zorder` с явным адресом.
pub fn d_raise(addr: &str) -> String {
    format!("hl.dsp.window.alter_zorder({{ mode = \"top\", {} }})", win(addr))
}

pub fn d_focus_desktop(n: u8) -> String {
    format!("hl.dsp.focus({{ workspace = {n} }})")
}

pub fn d_tag(addr: &str, tag: &str) -> String {
    format!("hl.dsp.window.tag({{ tag = \"{tag}\", {} }})", win(addr))
}

/// Снять тег с окна. У диспетчера `tagwindow` префикс «-» снимает тег, «+»
/// ставит, а без префикса тег переключается, поэтому снятие пишется явно.
pub fn d_untag(addr: &str, tag: &str) -> String {
    d_tag(addr, &format!("-{tag}"))
}

pub fn d_close(addr: &str) -> String {
    format!("hl.dsp.window.close({{ {} }})", win(addr))
}

/// Окно для модульных тестов: только те поля, которые читает демон.
#[cfg(test)]
pub fn test_client(addr: &str, class: &str, title: &str, ws: &str, tags: &[&str]) -> Client {
    serde_json::from_value(serde_json::json!({
        "address": addr, "class": class, "title": title, "workspace": { "id": 1, "name": ws },
        "tags": tags, "at": [0, 0], "size": [10, 10], "floating": true, "mapped": true
    }))
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_tag_forms() {
        assert_eq!(parse_app_tag("app:chromium#2"), Some(("chromium".to_string(), Some(2))));
        // Тег прежней версии без номера и тег правила композитора со звёздочкой.
        assert_eq!(parse_app_tag("app:neovide"), Some(("neovide".to_string(), None)));
        assert_eq!(parse_app_tag("app:neovide#1*"), Some(("neovide".to_string(), Some(1))));
        // Чужой тег и мусор после «#» приложением не считаются.
        assert_eq!(parse_app_tag("pin:1"), None);
        assert_eq!(parse_app_tag("app:"), None);
        assert_eq!(parse_app_tag("app:x#y"), Some(("x#y".to_string(), None)));

        let c = test_client("0x1", "chromium", "Новости", "1", &["app:chromium#3"]);
        assert_eq!(c.app_instance(), Some(("chromium".to_string(), 3)));
        assert_eq!(c.app().as_deref(), Some("chromium"));
        assert_eq!(c.app_tags().collect::<Vec<_>>(), vec!["app:chromium#3"]);
        // Тег без номера — экземпляр 1.
        let old = test_client("0x2", "neovide", "[Scratch]", "1", &["app:neovide"]);
        assert_eq!(old.app_instance(), Some(("neovide".to_string(), 1)));
    }

    #[test]
    fn ws_tag_forms() {
        assert_eq!(parse_ws_tag("ws:work"), Some("work".to_string()));
        // Звёздочку добавляет правило композитора.
        assert_eq!(parse_ws_tag("ws:surf*"), Some("surf".to_string()));
        assert_eq!(parse_ws_tag("ws:"), None);
        assert_eq!(parse_ws_tag("app:chromium#1"), None);
        assert_eq!(ws_tag("work"), "ws:work");

        let c = test_client("0x1", "google-chrome-ai", "ИИ", "2", &["app:chrome-ai#1", "ws:work*", "ws:surf"]);
        // Список по имени: так его показывает `workspaced status --json`.
        assert_eq!(c.workspaces(), vec!["surf".to_string(), "work".to_string()]);
        assert!(c.in_ws("work") && c.in_ws("surf") && !c.in_ws("chat"));
        assert!(c.has_ws());
        // Тег состава не мешает разбору тега экземпляра.
        assert_eq!(c.app_instance(), Some(("chrome-ai".to_string(), 1)));
        let free = test_client("0x2", "chromium", "Новости", "4", &["app:chromium#2"]);
        assert!(free.workspaces().is_empty() && !free.has_ws());
    }
}
