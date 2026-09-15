//! Команда «сохранить workspace»: запись текущего состояния активного workspace
//! в `config.toml` через toml_edit с сохранением комментариев (design D4).

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value, value};

use crate::config::PxRect;
use crate::daemon::Daemon;
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
        let live = clients.iter().find(|c| c.app().as_deref() == Some(app)).map(|c| c.rect());
        let item = match (place, live, expected) {
            (Place::Cell(c), Some(l), Some(e)) if l == e => value(c.as_str()),
            (Place::Cell(c), None, _) => value(c.as_str()),
            (Place::Cell(_), Some(l), _) => Item::Value(rect_item(l)),
            (Place::Rect { rect }, live, _) => Item::Value(rect_item(live.unwrap_or(*rect))),
        };
        apps_tab.insert(app, item);
    }
    // Посторонние окна, привязанные к workspace, становятся приложениями.
    let mut taken: Vec<String> = cfg.apps.keys().cloned().collect();
    let foreign: Vec<(String, crate::state::Foreign)> = d.state().foreign.iter().filter(|(_, f)| f.workspace.as_deref() == Some(&ws)).map(|(a, f)| (a.clone(), f.clone())).collect();
    for (addr, f) in foreign {
        let Some(c) = clients.iter().find(|c| c.address == addr) else { continue };
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
        apps_tab.insert(&name, Item::Value(rect_item(c.rect())));
    }
    let wtab = doc["workspaces"][&ws].as_table_mut().unwrap();
    wtab.insert("apps", Item::Table(apps_tab));

    let path = d.cfg_path().to_path_buf();
    let out = doc.to_string();
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, &path)?;
    d.set_cfg_text(out);
    d.reload_config();
    log::info!("workspace {ws} записан в конфиг");
    Ok(ws)
}
