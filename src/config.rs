//! Конфиг демона: разбор `config.toml`, прямоугольники, окружение, проверка.
//!
//! Формат описан в спецификации ws-config. Здесь только данные и проверки,
//! запись обратно в файл живёт в `save.rs`, генерация привязок — в `keys.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
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

    /// Тот же прямоугольник в форме конфига: все четыре числа в пикселях.
    pub fn to_rect(self) -> Rect {
        Rect { x: Coord::Px(self.x), y: Coord::Px(self.y), w: Coord::Px(self.w), h: Coord::Px(self.h) }
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
/// Приложение-вариант называет своё семейство полем `family`; приложение без
/// `cmd` окон не открывает, а только собирает подходящие по `class`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct App {
    #[serde(default)]
    pub cmd: Option<String>,
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
    /// Имя приложения-семейства, если это приложение — его вариант.
    #[serde(default)]
    pub family: Option<String>,
    /// Положение и размер окна, когда активный workspace об этом приложении
    /// ничего не говорит.
    #[serde(default)]
    pub rect: Option<Rect>,
    /// Заголовок диалога: окно приложения, чей заголовок подходит этому
    /// выражению, в workspace не принимается и остаётся свободным там, где
    /// его поставил композитор. Нужно потому, что собственный диалог
    /// приложения приходит с тем же классом, а родителя и модальности
    /// композитор не сообщает.
    #[serde(default)]
    pub dialog_title: Option<String>,
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

    /// Скомпилированное выражение заголовка диалога, если оно задано.
    pub fn dialog_matcher(&self) -> Result<Option<regex::Regex>> {
        self.dialog_title
            .as_deref()
            .map(|t| regex::Regex::new(t).with_context(|| format!("поле dialog_title {t:?}")))
            .transpose()
    }

    /// Окно приложения — его диалог: заголовок подходит `dialog_title`.
    /// Приложение без `dialog_title` диалогов не различает.
    pub fn is_dialog(&self, title: &str) -> bool {
        matches!(self.dialog_matcher(), Ok(Some(re)) if re.is_match(title))
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

/// Запись приложения в таблице `apps` workspace (изменение
/// workspace-overrides, решение D1): место — ячейка шаблона или свой
/// прямоугольник — и необязательные переопределения клавиши (`chain`)
/// и режима клавиши (`mode`) этого приложения в этом workspace. В файле
/// запись бывает строкой (имя ячейки), таблицей `{ rect = … }` или таблицей
/// с полем `cell` либо `rect` и полями `chain`, `mode`.
#[derive(Debug, Clone, PartialEq)]
pub struct WsApp {
    pub place: Placement,
    pub chain: Option<String>,
    pub mode: Option<String>,
    /// Ошибка места в записи-таблице: нет ни `cell`, ни `rect`, либо заданы
    /// оба. Разбор её не отклоняет, чтобы проверка конфига назвала
    /// workspace и приложение (решение D8).
    pub place_error: Option<&'static str>,
}

impl WsApp {
    /// Запись только о месте: так в эффективный конфиг входят записи
    /// дополнительных приложений сессии (решение D11).
    pub fn at(place: Placement) -> WsApp {
        WsApp { place, chain: None, mode: None, place_error: None }
    }

    /// Место задано ячейкой шаблона.
    pub fn has_cell(&self) -> bool {
        matches!(self.place, Placement::Cell(_))
    }
}

impl<'de> Deserialize<'de> for WsApp {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = WsApp;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("имя ячейки строкой или таблица с cell либо rect и необязательными chain, mode")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<WsApp, E> {
                Ok(WsApp::at(Placement::Cell(v.to_string())))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<WsApp, A::Error> {
                let (mut cell, mut rect, mut chain, mut mode) = (None::<String>, None::<Rect>, None::<String>, None::<String>);
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "cell" => cell = Some(map.next_value()?),
                        "rect" => rect = Some(map.next_value()?),
                        "chain" => chain = Some(map.next_value()?),
                        "mode" => mode = Some(map.next_value()?),
                        // Неизвестный ключ пропускается: обёртка serde_ignored
                        // видит пропуск и пишет предупреждение с путём ключа.
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                let (place, place_error) = match (cell, rect) {
                    (Some(c), None) => (Placement::Cell(c), None),
                    (None, Some(r)) => (Placement::Rect { rect: r }, None),
                    (None, None) => (Placement::Cell(String::new()), Some("нужно место: поле cell или rect")),
                    (Some(c), Some(_)) => (Placement::Cell(c), Some("заданы оба поля cell и rect, допустимо одно")),
                };
                Ok(WsApp { place, chain, mode, place_error })
            }
        }
        d.deserialize_any(V)
    }
}

/// Поведение клавиши приложения в workspace (спецификация ws-daemon,
/// «Цепочка приложения»): обмен ячеек с главной или подъём окна на месте.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Окна приложения занимают главную ячейку, прежнее главное уходит в их.
    Swap,
    /// Окна стоят на своих местах, меняется только порядок по глубине и фокус.
    Stack,
}

/// Допустимые значения поля `mode` у workspace и у записи приложения в нём.
pub const MODES: [&str; 2] = ["swap", "stack"];

fn default_mode() -> String {
    MODES[0].to_string()
}

/// Workspace: шаблон, приложения по ячейкам, главное, иконка, цепочка.
/// Сравнение на равенство нужно перечитыванию конфига: у workspace, чей раздел
/// изменился, назначения ячеек собираются заново (спецификация ws-config,
/// «Слежение за конфигом»).
#[derive(Debug, Clone, PartialEq, Deserialize)]
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
    /// Поведение клавиши приложения: `swap` (по умолчанию) или `stack`.
    /// Значение проверяется при чтении конфига, поэтому дальше читается как есть.
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Порядок записей сохраняется таким, как в файле: по нему конец цикла
    /// клавиши выбирает следующее приложение workspace.
    #[serde(default)]
    pub apps: IndexMap<String, WsApp>,
}

impl Mode {
    /// Режим по значению поля `mode`; значение проверено при чтении конфига.
    pub fn of(name: &str) -> Mode {
        if name == "stack" { Mode::Stack } else { Mode::Swap }
    }
}

impl Workspace {
    pub fn mode(&self) -> Mode {
        Mode::of(&self.mode)
    }
}

/// Служебные цепочки и зарезервированные сочетания.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Keys {
    /// Сохранить сессию: посторонние окна стола входят в активный workspace,
    /// файл конфига не меняется.
    #[serde(default)]
    pub save_session: Option<String>,
    /// Записать активный workspace в этот файл.
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

/// Номер стола в действии: число (`{ move = 3 }`) или строка — в записи
/// с `range` там стоит подстановка `{ move = "$n" }`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Desk {
    Num(i64),
    Str(String),
}

impl Desk {
    pub fn text(&self) -> String {
        match self {
            Desk::Num(n) => n.to_string(),
            Desk::Str(s) => s.clone(),
        }
    }
    /// Номер стола 1…8; `None` — не число или вне диапазона.
    pub fn number(&self) -> Option<u8> {
        self.text().trim().parse::<u8>().ok().filter(|n| (1..=8).contains(n))
    }
}

/// Перенос активного workspace текущего стола на другой стол:
/// `{ move = 3 }` либо `{ move = "$n" }` в записи с `range`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MoveAction {
    #[serde(rename = "move")]
    pub desktop: Desk,
}

/// Стол, workspace и приложение: `{ desktop = 3, workspace = "dots" }`.
/// Ключ `pull` у действия приложения перетаскивает окна приложения
/// в активный workspace текущего стола (изменение shared-windows, решение D8):
/// `{ app = "chrome-ai", pull = true }`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TargetAction {
    #[serde(default)]
    pub desktop: Option<u8>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub pull: Option<bool>,
}

/// Действие демона в записи `[[binds]]` (поле `action`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Action {
    Named(String),
    Half(HalfAction),
    Place(PlaceAction),
    Move(MoveAction),
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
    /// Классы окон, которые никогда не принимаются в workspace: выражения
    /// сопоставляются с классом целиком. Сюда записываются диалоги, приходящие
    /// отдельным классом (порталы выбора файла, запрос пароля).
    #[serde(default)]
    pub ignore_classes: Vec<String>,
    #[serde(default)]
    pub workspaces: BTreeMap<String, Workspace>,
    #[serde(default)]
    pub binds: Vec<Bind>,
    #[serde(default)]
    pub startup: Option<Startup>,
}

/// Запуск приложения: программа, аргументы, рабочий каталог и сложенное окружение.
pub type AppCommand = (String, Vec<String>, Option<String>, BTreeMap<String, String>);

/// Имя сущности, безопасное для командной строки и имени тега. Двоеточие и `#`
/// недопустимы: ими демон отделяет части тега экземпляра `app:<имя>#<номер>`.
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
                bail!("недопустимое имя {name:?}: разрешены буквы, цифры, «_», «-», «.»; двоеточие и «#» отделяют части тега экземпляра");
            }
        }
        for (tname, t) in &self.templates {
            if !t.cells.contains_key(&t.main) {
                bail!("шаблон {tname}: главная ячейка {:?} не объявлена в cells", t.main);
            }
        }
        for (aname, a) in &self.apps {
            if a.cmd.is_none() && a.class.is_none() {
                bail!("приложение {aname}: нужно хотя бы одно из полей cmd и class: без cmd приложение не открывает окон, без class его окно не узнать");
            }
            if a.title.is_some() && a.class.is_none() {
                bail!("приложение {aname}: поле title без class не имеет смысла");
            }
            if a.dialog_title.is_some() && a.class.is_none() {
                bail!("приложение {aname}: поле dialog_title без class не имеет смысла: окно приложения узнаётся по class");
            }
            a.matchers().with_context(|| format!("приложение {aname}"))?;
            a.dialog_matcher().with_context(|| format!("приложение {aname}"))?;
            // Уровней ровно два: семейство и его варианты.
            if let Some(f) = &a.family {
                if f == aname {
                    bail!("приложение {aname}: поле family называет само приложение");
                }
                let Some(parent) = self.apps.get(f) else {
                    bail!("приложение {aname}: семейство {f:?} не описано в [apps]");
                };
                if parent.family.is_some() {
                    bail!("приложение {aname}: семейство {f:?} само вариант семейства {:?}, а уровней ровно два", parent.family.as_deref().unwrap_or(""));
                }
            }
        }
        for c in &self.ignore_classes {
            regex::Regex::new(&format!("^(?:{c})$")).with_context(|| format!("ignore_classes: выражение {c:?}"))?;
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
            if !MODES.contains(&w.mode.as_str()) {
                bail!("workspace {wname}: mode должен быть одним из {}, получено {:?}", MODES.join(", "), w.mode);
            }
            for (aname, entry) in &w.apps {
                if !self.apps.contains_key(aname) {
                    bail!("workspace {wname}: приложение {aname:?} не описано в [apps]");
                }
                if let Some(e) = entry.place_error {
                    bail!("workspace {wname}: запись приложения {aname}: {e}");
                }
                if let Some(m) = &entry.mode
                    && !MODES.contains(&m.as_str())
                {
                    bail!("workspace {wname}: приложение {aname}: mode должен быть одним из {}, получено {m:?}", MODES.join(", "));
                }
                if let Placement::Cell(cell) = &entry.place
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
                Some(Action::Named(n)) if !matches!(n.as_str(), "sessions" | "save-session" | "save-workspace" | "next-workspace" | "maximize" | "arrange" | "detach") => {
                    bail!("привязка {:?}: неизвестное действие {n:?}", b.chain)
                }
                Some(Action::Half(HalfAction { half })) if !matches!(half.as_str(), "left" | "right" | "up" | "down") => {
                    bail!("привязка {:?}: half должен быть left, right, up или down, получено {half:?}", b.chain)
                }
                Some(Action::Place(PlaceAction { place })) if !PLACES.contains(&place.as_str()) => {
                    bail!("привязка {:?}: place должен быть одним из {}, получено {place:?}", b.chain, PLACES.join(", "))
                }
                Some(Action::Target(TargetAction { desktop, workspace, app, pull })) => {
                    // Перетаскивание идёт в активный workspace текущего стола,
                    // поэтому ключ pull имеет смысл только у действия приложения
                    // без workspace.
                    if pull.is_some() && (app.is_none() || workspace.is_some()) {
                        bail!("привязка {:?}: ключ pull допустим только вместе с app и без workspace", b.chain);
                    }
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

    /// Команда, аргументы и каталог приложения с подстановкой из сложенного
    /// окружения; `None` у приложения без `cmd` — оно окон не открывает.
    pub fn app_command(&self, workspace: Option<&Workspace>, app: &App) -> Option<AppCommand> {
        let env = self.app_env(workspace, app);
        let cmd = expand(app.cmd.as_ref()?, &env);
        let args = app.args.iter().map(|a| expand(a, &env)).collect();
        let cwd = app.cwd.as_ref().map(|c| expand(c, &env));
        Some((cmd, args, cwd, env))
    }

    /// Семейство приложения: имя из поля `family`, если приложение — вариант.
    pub fn family_of(&self, app: &str) -> Option<&str> {
        self.apps.get(app)?.family.as_deref()
    }

    /// Окно приложения `name` принадлежит `target`: это само приложение либо
    /// его семейство (окно варианта — окно своего семейства).
    pub fn app_is(&self, name: &str, target: &str) -> bool {
        name == target || self.family_of(name) == Some(target)
    }

    /// Класс окна попадает в список `ignore_classes`: такое окно ни при каких
    /// условиях не принимается в workspace. Выражение с ошибкой пропускается
    /// с предупреждением: конфиг проверен при чтении, но эффективный конфиг
    /// собирается и из записей сессии.
    pub fn ignored_class(&self, class: &str) -> bool {
        self.ignore_classes.iter().any(|c| match regex::Regex::new(&format!("^(?:{c})$")) {
            Ok(re) => re.is_match(class),
            Err(e) => {
                log::warn!("ignore_classes: выражение {c:?} не разобрано: {e}");
                false
            }
        })
    }

    /// Запись, описывающая приложение в workspace (изменение
    /// workspace-overrides, решение D6): запись самого приложения, а без неё —
    /// запись его семейства. Возвращает имя записи и её саму.
    pub fn ws_entry(&self, ws: &str, app: &str) -> Option<(&str, &WsApp)> {
        let w = self.workspaces.get(ws)?;
        if let Some((k, e)) = w.apps.get_key_value(app) {
            return Some((k.as_str(), e));
        }
        let f = self.family_of(app)?;
        w.apps.get_key_value(f).map(|(k, e)| (k.as_str(), e))
    }

    /// Ключ приложения в workspace (решение D2): `chain` записи самого
    /// приложения в этом workspace, а без него — собственный `chain`
    /// приложения.
    pub fn app_key(&self, ws: &str, app: &str) -> Option<&str> {
        self.workspaces
            .get(ws)
            .and_then(|w| w.apps.get(app))
            .and_then(|e| e.chain.as_deref())
            .or_else(|| self.apps.get(app)?.chain.as_deref())
    }

    /// Workspace переопределяет клавишу приложения: у записи самого
    /// приложения есть `chain`, и он не совпадает с собственной клавишей.
    /// Переопределение, совпадающее с собственной клавишей, ничего не меняет
    /// (решение D2); приложение, описанное только через семейство, своей
    /// записи не имеет и клавишу не переопределяет.
    pub fn overrides_key(&self, ws: &str, app: &str) -> bool {
        let Some(c) = self.workspaces.get(ws).and_then(|w| w.apps.get(app)).and_then(|e| e.chain.as_deref()) else {
            return false;
        };
        match self.apps.get(app).and_then(|a| a.chain.as_deref()) {
            Some(own) => !crate::keys::same_chain(c, own),
            None => true,
        }
    }

    /// Режим клавиши приложения в workspace (решение D7): поле `mode` записи,
    /// описывающей приложение; без него — режим workspace, кроме приложения
    /// без ячейки в workspace режима `swap`, которое выбирается в режиме
    /// `stack`. Приложение без записи в workspace берёт режим workspace.
    pub fn app_mode(&self, ws: &str, app: &str) -> Mode {
        let ws_mode = self.workspaces.get(ws).map(Workspace::mode).unwrap_or(Mode::Swap);
        match self.ws_entry(ws, app) {
            Some((_, e)) => match &e.mode {
                Some(m) => Mode::of(m),
                None if ws_mode == Mode::Swap && !e.has_cell() => Mode::Stack,
                None => ws_mode,
            },
            None => ws_mode,
        }
    }

    /// Приложение, которым workspace откликается на цепочку: запись, ключ
    /// которой (`app_key`) совпадает с цепочкой. При двух откликах —
    /// запись, стоящая раньше в разделе (решение D8): записи файла идут
    /// раньше записей сессии.
    pub fn responds(&self, ws: &str, chain: &str) -> Option<String> {
        let w = self.workspaces.get(ws)?;
        w.apps.keys().find(|a| self.app_key(ws, a).is_some_and(|k| crate::keys::same_chain(k, chain))).cloned()
    }

    /// Варианты семейства по именам.
    pub fn variants_of(&self, family: &str) -> Vec<&str> {
        self.apps.iter().filter(|(_, a)| a.family.as_deref() == Some(family)).map(|(n, _)| n.as_str()).collect()
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
    fn workspace_mode_and_app_order() {
        // Без поля mode workspace работает обменом ячеек.
        let cfg = Config::parse(MINIMAL).unwrap();
        assert_eq!(cfg.workspaces["work"].mode(), Mode::Swap);

        // Порядок приложений в разделе workspace — как в файле, а не по алфавиту.
        let text = MINIMAL.replace(
            "[apps.terminal]\ncmd = \"wezterm-gui\"\n",
            "[apps.terminal]\ncmd = \"wezterm-gui\"\n[apps.browser]\ncmd = \"chromium\"\n[apps.editor]\ncmd = \"neovide\"\n",
        )
        .replace(
            "apps = { terminal = \"right\" }",
            "mode = \"stack\"\napps = { terminal = \"right\", editor = \"left\", browser = \"left\" }",
        );
        let cfg = Config::parse(&text).unwrap();
        assert_eq!(cfg.workspaces["work"].mode(), Mode::Stack);
        assert_eq!(cfg.workspaces["work"].apps.keys().map(String::as_str).collect::<Vec<_>>(), vec!["terminal", "editor", "browser"]);

        // Опечатка в режиме отклоняется с указанием workspace и допустимых значений.
        let bad = text.replace("mode = \"stack\"", "mode = \"stak\"");
        let err = Config::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("work") && err.contains("stak") && err.contains("swap") && err.contains("stack"), "{err}");
    }

    /// Приложения браузеров и workspace для проверок записи приложения
    /// в workspace (изменение workspace-overrides).
    const OVERRIDES: &str = r#"
[templates.overlay]
main = "center"
[templates.overlay.cells]
left = { x = 340, y = 10, w = 1920, h = 2140 }
center = { x = 1125, y = 10, w = 1920, h = 2140 }
right = { x = 1910, y = 10, w = 1920, h = 2140 }

[apps.chrome]
cmd = "chrome"
chain = "SUPER+SHIFT+B"
[apps.chrome-ai]
cmd = "chrome"
chain = "SUPER+SHIFT+V"
[apps.yandex]
cmd = "yandex"
chain = "SUPER+SHIFT+Y"
[apps.calc]
cmd = "galculator"
[apps.chromium]
cmd = "chromium"
chain = "SUPER+C"

[workspaces.surf]
template = "overlay"
mode = "stack"
[workspaces.surf.apps]
chrome = { cell = "left", chain = "SUPER+B" }
yandex = "center"
chrome-ai = { cell = "right", chain = "SUPER+V" }

[workspaces.work]
template = "overlay"
[workspaces.work.apps]
chromium = "left"
calc = { rect = { x = 2600, y = 1500, w = 600, h = 400 } }
"#;

    #[test]
    fn ws_app_entry_forms() {
        // Прежние формы читаются без правки: строка и { rect = … }.
        let cfg = Config::parse(MINIMAL).unwrap();
        assert_eq!(cfg.workspaces["work"].apps["terminal"], WsApp::at(Placement::Cell("right".into())));
        let cfg = Config::parse(OVERRIDES).unwrap();
        let surf = &cfg.workspaces["surf"];
        assert_eq!(surf.apps.keys().map(String::as_str).collect::<Vec<_>>(), vec!["chrome", "yandex", "chrome-ai"]);
        let ai = &surf.apps["chrome-ai"];
        assert_eq!((ai.place.clone(), ai.chain.as_deref(), ai.mode.as_deref()), (Placement::Cell("right".into()), Some("SUPER+V"), None));
        let calc = &cfg.workspaces["work"].apps["calc"];
        assert!(!calc.has_cell() && calc.chain.is_none() && calc.mode.is_none());
        // Таблица с rect, chain и mode.
        let text = OVERRIDES.replace(
            "calc = { rect = { x = 2600, y = 1500, w = 600, h = 400 } }",
            "calc = { rect = { x = 2600, y = 1500, w = 600, h = 400 }, mode = \"swap\", chain = \"SUPER+K\" }",
        );
        let cfg = Config::parse(&text).unwrap();
        let calc = &cfg.workspaces["work"].apps["calc"];
        assert_eq!((calc.chain.as_deref(), calc.mode.as_deref()), (Some("SUPER+K"), Some("swap")));
        assert_eq!(calc.place, Placement::Rect { rect: PxRect { x: 2600, y: 1500, w: 600, h: 400 }.to_rect() });
        // Неизвестный ключ записи — предупреждение, а не отказ.
        let text = OVERRIDES.replace("chrome-ai = { cell = \"right\", chain = \"SUPER+V\" }", "chrome-ai = { cell = \"right\", chian = \"SUPER+V\" }");
        let cfg = Config::parse(&text).unwrap();
        assert!(cfg.workspaces["surf"].apps["chrome-ai"].chain.is_none());
    }

    #[test]
    fn ws_app_entry_errors() {
        let entry = "chrome-ai = { cell = \"right\", chain = \"SUPER+V\" }";
        for bad in ["chrome-ai = { chain = \"SUPER+V\" }", "chrome-ai = { cell = \"right\", rect = { x = 0, y = 0, w = 10, h = 10 } }"] {
            let err = Config::parse(&OVERRIDES.replace(entry, bad)).unwrap_err().to_string();
            assert!(err.contains("surf") && err.contains("chrome-ai"), "{bad}: {err}");
        }
        let err = Config::parse(&OVERRIDES.replace(entry, "chrome-ai = { cell = \"right\", mode = \"stak\" }")).unwrap_err().to_string();
        assert!(err.contains("surf") && err.contains("chrome-ai") && err.contains("stak") && err.contains("swap") && err.contains("stack"), "{err}");
        // Ячейка проверяется и в записи-таблице.
        let err = Config::parse(&OVERRIDES.replace(entry, "chrome-ai = { cell = \"top\" }")).unwrap_err().to_string();
        assert!(err.contains("surf") && err.contains("chrome-ai") && err.contains("top"), "{err}");
    }

    #[test]
    fn app_key_and_mode_in_workspace() {
        let text = OVERRIDES.replace(
            "chromium = \"left\"",
            "chromium = { cell = \"left\", mode = \"stack\" }\nyandex = \"right\"",
        );
        let cfg = Config::parse(&text).unwrap();
        // Ключ: переопределение главнее собственной клавиши.
        assert_eq!(cfg.app_key("surf", "chrome-ai"), Some("SUPER+V"));
        assert_eq!(cfg.app_key("surf", "yandex"), Some("SUPER+SHIFT+Y"));
        assert_eq!(cfg.app_key("work", "chrome-ai"), Some("SUPER+SHIFT+V"));
        assert!(cfg.overrides_key("surf", "chrome-ai") && !cfg.overrides_key("surf", "yandex") && !cfg.overrides_key("work", "chrome-ai"));
        // Отклик workspace на цепочку — по ключам записей, регистр клавиши не важен.
        assert_eq!(cfg.responds("surf", "super+v"), Some("chrome-ai".into()));
        assert_eq!(cfg.responds("surf", "SUPER+SHIFT+Y"), Some("yandex".into()));
        assert_eq!(cfg.responds("surf", "SUPER+SHIFT+V"), None);
        assert_eq!(cfg.responds("work", "SUPER+V"), None);
        // Режим: поле записи, иначе режим workspace; без ячейки в swap — stack.
        assert_eq!(cfg.app_mode("surf", "chrome"), Mode::Stack);
        assert_eq!(cfg.app_mode("work", "calc"), Mode::Stack);
        assert_eq!(cfg.app_mode("work", "chromium"), Mode::Stack);
        assert_eq!(cfg.app_mode("work", "yandex"), Mode::Swap);
        let swap = text.replace("calc = { rect = { x = 2600, y = 1500, w = 600, h = 400 } }", "calc = { rect = { x = 2600, y = 1500, w = 600, h = 400 }, mode = \"swap\" }");
        assert_eq!(Config::parse(&swap).unwrap().app_mode("work", "calc"), Mode::Swap);
        // Переопределение, совпадающее с собственной клавишей, ничего не меняет.
        let same = text.replace("yandex = \"center\"", "yandex = { cell = \"center\", chain = \"shift+super+Y\" }");
        assert!(!Config::parse(&same).unwrap().overrides_key("surf", "yandex"));
        // Вариант без своей записи описан записью семейства и берёт её режим.
        let fam = text.replace("[apps.chrome-ai]\ncmd = \"chrome\"", "[apps.chrome-ai]\nfamily = \"chrome\"\ncmd = \"chrome\"").replace("chrome-ai = { cell = \"right\", chain = \"SUPER+V\" }\n", "");
        let cfg = Config::parse(&fam).unwrap();
        assert_eq!(cfg.ws_entry("surf", "chrome-ai").map(|(n, _)| n), Some("chrome"));
        assert_eq!(cfg.app_key("surf", "chrome-ai"), Some("SUPER+SHIFT+V"));
        assert!(!cfg.overrides_key("surf", "chrome-ai"));
    }

    #[test]
    fn family_and_variant() {
        // Вариант называет семейство плоским полем family; уровней ровно два.
        let ok = format!(
            "{MINIMAL}\n[apps.wezterm]\ncmd = \"wezterm-gui\"\nclass = \"^org\\\\.wezfurlong\\\\.wezterm$\"\n[apps.herdr]\nfamily = \"wezterm\"\ncmd = \"wezterm-gui\"\nclass = \"^wezterm-herdr$\"\n"
        );
        let cfg = Config::parse(&ok).unwrap();
        assert_eq!(cfg.family_of("herdr"), Some("wezterm"));
        assert_eq!(cfg.family_of("wezterm"), None);
        assert_eq!(cfg.variants_of("wezterm"), vec!["herdr"]);
        assert!(cfg.app_is("herdr", "wezterm") && cfg.app_is("herdr", "herdr") && !cfg.app_is("wezterm", "herdr"));

        let no_family = ok.replace("family = \"wezterm\"", "family = \"weztermm\"");
        let err = Config::parse(&no_family).unwrap_err().to_string();
        assert!(err.contains("herdr") && err.contains("weztermm"), "{err}");

        let deep = format!("{ok}[apps.herdr-lab]\nfamily = \"herdr\"\ncmd = \"wezterm-gui\"\nclass = \"^wezterm-herdr-lab$\"\n");
        let err = Config::parse(&deep).unwrap_err().to_string();
        assert!(err.contains("herdr-lab") && err.contains("уровней ровно два"), "{err}");

        let selfish = ok.replace("family = \"wezterm\"", "family = \"herdr\"");
        let err = Config::parse(&selfish).unwrap_err().to_string();
        assert!(err.contains("herdr") && err.contains("само приложение"), "{err}");
    }

    #[test]
    fn app_without_cmd_and_invalid_names() {
        // Приложение-семейство может только собирать окна; без cmd и без class
        // его ни запустить, ни узнать.
        let only_class = format!("{MINIMAL}\n[apps.wezterm]\nclass = \"^org\\\\.wezfurlong\\\\.wezterm$\"\n");
        let cfg = Config::parse(&only_class).unwrap();
        assert!(cfg.apps["wezterm"].cmd.is_none());
        assert!(cfg.app_command(None, &cfg.apps["wezterm"]).is_none());

        let empty = format!("{MINIMAL}\n[apps.wezterm]\nchain = \"SUPER+W\"\n");
        let err = Config::parse(&empty).unwrap_err().to_string();
        assert!(err.contains("wezterm") && err.contains("cmd") && err.contains("class"), "{err}");

        // Двоеточие и «#» отделяют части тега экземпляра, в имени их быть не может.
        for bad in ["chrome#ai", "chrome:ai"] {
            let text = format!("{MINIMAL}\n[apps.\"{bad}\"]\ncmd = \"x\"\n");
            let err = Config::parse(&text).unwrap_err().to_string();
            assert!(err.contains(bad), "{err}");
        }
    }

    #[test]
    fn dialog_title_and_ignore_classes() {
        // Диалог приложения отличается заголовком: класс у него тот же.
        let text = format!(
            "ignore_classes = [\"termfilechooser\", \"pinentry.*\"]\n{MINIMAL}\n[apps.chrome]\ncmd = \"google-chrome\"\nclass = \"(?i)^google-chrome$\"\ndialog_title = \"^(Диспетчер задач|Task Manager)$\"\n"
        );
        let cfg = Config::parse(&text).unwrap();
        let chrome = &cfg.apps["chrome"];
        assert!(chrome.is_dialog("Диспетчер задач") && chrome.is_dialog("Task Manager"));
        assert!(!chrome.is_dialog("Новая вкладка - Google Chrome"));
        // Приложение без dialog_title диалогов не различает.
        assert!(!cfg.apps["terminal"].is_dialog("Диспетчер задач"));
        // Класс из списка исключений сопоставляется целиком.
        assert!(cfg.ignored_class("termfilechooser") && cfg.ignored_class("pinentry-gtk"));
        assert!(!cfg.ignored_class("termfilechooser-2") && !cfg.ignored_class("chromium"));

        // dialog_title без class не имеет смысла: окно приложения узнаётся по class.
        let no_class = format!("{MINIMAL}\n[apps.a]\ncmd = \"a\"\ndialog_title = \"x\"\n");
        let err = Config::parse(&no_class).unwrap_err().to_string();
        assert!(err.contains("приложение a") && err.contains("dialog_title"), "{err}");
        // Ошибка в выражении называет поле.
        let bad = text.replace("dialog_title = \"^(Диспетчер задач|Task Manager)$\"", "dialog_title = \"(\"");
        assert!(format!("{:#}", Config::parse(&bad).unwrap_err()).contains("dialog_title"));
        let bad = text.replace("\"pinentry.*\"", "\"(\"");
        assert!(format!("{:#}", Config::parse(&bad).unwrap_err()).contains("ignore_classes"));
    }

    #[test]
    fn detach_action_in_binds() {
        // Отделение окна от workspace — действие демона строкой.
        let ok = format!("{MINIMAL}\n[[binds]]\nchain = \"SUPER+BackSpace\"\naction = \"detach\"\n");
        let cfg = Config::parse(&ok).unwrap();
        assert_eq!(cfg.binds[0].action, Some(Action::Named("detach".into())));
        let bad = ok.replace("\"detach\"", "\"detahc\"");
        let err = Config::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("BackSpace") && err.contains("detahc"), "{err}");
    }

    #[test]
    fn pull_action_in_binds() {
        let ok = format!("{MINIMAL}\n[[binds]]\nchain = \"SUPER+TAB v\"\naction = {{ app = \"terminal\", pull = true }}\n");
        let cfg = Config::parse(&ok).unwrap();
        assert_eq!(cfg.binds[0].action, Some(Action::Target(TargetAction { desktop: None, workspace: None, app: Some("terminal".into()), pull: Some(true) })));
        let binds = crate::keys::collect(&cfg).unwrap();
        let b = binds.iter().find(|b| b.source.contains("SUPER+TAB v")).unwrap();
        assert!(matches!(&b.act, crate::keys::Act::Daemon(c) if c == "workspaced app terminal --pull"), "{:?}", b.act);
        // С workspace и без app ключ отклоняется с указанием цепочки.
        for bad in ["{ workspace = \"work\", pull = true }", "{ app = \"terminal\", workspace = \"work\", pull = true }", "{ desktop = 2, pull = true }"] {
            let text = ok.replace("{ app = \"terminal\", pull = true }", bad);
            let err = Config::parse(&text).unwrap_err().to_string();
            assert!(err.contains("SUPER+TAB v") && err.contains("pull"), "{bad}: {err}");
        }
        // pull = false у действия приложения допустим и ничего не добавляет.
        let off = ok.replace("pull = true", "pull = false");
        let cfg = Config::parse(&off).unwrap();
        let binds = crate::keys::collect(&cfg).unwrap();
        let b = binds.iter().find(|b| b.source.contains("SUPER+TAB v")).unwrap();
        assert!(matches!(&b.act, crate::keys::Act::Daemon(c) if c == "workspaced app terminal"), "{:?}", b.act);
    }

    #[test]
    fn app_default_rect() {
        let text = format!("{MINIMAL}\n[apps.calc]\ncmd = \"galculator\"\nrect = {{ x = 2600, y = 1500, w = 600, h = 400 }}\n");
        let cfg = Config::parse(&text).unwrap();
        let r = cfg.apps["calc"].rect.as_ref().unwrap().resolve(3840, 2160);
        assert_eq!(r, PxRect { x: 2600, y: 1500, w: 600, h: 400 });
        assert!(cfg.apps["terminal"].rect.is_none());
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
        let (_, args, _, env) = cfg.app_command(Some(ws), &cfg.apps["term"]).unwrap();
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
        // Тильда подставляется только в начале строки, а $HOME — в любом месте,
        // поэтому каталог внутри аргумента пишется через $HOME.
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand("--user-data-dir=~/.config/x", &env), "--user-data-dir=~/.config/x");
        assert_eq!(expand("--user-data-dir=$HOME/.config/x", &env), format!("--user-data-dir={home}/.config/x"));
    }
}
