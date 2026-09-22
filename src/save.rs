//! Команда «сохранить workspace»: запись текущего состояния активного workspace
//! в `config.toml` через toml_edit с сохранением комментариев (design D4).

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value, value};

use crate::config::PxRect;
use crate::daemon::{self, Daemon};
use crate::hypr;
use crate::state::Place;

fn rect_table(r: PxRect) -> InlineTable {
    let mut t = InlineTable::new();
    t.insert("x", (r.x as i64).into());
    t.insert("y", (r.y as i64).into());
    t.insert("w", (r.w as i64).into());
    t.insert("h", (r.h as i64).into());
    t
}

fn rect_item(r: PxRect) -> Value {
    let mut t = InlineTable::new();
    t.insert("rect", Value::InlineTable(rect_table(r)));
    Value::InlineTable(t)
}

/// Запись приложения в таблице `apps` workspace: имя ячейки, если окно стоит
/// в ней, иначе прямоугольник. Место берётся у первого экземпляра, остальные
/// окна приложения отдельными записями не появляются.
fn app_item(place: &Place, live: Option<PxRect>, expected: Option<PxRect>) -> Item {
    match (place, live, expected) {
        (Place::Cell(c), Some(l), Some(e)) if l == e => value(c.as_str()),
        (Place::Cell(c), None, _) => value(c.as_str()),
        (Place::Cell(_), Some(l), _) => Item::Value(rect_item(l)),
        (Place::Rect { rect }, live, _) => Item::Value(rect_item(live.unwrap_or(*rect))),
    }
}

/// Имя для постороннего окна по классу, уникальное среди приложений.
fn app_name(class: &str, taken: &[String]) -> String {
    let base: String = class.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect::<String>().trim_matches('-').to_string();
    let base = if base.is_empty() { "app".to_string() } else { base };
    if !taken.contains(&base) {
        return base;
    }
    (2..).map(|i| format!("{base}-{i}")).find(|n| !taken.contains(n)).unwrap()
}

pub fn save_workspace(d: &mut Daemon) -> Result<String> {
    let n = d.current_desktop();
    let Some(ws) = d.state().desktops.get(&n).and_then(|x| x.active.clone()) else { bail!("на столе {n} нет активного workspace") };
    let text = std::fs::read_to_string(d.cfg_path()).context("чтение конфига")?;
    if text != d.cfg_text() {
        d.reload_config();
        bail!("конфиг изменился с последнего чтения; перечитан, повторите команду");
    }
    let mut doc: DocumentMut = text.parse().context("разбор конфига для записи")?;
    let clients = d.clients()?;
    let mon = d.mon();
    let cfg = d.cfg().clone();
    let cells = d.state_mut().cells_of(&cfg, &ws, mon).clone();
    let main_app = d.state_mut().main_app(&cfg, &ws, mon);

    let root = doc.as_table_mut();
    let workspaces = root.entry("workspaces").or_insert(Item::Table(Table::new())).as_table_mut().context("[workspaces] не таблица")?;
    let wtab = workspaces.entry(&ws).or_insert(Item::Table(Table::new())).as_table_mut().context("workspace не таблица")?;
    if let Some(m) = &main_app {
        wtab["main"] = value(m.as_str());
    }
    // Приложения по ячейкам; окно не в своей ячейке — rect.
    let mut apps_tab = Table::new();
    apps_tab.set_implicit(false);
    for (app, place) in &cells {
        let expected = d.state_mut().rect_for(&cfg, &ws, app, mon);
        // У приложения с несколькими окнами записывается место первого
        // экземпляра: стопка стоит в одном месте, и отдельных записей
        // для остальных окон в файле не появляется.
        let live = daemon::app_windows(&cfg, &clients, app).first().map(|c| c.rect());
        apps_tab.insert(app, app_item(place, live, expected));
    }
    // Посторонние окна на столе workspace становятся его приложениями: описанное
    // в конфиге приложение вне workspace — под своим именем, остальные — новой
    // записью в [apps] по команде процесса. Окно получает тег приложения и
    // перестаёт быть посторонним; свободные окна на других столах не трогаются.
    let mut taken: Vec<String> = cfg.apps.keys().cloned().collect();
    let mut ex = Vec::new();
    let mut adopted = Vec::new();
    for c in clients.iter().filter(|c| c.desktop() == Some(n)) {
        // Окно варианта считается окном своего семейства: если workspace
        // описывает семейство, второй записи о варианте не появляется.
        if c.app().is_some_and(|a| cells.keys().any(|x| cfg.app_is(&a, x))) {
            continue;
        }
        let Some(f) = d.state().foreign.get(&c.address).cloned() else {
            continue;
        };
        let name = match c.app() {
            Some(a) if cfg.apps.contains_key(&a) => a,
            _ => {
                if f.cmd.is_empty() {
                    continue;
                }
                let name = app_name(&c.class, &taken);
                taken.push(name.clone());
                let apps_root = root.entry("apps").or_insert(Item::Table(Table::new())).as_table_mut().context("[apps] не таблица")?;
                let mut t = Table::new();
                t["cmd"] = value(f.cmd[0].as_str());
                if f.cmd.len() > 1 {
                    let mut arr = toml_edit::Array::new();
                    for a in &f.cmd[1..] {
                        arr.push(a.as_str());
                    }
                    t["args"] = value(arr);
                }
                if let Some(cwd) = &f.cwd {
                    t["cwd"] = value(cwd.as_str());
                }
                apps_root.insert(&name, Item::Table(t));
                ex.push(hypr::d_tag(&c.address, &format!("app:{name}#{}", daemon::free_instance(&clients, &name))));
                name
            }
        };
        apps_tab.insert(&name, Item::Value(rect_item(c.rect())));
        adopted.push(c.address.clone());
        log::info!("сохранение: окно {} ({}) → приложение {name} workspace {ws}", c.address, c.class);
    }
    let wtab = doc["workspaces"][&ws].as_table_mut().unwrap();
    wtab.insert("apps", Item::Table(apps_tab));

    let path = d.cfg_path().to_path_buf();
    let out = doc.to_string();
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, &path)?;
    d.set_cfg_text(out);
    if !ex.is_empty() {
        d.hypr().dispatch_all(&ex)?;
    }
    for a in &adopted {
        d.state_mut().foreign.remove(a);
    }
    // Назначения ячеек workspace пересобираются из только что записанного конфига,
    // иначе новые приложения в них не попадут до перезапуска демона.
    d.state_mut().cells.remove(&ws);
    d.reload_config();
    log::info!("workspace {ws} записан в конфиг");
    Ok(ws)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::daemon;
    use crate::hypr::test_client;

    const CFG: &str = r#"
[templates.thirds]
main = "center"
[templates.thirds.cells]
left   = { x = -805, y = 10, w = 1920, h = 2140 }
center = { x = 1125, y = 10, w = 1920, h = 2140 }

[apps.chromium]
cmd = "chromium"
class = "(?i)^chromium(-browser)?$"

[apps.herdr]
cmd = "wezterm-gui"
class = "^wezterm-herdr$"

[workspaces.work]
template = "thirds"
main = "herdr"
apps = { herdr = "center", chromium = "left" }
"#;

    fn at(addr: &str, tag: &str, r: PxRect) -> crate::hypr::Client {
        let mut c = test_client(addr, "chromium", "Новости", "1", &[tag]);
        c.at = (r.x, r.y);
        c.size = (r.w, r.h);
        c
    }

    #[test]
    fn rect_of_first_instance_only() {
        let cfg = Config::parse(CFG).unwrap();
        let cell = PxRect { x: -805, y: 10, w: 1920, h: 2140 };
        let moved = PxRect { x: 100, y: 200, w: 800, h: 600 };
        // Первый экземпляр сдвинут пользователем, второй стоит в ячейке.
        let clients = vec![at("0x2", "app:chromium#2", cell), at("0x1", "app:chromium#1", moved)];
        let wins = daemon::app_windows(&cfg, &clients, "chromium");
        assert_eq!(wins.len(), 2);
        let live = wins.first().map(|c| c.rect());
        assert_eq!(live, Some(moved));

        let mut tab = Table::new();
        tab.insert("chromium", app_item(&Place::Cell("left".into()), live, Some(cell)));
        // В файле одна запись о приложении, и это место первого экземпляра.
        assert_eq!(tab.len(), 1);
        let written = tab["chromium"].as_inline_table().unwrap()["rect"].as_inline_table().unwrap().clone();
        assert_eq!(written.get("x").unwrap().as_integer(), Some(100));
        assert_eq!(written.get("w").unwrap().as_integer(), Some(800));

        // Окно первого экземпляра в своей ячейке — записывается имя ячейки.
        let clients = vec![at("0x1", "app:chromium#1", cell), at("0x2", "app:chromium#2", moved)];
        let live = daemon::app_windows(&cfg, &clients, "chromium").first().map(|c| c.rect());
        assert_eq!(app_item(&Place::Cell("left".into()), live, Some(cell)).as_str(), Some("left"));
    }
}
