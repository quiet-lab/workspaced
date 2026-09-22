//! workspaced — демон workspace для сессии Hyprland.
//!
//! Один бинарник: подкоманда `daemon` запускает сервер, остальные подкоманды —
//! клиенты, которые шлют запрос в сокет демона и печатают ответ.

mod client;
mod config;
mod daemon;
mod hypr;
mod keys;
mod save;
mod session;
mod state;

use serde_json::json;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "workspaced", version, about = "Демон workspace для сессии Hyprland")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Запустить демон (сервер)
    Daemon,
    /// Проверить конфиг и вывести сводку
    Check,
    /// Напечатать привязки: код Lua для конфига Hyprland (--lua) или таблицу (--list)
    Keys {
        /// Код Lua; удачный результат сохраняется в ~/.local/state/workspaced/keys.lua
        #[arg(long)]
        lua: bool,
        /// Таблица всех привязок: цепочка, действие, флаги, источник
        #[arg(long)]
        list: bool,
    },
    /// Поставить активное окно в половину рабочей области
    Half {
        /// left, right, up или down
        side: String,
    },
    /// Развернуть активное окно на рабочую область с отступами; повторно — вернуть прежние положение и размер
    Maximize,
    /// Поставить активное окно в позицию рабочей области (половина ширины и высоты в углах и по центру рядов, center — на всю высоту, full — вся область)
    Place {
        /// top-left, top-center, top-right, bottom-left, bottom-center, bottom-right, center или full
        position: String,
    },
    /// Напечатать состояние демона
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Поднять workspace на столе
    Raise {
        workspace: String,
        #[arg(long)]
        desktop: Option<u8>,
    },
    /// Сделать приложение главным (несколько имён — кандидаты с одной цепочкой)
    App {
        #[arg(required = true)]
        app: Vec<String>,
        #[arg(long)]
        desktop: Option<u8>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Поднять следующий workspace из списка текущего стола
    Next,
    /// Принять посторонние окна текущего стола в активный workspace на время сессии
    SaveSession,
    /// Записать текущее состояние активного workspace в конфиг
    SaveWorkspace,
    /// Убрать workspace из списка стола (окна паркуются)
    Remove {
        workspace: String,
        #[arg(long)]
        desktop: Option<u8>,
    },
    /// Открыть в панели окно выбора сессии
    Sessions,
    /// Сессии
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Сохранить текущее состояние под именем
    Save { name: String },
    /// Загрузить сессию со сверкой открытых окон
    Load { name: String },
    /// Список сессий
    List {
        #[arg(long)]
        json: bool,
    },
}

fn main() -> anyhow::Result<()> {
    // Клиент печатает в канал (`| head`): обрыв канала завершает процесс, а не роняет его.
    unsafe {
        let _ = nix::sys::signal::signal(nix::sys::signal::Signal::SIGPIPE, nix::sys::signal::SigHandler::SigDfl);
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon => daemon::run(),
        Command::Check => check(),
        Command::Keys { list, .. } => {
            let cfg = config::Config::load(&config::config_path())?;
            if list {
                print!("{}", keys::to_list(&cfg)?);
            } else {
                let code = keys::to_lua(&cfg)?;
                if let Err(e) = keys::write_cache(&code) {
                    eprintln!("workspaced: копия привязок не записана: {e:#}");
                }
                print!("{code}");
            }
            Ok(())
        }
        Command::Half { side } => client::call(json!({ "cmd": "half", "side": side })).map(|_| ()),
        Command::Maximize => client::call(json!({ "cmd": "maximize" })).map(|_| ()),
        Command::Place { position } => client::call(json!({ "cmd": "place", "position": position })).map(|_| ()),
        Command::Status { json: as_json } => {
            let v = client::call(json!({ "cmd": "status" }))?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print_status(&v);
            }
            Ok(())
        }
        Command::Raise { workspace, desktop } => client::call(json!({ "cmd": "raise", "workspace": workspace, "desktop": desktop })).map(|_| ()),
        Command::App { app, desktop, workspace } => client::call(json!({ "cmd": "app", "apps": app, "desktop": desktop, "workspace": workspace })).map(|_| ()),
        Command::Next => client::call(json!({ "cmd": "next" })).map(|_| ()),
        Command::SaveSession => {
            let v = client::call(json!({ "cmd": "save-session" }))?;
            let ws = v.get("workspace").and_then(|w| w.as_str()).unwrap_or("?");
            match v.get("adopted").and_then(|n| n.as_u64()).unwrap_or(0) {
                0 => println!("workspace {ws}: посторонних окон на столе нет"),
                n => println!("workspace {ws}: принято окон — {n}"),
            }
            Ok(())
        }
        Command::SaveWorkspace => {
            let v = client::call(json!({ "cmd": "save-workspace" }))?;
            println!("workspace {} записан в конфиг", v.get("workspace").and_then(|w| w.as_str()).unwrap_or("?"));
            Ok(())
        }
        Command::Remove { workspace, desktop } => client::call(json!({ "cmd": "remove", "workspace": workspace, "desktop": desktop })).map(|_| ()),
        Command::Sessions => client::call(json!({ "cmd": "sessions" })).map(|_| ()),
        Command::Session { command } => match command {
            SessionCommand::Save { name } => client::call(json!({ "cmd": "session", "op": "save", "name": name })).map(|_| ()),
            SessionCommand::Load { name } => client::call(json!({ "cmd": "session", "op": "load", "name": name })).map(|_| ()),
            SessionCommand::List { json: as_json } => {
                let v = client::call(json!({ "cmd": "session", "op": "list" }))?;
                let list = v.get("sessions").cloned().unwrap_or_default();
                if as_json {
                    println!("{}", serde_json::to_string_pretty(&list)?);
                } else if let Some(arr) = list.as_array() {
                    for s in arr {
                        println!("{}\t{}", s.get("name").and_then(|n| n.as_str()).unwrap_or("?"), s.get("saved").and_then(|n| n.as_str()).unwrap_or(""));
                    }
                }
                Ok(())
            }
        },
    }
}

/// Краткое состояние: столы, их workspace и окна.
fn print_status(v: &serde_json::Value) {
    println!("активный стол: {}", v.get("current_desktop").and_then(|d| d.as_u64()).unwrap_or(0));
    if let Some(desktops) = v.get("desktops").and_then(|d| d.as_object()) {
        for (n, d) in desktops {
            let list: Vec<String> = d
                .get("workspaces")
                .and_then(|w| w.as_array())
                .map(|a| a.iter().map(|w| format!("{}{}", w.get("name").and_then(|x| x.as_str()).unwrap_or("?"), if w.get("active").and_then(|x| x.as_bool()).unwrap_or(false) { "*" } else { "" })).collect())
                .unwrap_or_default();
            if !list.is_empty() {
                println!("стол {n}: {}", list.join(", "));
            }
        }
    }
    if let Some(ws) = v.get("windows").and_then(|w| w.as_array()) {
        for w in ws {
            println!(
                "  {} {:<28} стол {:<14} приложение {}",
                w.get("address").and_then(|x| x.as_str()).unwrap_or(""),
                w.get("class").and_then(|x| x.as_str()).unwrap_or(""),
                w.get("workspace").and_then(|x| x.as_str()).unwrap_or(""),
                w.get("app").and_then(|x| x.as_str()).or_else(|| w.get("foreign").and_then(|x| x.as_bool()).filter(|f| *f).map(|_| "(постороннее)")).unwrap_or("-")
            );
        }
    }
}

/// Проверить конфиг и напечатать сводку.
fn check() -> anyhow::Result<()> {
    let path = config::config_path();
    let cfg = config::Config::load(&path)?;
    let binds = keys::collect(&cfg)?;
    println!("{}: конфиг корректен", path.display());
    println!("шаблонов: {}, приложений: {}, workspace: {}, привязок: {}", cfg.templates.len(), cfg.apps.len(), cfg.workspaces.len(), binds.len());
    for w in cfg.workspaces.keys() {
        println!("  workspace {w}");
    }
    for (a, app) in &cfg.apps {
        match &app.family {
            Some(f) => println!("  приложение {a} (вариант семейства {f})"),
            None => println!("  приложение {a}"),
        }
    }
    for t in cfg.templates.keys() {
        println!("  шаблон {t}");
    }
    Ok(())
}
