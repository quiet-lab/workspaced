//! Конфиг демона: разбор `config.toml`, прямоугольники, окружение, проверка.
//!
//! Формат описан в спецификации ws-config. Здесь только данные и проверки,
//! запись обратно в файл живёт в `save.rs`, генерация привязок — в `keys.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Путь к конфигу: `$XDG_CONFIG_HOME/workspaced/config.toml`.
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("workspaced")
        .join("config.toml")
}

/// Одна координата или размер: пиксели либо доля монитора в процентах.
#[derive(Debug, Clone, PartialEq)]
pub enum Coord {
    Px(i32),
    Percent(f64),
}

impl<'de> Deserialize<'de> for Coord {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(i64),
            Float(f64),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Int(v) => Ok(Coord::Px(v as i32)),
            Raw::Float(v) => Ok(Coord::Px(v.round() as i32)),
            Raw::Str(s) => {
                let t = s.trim();
                let Some(num) = t.strip_suffix('%') else {
                    return Err(serde::de::Error::custom(format!(
                        "ожидалось число пикселей или строка с процентом, получено {s:?}"
                    )));
                };
                let v: f64 = num.trim().parse().map_err(|_| {
                    serde::de::Error::custom(format!("не число в процентах: {s:?}"))
                })?;
                Ok(Coord::Percent(v))
            }
        }
    }
}

impl Coord {
    /// Пиксели по размеру монитора вдоль соответствующей оси.
    pub fn resolve(&self, axis: i32) -> i32 {
        match self {
            Coord::Px(v) => *v,
            Coord::Percent(p) => (axis as f64 * p / 100.0).round() as i32,
        }
    }
}

/// Прямоугольник в конфиге: от левого верхнего угла экрана.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Rect {
    pub x: Coord,
    pub y: Coord,
    pub w: Coord,
    pub h: Coord,
}

/// Прямоугольник в пикселях экрана.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct PxRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn resolve(&self, mon_w: i32, mon_h: i32) -> PxRect {
        PxRect {
            x: self.x.resolve(mon_w),
            y: self.y.resolve(mon_h),
            w: self.w.resolve(mon_w),
            h: self.h.resolve(mon_h),
        }
    }
}

impl PxRect {
    /// Прямоугольник, уменьшенный на зазор с каждой стороны. Нужен командам
    /// `half`, `place` и `maximize`; ячейки и `rect` из конфига не трогает.
    pub fn inset(self, gap: i32) -> PxRect {
        PxRect { x: self.x + gap, y: self.y + gap, w: (self.w - 2 * gap).max(1), h: (self.h - 2 * gap).max(1) }
    }
}

/// Шаблон геометрии: именованные ячейки и главная.
#[derive(Debug, Clone, Deserialize)]
pub struct Template {
    pub main: String,
    #[serde(default)]
    pub cells: BTreeMap<String, Rect>,
}

/// Приложение: команда, аргументы, каталог, окружение, цепочка и, при
/// необходимости, регулярные выражения класса и заголовка для захвата уже
/// открытого окна (спецификация ws-daemon, «Захват открытых окон приложения»).
#[derive(Debug, Clone, Deserialize)]
pub struct App {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub chain: Option<String>,
    /// Класс окна целиком (`^(?:…)$` добавляется демоном).
    #[serde(default)]
    pub class: Option<String>,
    /// Заголовок окна, как написано (например, префикс `^herdr · `).
    #[serde(default)]
    pub title: Option<String>,
}

impl App {
    /// Скомпилированные выражения захвата: класс целиком и заголовок как есть.
    pub fn matchers(&self) -> Result<Option<(regex::Regex, Option<regex::Regex>)>> {
        let Some(class) = &self.class else {
            return Ok(None);
        };
        let class_re = regex::Regex::new(&format!("^(?:{class})$")).with_context(|| format!("поле class {class:?}"))?;
        let title_re = self.title.as_deref().map(|t| regex::Regex::new(t).with_context(|| format!("поле title {t:?}"))).transpose()?;
        Ok(Some((class_re, title_re)))
    }
}

/// Стартовый workspace: поднимается при старте демона после восстановления сессии.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Startup {
    pub workspace: String,
    #[serde(default = "default_desktop")]
    pub desktop: u8,
}

fn default_desktop() -> u8 {
    1
}

/// Место приложения в workspace: ячейка шаблона или свой прямоугольник.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Placement {
    Cell(String),
    Rect { rect: Rect },
}

/// Workspace: шаблон, приложения по ячейкам, главное, иконка, цепочка.
#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub template: String,
    #[serde(default)]
    pub main: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub chain: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub apps: BTreeMap<String, Placement>,
}

/// Служебные цепочки и зарезервированные сочетания.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Keys {
    #[serde(default)]
    pub save_workspace: Option<String>,
    #[serde(default)]
    pub sessions: Option<String>,
    #[serde(default)]
    pub next_workspace: Option<String>,
    /// Сочетания, которые не может занимать ни одна привязка (карта XKB,
    /// намеренно свободные сочетания Openbox).
    #[serde(default)]
    pub reserved: Vec<String>,
}

/// Половина рабочей области: `{ half = "left" }`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HalfAction {
    pub half: String,
}

/// Позиция окна на рабочей области: `{ place = "top-left" }`. Углы и центры
/// рядов — окно в половину ширины и высоты; `center` — половина ширины на всю
/// высоту; `full` — вся рабочая область.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlaceAction {
    pub place: String,
}

/// Допустимые позиции действия `place`.
pub const PLACES: [&str; 8] = ["top-left", "top-center", "top-right", "bottom-left", "bottom-center", "bottom-right", "center", "full"];

/// Стол, workspace и приложение: `{ desktop = 3, workspace = "dots" }`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TargetAction {
    #[serde(default)]
    pub desktop: Option<u8>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub app: Option<String>,
}

/// Действие демона в записи `[[binds]]` (поле `action`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Action {
    Named(String),
    Half(HalfAction),
    Place(PlaceAction),
    Target(TargetAction),
}

/// Диспетчер Hyprland в записи `[[binds]]`: одно выражение или несколько по порядку.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Dispatch {
    One(String),
    Many(Vec<String>),
}

impl Dispatch {
    pub fn exprs(&self) -> Vec<String> {
        match self {
            Dispatch::One(s) => vec![s.clone()],
            Dispatch::Many(v) => v.clone(),
        }
    }
}

/// Запись `[[binds]]`: цепочка, ровно одно действие, флаги и диапазон.
#[derive(Debug, Clone, Deserialize)]
pub struct Bind {
    pub chain: String,
    #[serde(default)]
    pub action: Option<Action>,
    #[serde(default)]
    pub exec: Option<String>,
    #[serde(default)]
    pub dispatch: Option<Dispatch>,
    #[serde(default)]
    pub lua: Option<String>,
    /// `[от, до]`: запись разворачивается в серию с подстановкой `$n`.
    #[serde(default)]
    pub range: Option<(i64, i64)>,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub repeating: bool,
    #[serde(default)]
    pub mouse: bool,
    #[serde(default)]
    pub release: bool,
}

impl Bind {
    /// Имена заданных полей действия (для проверки «ровно одно»).
    pub fn action_fields(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.action.is_some() { v.push("action"); }
        if self.exec.is_some() { v.push("exec"); }
        if self.dispatch.is_some() { v.push("dispatch"); }
        if self.lua.is_some() { v.push("lua"); }
        v
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub keys: Keys,
    /// Зазор для команд `half`, `place` и `maximize`; ячейки шаблонов и `rect`
    /// приложений передаются композитору как записаны.
    #[serde(default)]
    pub gap: i32,
    #[serde(default)]
    pub templates: BTreeMap<String, Template>,
    #[serde(default)]
    pub apps: BTreeMap<String, App>,
    #[serde(default)]
    pub workspaces: BTreeMap<String, Workspace>,
    #[serde(default)]
    pub binds: Vec<Bind>,
    #[serde(default)]
    pub startup: Option<Startup>,
}

/// Имя сущности, безопасное для командной строки и имени тега.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

impl Config {
    /// Прочитать и проверить файл. Неизвестные ключи попадают в журнал предупреждением.
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("не удалось прочитать {}", path.display()))?;
        Config::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Config> {
        let de = toml::de::Deserializer::parse(text).context("ошибка разбора TOML")?;
        let cfg: Config = serde_ignored::deserialize(de, |path| {
            log::warn!("неизвестный ключ конфига: {path}");
        })
        .context("ошибка разбора конфига")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Структурные проверки: имена, ссылки на шаблоны, ячейки и приложения, цепочки.
    pub fn validate(&self) -> Result<()> {
        for name in self.templates.keys().chain(self.apps.keys()).chain(self.workspaces.keys()) {
            if !valid_name(name) {
                bail!("недопустимое имя {name:?}: разрешены буквы, цифры, «_», «-», «.»");
            }
        }
        for (tname, t) in &self.templates {
            if !t.cells.contains_key(&t.main) {
                bail!("шаблон {tname}: главная ячейка {:?} не объявлена в cells", t.main);
            }
        }
        for (aname, a) in &self.apps {
            if a.title.is_some() && a.class.is_none() {
                bail!("приложение {aname}: поле title без class не имеет смысла");
            }
            a.matchers().with_context(|| format!("приложение {aname}"))?;
        }
        if let Some(st) = &self.startup {
            if !self.workspaces.contains_key(&st.workspace) {
                bail!("startup: workspace {:?} не описан в [workspaces]", st.workspace);
            }
            if !(1..=8).contains(&st.desktop) {
                bail!("startup: стол должен быть 1…8, получено {}", st.desktop);
            }
        }
        for (wname, w) in &self.workspaces {
            let Some(t) = self.templates.get(&w.template) else {
                bail!("workspace {wname}: шаблон {:?} не найден", w.template);
            };
            for (aname, place) in &w.apps {
                if !self.apps.contains_key(aname) {
                    bail!("workspace {wname}: приложение {aname:?} не описано в [apps]");
                }
                if let Placement::Cell(cell) = place
                    && !t.cells.contains_key(cell)
                {
                    bail!("workspace {wname}: приложение {aname} ссылается на ячейку {cell:?}, которой нет в шаблоне {}", w.template);
                }
            }
            if let Some(main) = &w.main
                && !w.apps.contains_key(main)
            {
                bail!("workspace {wname}: главное приложение {main:?} не входит в apps");
            }
        }
        for b in &self.binds {
            let fields = b.action_fields();
            if fields.len() != 1 {
                if fields.is_empty() {
                    bail!("привязка {:?}: нужно одно из полей action, exec, dispatch, lua", b.chain);
                }
                bail!("привязка {:?}: задано несколько действий ({}), допустимо одно", b.chain, fields.join(", "));
            }
            if let Some((from, to)) = b.range {
                if from > to {
                    bail!("привязка {:?}: range = [{from}, {to}] — начало больше конца", b.chain);
                }
                if to - from > 100 {
                    bail!("привязка {:?}: range = [{from}, {to}] слишком велик (не более 100 значений)", b.chain);
                }
                if !b.chain.contains("$n") {
                    bail!("привязка {:?}: range задан, но в chain нет подстановки $n", b.chain);
                }
            }
            if let Some(Dispatch::Many(v)) = &b.dispatch
                && v.is_empty()
            {
                bail!("привязка {:?}: пустой список dispatch", b.chain);
            }
            match &b.action {
                Some(Action::Named(n)) if !matches!(n.as_str(), "sessions" | "save-workspace" | "next-workspace" | "maximize") => {
                    bail!("привязка {:?}: неизвестное действие {n:?}", b.chain)
                }
                Some(Action::Half(HalfAction { half })) if !matches!(half.as_str(), "left" | "right" | "up" | "down") => {
                    bail!("привязка {:?}: half должен быть left, right, up или down, получено {half:?}", b.chain)
                }
                Some(Action::Place(PlaceAction { place })) if !PLACES.contains(&place.as_str()) => {
                    bail!("привязка {:?}: place должен быть одним из {}, получено {place:?}", b.chain, PLACES.join(", "))
                }
                Some(Action::Target(TargetAction { desktop, workspace, app })) => {
                    if workspace.is_none() && app.is_none() {
                        bail!("привязка {:?}: в action нужен workspace или app", b.chain);
                    }
                    if let Some(d) = desktop
                        && !(1..=8).contains(d)
                    {
                        bail!("привязка {:?}: desktop должен быть от 1 до 8", b.chain);
                    }
                    if let Some(w) = workspace
                        && !self.workspaces.contains_key(w)
                    {
                        bail!("привязка {:?}: workspace {w:?} не найден", b.chain);
                    }
                    if let Some(a) = app
                        && !self.apps.contains_key(a)
                    {
                        bail!("привязка {:?}: приложение {a:?} не найдено", b.chain);
                    }
                }
                _ => {}
            }
        }
        crate::keys::check_chains(self)?;
        Ok(())
    }

    /// Сложенное окружение приложения в контексте workspace (три слоя, с подстановкой).
    pub fn app_env(&self, workspace: Option<&Workspace>, app: &App) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        for layer in [Some(&self.env), workspace.map(|w| &w.env), Some(&app.env)].into_iter().flatten() {
            for (k, v) in layer {
                let expanded = expand(v, &env);
                env.insert(k.clone(), expanded);
            }
        }
        env
    }

    /// Команда, аргументы и каталог приложения с подстановкой из сложенного окружения.
    pub fn app_command(&self, workspace: Option<&Workspace>, app: &App) -> (String, Vec<String>, Option<String>, BTreeMap<String, String>) {
        let env = self.app_env(workspace, app);
        let cmd = expand(&app.cmd, &env);
        let args = app.args.iter().map(|a| expand(a, &env)).collect();
        let cwd = app.cwd.as_ref().map(|c| expand(c, &env));
        (cmd, args, cwd, env)
    }
}

/// Подстановка `~` в начале строки и `$ИМЯ` / `${ИМЯ}` из сложенного окружения,
/// затем из окружения демона, иначе пустая строка.
pub fn expand(s: &str, env: &BTreeMap<String, String>) -> String {
    let lookup = |name: &str| -> String {
        env.get(name).cloned().or_else(|| std::env::var(name).ok()).unwrap_or_default()
    };
    let mut src: &str = s;
    let mut out = String::with_capacity(s.len());
    if let Some(rest) = s.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/'))
    {
        out.push_str(&lookup("HOME"));
        src = rest;
    }
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '$' && i + 1 < chars.len() {
            if chars[i + 1] == '{' {
                if let Some(end) = chars[i + 2..].iter().position(|&x| x == '}') {
                    let name: String = chars[i + 2..i + 2 + end].iter().collect();
                    out.push_str(&lookup(&name));
                    i += end + 3;
                    continue;
                }
            } else if chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '_' {
                let mut j = i + 1;
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let name: String = chars[i + 1..j].iter().collect();
                out.push_str(&lookup(&name));
                i = j;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[templates.halves]
main = "right"
[templates.halves.cells]
left  = { x = 330, y = 0, w = "50%", h = "100%" }
right = { x = "50%", y = 0, w = "50%", h = "100%" }

[apps.terminal]
cmd = "wezterm-gui"

[workspaces.work]
template = "halves"
main = "terminal"
apps = { terminal = "right" }
"#;

    #[test]
    fn minimal_config() {
        let cfg = Config::parse(MINIMAL).unwrap();
        assert_eq!(cfg.templates.len(), 1);
        assert_eq!(cfg.apps.len(), 1);
        assert_eq!(cfg.workspaces.len(), 1);
    }

    #[test]
    fn named_actions_in_binds() {
        // Служебные действия строкой: известное принимается, опечатка отклоняется с указанием привязки.
        let ok = format!("{MINIMAL}\n[[binds]]\nchain = \"SUPER+X\"\naction = \"maximize\"\n");
        Config::parse(&ok).unwrap();
        let bad = format!("{MINIMAL}\n[[binds]]\nchain = \"SUPER+X\"\naction = \"maximise\"\n");
        let err = Config::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("SUPER+X") && err.contains("maximise"), "{err}");
    }

    #[test]
    fn place_action_in_binds() {
        let ok = format!("{MINIMAL}\n[[binds]]\nchain = \"ALT+SUPER+Home\"\naction = {{ place = \"top-left\" }}\n");
        let cfg = Config::parse(&ok).unwrap();
        assert_eq!(cfg.binds[0].action, Some(Action::Place(PlaceAction { place: "top-left".into() })));
        let bad = format!("{MINIMAL}\n[[binds]]\nchain = \"ALT+SUPER+Home\"\naction = {{ place = \"left\" }}\n");
        let err = Config::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("Home") && err.contains("top-left") && err.contains("full"), "{err}");
    }

    #[test]
    fn class_title_and_startup() {
        let ok = format!("{MINIMAL}\n[apps.terminal]\nclass = \"^org\\\\.wezfurlong\\\\.wezterm$\"\ntitle = \"^herdr · \"\n[startup]\nworkspace = \"work\"\n");
        let ok = ok
            .replace(
                "[apps.terminal]\ncmd = \"wezterm-gui\"\n",
                "[apps.terminal]\ncmd = \"wezterm-gui\"\nclass = \"^org\\\\.wezfurlong\\\\.wezterm$\"\ntitle = \"^herdr · \"\n",
            )
            .replace("\n[apps.terminal]\nclass", "\n[apps.unused]\ncmd = \"x\"\nclass");
        let cfg = Config::parse(&ok).unwrap();
        assert_eq!(cfg.startup.as_ref().map(|s| (s.workspace.as_str(), s.desktop)), Some(("work", 1)));
        let (class_re, title_re) = cfg.apps["terminal"].matchers().unwrap().unwrap();
        assert!(class_re.is_match("org.wezfurlong.wezterm") && !class_re.is_match("org.wezfurlong.wezterm2"));
        assert!(title_re.unwrap().is_match("herdr · dev-lab"));
        let title_only = format!("{MINIMAL}\n[apps.a]\ncmd = \"a\"\ntitle = \"x\"\n");
        assert!(Config::parse(&title_only).unwrap_err().to_string().contains("приложение a"));
        let bad_re = format!("{MINIMAL}\n[apps.a]\ncmd = \"a\"\nclass = \"(\"\n");
        assert!(format!("{:#}", Config::parse(&bad_re).unwrap_err()).contains("class"));
        let bad_ws = format!("{MINIMAL}\n[startup]\nworkspace = \"nope\"\n");
        assert!(Config::parse(&bad_ws).unwrap_err().to_string().contains("nope"));
        let bad_desk = format!("{MINIMAL}\n[startup]\nworkspace = \"work\"\ndesktop = 9\n");
        assert!(Config::parse(&bad_desk).unwrap_err().to_string().contains("1…8"));
    }

    #[test]
    fn percent_and_pixels_in_one_cell() {
        let cfg = Config::parse(MINIMAL).unwrap();
        let cell = &cfg.templates["halves"].cells["left"];
        assert_eq!(cell.resolve(3840, 2160), PxRect { x: 330, y: 0, w: 1920, h: 2160 });
    }

    #[test]
    fn negative_and_oversized_rect() {
        // Окно может выходить за края экрана: значения не обрезаются и не сдвигаются.
        let cfg = Config::parse(&MINIMAL.replace(
            "left  = { x = 330, y = 0, w = \"50%\", h = \"100%\" }",
            "left  = { x = -400, y = \"-10%\", w = 5000, h = \"120%\" }",
        ))
        .unwrap();
        let r = cfg.templates["halves"].cells["left"].resolve(3840, 2160);
        assert_eq!(r, PxRect { x: -400, y: -216, w: 5000, h: 2592 });
        assert_eq!(r.inset(5), PxRect { x: -395, y: -211, w: 4990, h: 2582 });
    }

    #[test]
    fn unknown_cell_is_error() {
        let text = MINIMAL.replace("apps = { terminal = \"right\" }", "apps = { terminal = \"top\" }");
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("work") && err.contains("terminal") && err.contains("top"), "{err}");
    }

    #[test]
    fn env_layers_and_substitution() {
        let text = r#"
[env]
NODE_ENV = "production"
[templates.t]
main = "c"
cells = { c = { x = 0, y = 0, w = 10, h = 10 } }
[apps.term]
cmd = "wezterm-gui"
args = ["--cwd", "$ROOT"]
env = { NODE_ENV = "test" }
[apps.plain]
cmd = "true"
[workspaces.w]
template = "t"
env = { ROOT = "~/work/front-end", NODE_ENV = "development" }
apps = { term = "c", plain = "c" }
"#;
        let cfg = Config::parse(text).unwrap();
        let ws = &cfg.workspaces["w"];
        let home = std::env::var("HOME").unwrap();
        let (_, args, _, env) = cfg.app_command(Some(ws), &cfg.apps["term"]);
        assert_eq!(args, vec!["--cwd".to_string(), format!("{home}/work/front-end")]);
        assert_eq!(env["ROOT"], format!("{home}/work/front-end"));
        assert_eq!(env["NODE_ENV"], "test");
        let env2 = cfg.app_env(Some(ws), &cfg.apps["plain"]);
        assert_eq!(env2["NODE_ENV"], "development");
        let env3 = cfg.app_env(None, &cfg.apps["plain"]);
        assert_eq!(env3["NODE_ENV"], "production");
    }

    #[test]
    fn expand_forms() {
        let mut env = BTreeMap::new();
        env.insert("A".to_string(), "x".to_string());
        assert_eq!(expand("$A/${A}/$NOPE_VAR_Q/$", &env), "x/x//$");
        assert_eq!(expand("~", &env), std::env::var("HOME").unwrap());
        assert_eq!(expand("a~b", &env), "a~b");
    }
}
