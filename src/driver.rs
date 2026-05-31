//! Versioned stdio protocol for TypeScript and other external session clients.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::session::Session;
use crate::shot::{ColorMode, Options};

/// Current JSON Lines protocol version spoken by `cellshot driver`.
pub const PROTOCOL_VERSION: u8 = 1;

/// Serve isolated embedded sessions over newline-delimited JSON requests and responses.
///
/// A hello message is written before any requests are read. Standard output is reserved for
/// protocol messages; callers should send diagnostic output elsewhere.
pub fn serve(reader: impl BufRead, mut writer: impl Write) -> Result<()> {
    write_message(
        &mut writer,
        &json!({
            "type": "hello",
            "protocolVersion": PROTOCOL_VERSION,
            "cellshotVersion": env!("CARGO_PKG_VERSION")
        }),
    )?;
    let mut sessions = HashMap::<String, Session>::new();
    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                write_error(&mut writer, None, "READ_ERROR", &error.to_string())?;
                break;
            }
        };
        let request = match serde_json::from_str::<Request>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_error(&mut writer, None, "INVALID_REQUEST", &error.to_string())?;
                continue;
            }
        };
        let id = request.id;
        let shutdown = matches!(&request.method, Method::Shutdown);
        match dispatch(&mut sessions, request) {
            Ok(result) => write_message(
                &mut writer,
                &json!({ "type": "response", "id": id, "result": result }),
            )?,
            Err(error) => write_error(
                &mut writer,
                Some(id),
                "REQUEST_FAILED",
                &format!("{error:#}"),
            )?,
        }
        if shutdown {
            break;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct Request {
    id: u64,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(flatten)]
    method: Method,
}

#[derive(Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
enum Method {
    Launch(LaunchParams),
    Status,
    Send(SendParams),
    WaitForText(WaitForTextParams),
    WaitForIdle(WaitForIdleParams),
    Shot(ShotParams),
    History(HistoryParams),
    Resize(ResizeParams),
    Stop,
    Shutdown,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LaunchParams {
    command: Vec<String>,
    cwd: Option<PathBuf>,
    record: Option<PathBuf>,
    cols: Option<u16>,
    rows: Option<u16>,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
    max_bytes: Option<usize>,
    host: Option<HostProfile>,
    color: Option<DriverColorMode>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum HostProfile {
    Opentui,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum DriverColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendParams {
    input: Vec<InputAtom>,
    #[serde(default)]
    pace_ms: u64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum InputAtom {
    Text { value: String },
    Key { value: Key },
    Control { value: String },
    Bytes { value: Vec<u8> },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum Key {
    Enter,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Tab,
    ShiftTab,
    Backspace,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WaitForTextParams {
    text: String,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WaitForIdleParams {
    #[serde(default = "default_settle_ms")]
    quiet_for_ms: u64,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShotParams {
    #[serde(default = "default_settle_ms")]
    settle_ms: u64,
    #[serde(default = "default_timeout_ms")]
    deadline_ms: u64,
}

#[derive(Default, Deserialize)]
struct HistoryParams {
    #[serde(default)]
    ansi: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResizeParams {
    cols: u16,
    rows: u16,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
}

fn dispatch(sessions: &mut HashMap<String, Session>, request: Request) -> Result<Value> {
    match request.method {
        Method::Launch(params) => {
            let session_id = required_session_id(&request.session_id)?;
            if sessions.contains_key(session_id) {
                bail!("driver session {session_id:?} already exists");
            }
            let options = options(&params);
            let session = Session::start(
                &params.command,
                params.cwd.as_deref(),
                params.record.as_deref(),
                &options,
            )?;
            sessions.insert(session_id.to_owned(), session);
            Ok(json!({ "sessionId": session_id }))
        }
        Method::Status => Ok(serde_json::to_value(
            session(sessions, &request.session_id)?.status()?,
        )?),
        Method::Send(params) => {
            let input = input_bytes(params.input, params.pace_ms > 0)?;
            session(sessions, &request.session_id)?
                .send_all(&input, Duration::from_millis(params.pace_ms))?;
            Ok(Value::Null)
        }
        Method::WaitForText(params) => {
            session(sessions, &request.session_id)?
                .wait_for_text(&params.text, Duration::from_millis(params.timeout_ms))?;
            Ok(Value::Null)
        }
        Method::WaitForIdle(params) => {
            session(sessions, &request.session_id)?.wait_for_idle(
                Duration::from_millis(params.quiet_for_ms),
                Duration::from_millis(params.timeout_ms),
            )?;
            Ok(Value::Null)
        }
        Method::Shot(params) => Ok(serde_json::to_value(
            session(sessions, &request.session_id)?.capture(
                Duration::from_millis(params.settle_ms),
                Duration::from_millis(params.deadline_ms),
            )?,
        )?),
        Method::History(params) => Ok(json!({
            "ansi": params.ansi,
            "bytes": session(sessions, &request.session_id)?.history(params.ansi)?
        })),
        Method::Resize(params) => {
            let session = session(sessions, &request.session_id)?;
            let status = session.status()?;
            session.resize(
                params.cols,
                params.rows,
                params.cell_width.unwrap_or(status.cell_width),
                params.cell_height.unwrap_or(status.cell_height),
            )?;
            Ok(Value::Null)
        }
        Method::Stop => {
            let session_id = required_session_id(&request.session_id)?;
            let mut session = sessions
                .remove(session_id)
                .ok_or_else(|| anyhow!("driver session {session_id:?} does not exist"))?;
            session.stop()?;
            Ok(Value::Null)
        }
        Method::Shutdown => {
            for session in sessions.values_mut() {
                session.stop()?;
            }
            sessions.clear();
            Ok(Value::Null)
        }
    }
}

fn required_session_id(session_id: &Option<String>) -> Result<&str> {
    session_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("request requires a non-empty sessionId"))
}

fn session<'a>(
    sessions: &'a mut HashMap<String, Session>,
    session_id: &Option<String>,
) -> Result<&'a mut Session> {
    let session_id = required_session_id(session_id)?;
    sessions
        .get_mut(session_id)
        .ok_or_else(|| anyhow!("driver session {session_id:?} does not exist"))
}

fn options(params: &LaunchParams) -> Options {
    let mut options = Options::default();
    options.cols = params.cols.unwrap_or(options.cols);
    options.rows = params.rows.unwrap_or(options.rows);
    options.cell_width = params.cell_width.unwrap_or(options.cell_width);
    options.cell_height = params.cell_height.unwrap_or(options.cell_height);
    options.max_bytes = params.max_bytes.unwrap_or(options.max_bytes);
    options.opentui_host = matches!(params.host, Some(HostProfile::Opentui));
    options.color = match params.color {
        Some(DriverColorMode::Always) => ColorMode::Always,
        Some(DriverColorMode::Never) => ColorMode::Never,
        Some(DriverColorMode::Auto) | None => ColorMode::Auto,
    };
    options
}

fn input_bytes(input: Vec<InputAtom>, paced: bool) -> Result<Vec<Vec<u8>>> {
    let mut output = Vec::new();
    for atom in input {
        match atom {
            InputAtom::Text { value } if paced => {
                output.extend(value.chars().map(|char| char.to_string().into_bytes()));
            }
            InputAtom::Text { value } => output.push(value.into_bytes()),
            InputAtom::Key { value } => output.push(key_bytes(value).to_vec()),
            InputAtom::Control { value } => output.push(control_bytes(&value)?),
            InputAtom::Bytes { value } => output.push(value),
        }
    }
    Ok(output)
}

fn key_bytes(key: Key) -> &'static [u8] {
    match key {
        Key::Enter => b"\r",
        Key::Escape => b"\x1b",
        Key::ArrowUp => b"\x1b[A",
        Key::ArrowDown => b"\x1b[B",
        Key::ArrowLeft => b"\x1b[D",
        Key::ArrowRight => b"\x1b[C",
        Key::Tab => b"\t",
        Key::ShiftTab => b"\x1b[Z",
        Key::Backspace => b"\x7f",
        Key::Delete => b"\x1b[3~",
        Key::Home => b"\x1b[H",
        Key::End => b"\x1b[F",
        Key::PageUp => b"\x1b[5~",
        Key::PageDown => b"\x1b[6~",
    }
}

fn control_bytes(value: &str) -> Result<Vec<u8>> {
    if value.len() != 1 {
        bail!("control input value must be one ASCII letter");
    }
    let value = value.as_bytes()[0].to_ascii_lowercase();
    if !value.is_ascii_lowercase() {
        bail!("control input value must be one ASCII letter");
    }
    Ok(vec![value - b'a' + 1])
}

fn default_settle_ms() -> u64 {
    250
}

fn default_timeout_ms() -> u64 {
    5_000
}

fn write_error(writer: &mut impl Write, id: Option<u64>, code: &str, message: &str) -> Result<()> {
    write_message(
        writer,
        &json!({ "type": "error", "id": id, "error": { "code": code, "message": message } }),
    )
}

fn write_message(writer: &mut impl Write, message: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *writer, message)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};

    use super::*;

    #[cfg(unix)]
    #[test]
    fn jsonl_driver_operates_an_isolated_session() {
        let requests = concat!(
            r#"{"id":1,"method":"launch","sessionId":"app","params":{"command":["sh","-c","printf ready; IFS= read -r line; printf '\r\ngot:%s' \"$line\"; sleep 1"],"cols":20,"rows":4}}"#,
            "\n",
            r#"{"id":2,"method":"waitForText","sessionId":"app","params":{"text":"ready","timeoutMs":2000}}"#,
            "\n",
            r#"{"id":3,"method":"send","sessionId":"app","params":{"input":[{"type":"text","value":"hello"},{"type":"key","value":"enter"}]}}"#,
            "\n",
            r#"{"id":4,"method":"waitForText","sessionId":"app","params":{"text":"got:hello","timeoutMs":2000}}"#,
            "\n",
            r#"{"id":5,"method":"shot","sessionId":"app","params":{"settleMs":10,"deadlineMs":2000}}"#,
            "\n",
            r#"{"id":6,"method":"stop","sessionId":"app"}"#,
            "\n",
            r#"{"id":7,"method":"shutdown"}"#,
            "\n"
        );
        let mut output = Vec::new();

        serve(
            BufReader::new(Cursor::new(requests.as_bytes())),
            &mut output,
        )
        .unwrap();

        let messages = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(messages[0]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(messages[5]["result"]["reason"], "idle");
        assert!(messages[5]["result"]["shot"]["frame"]["cells"].is_array());
        assert_eq!(messages[7]["result"], Value::Null);
    }

    #[test]
    fn rejects_unsupported_control_input_in_protocol() {
        assert!(control_bytes("meta").is_err());
        assert!(control_bytes("1").is_err());
        assert_eq!(control_bytes("C").unwrap(), b"\x03");
    }
}
