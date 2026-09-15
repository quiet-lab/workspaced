//! Клиент: одна строка JSON в сокет демона, одна строка ответа.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::daemon::socket_path;

pub fn call(req: Value) -> Result<Value> {
    let path = socket_path()?;
    let mut s = UnixStream::connect(&path).with_context(|| format!("демон workspaced недоступен ({})", path.display()))?;
    writeln!(s, "{req}")?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    let v: Value = serde_json::from_str(line.trim()).context("ответ демона не JSON")?;
    if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
        bail!("{}", v.get("error").and_then(|e| e.as_str()).unwrap_or("ошибка демона"));
    }
    Ok(v)
}
