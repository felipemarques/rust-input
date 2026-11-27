//! `rustinput mcp` (0.50.0): a server of the Model Context Protocol on
//! stdin and stdout, for an AI agent (Claude Code, Claude Desktop, any MCP
//! client) on this computer: `claude mcp add rustinput -- rustinput mcp`.
//!
//! It is a client of the running RustInput, like `rustinput status`: each
//! tool asks the service and answers in the words of the command line. An
//! agent may see the state and take the safe actions; it never authorizes or
//! pairs a computer, never types, presses keys or moves the pointer, and
//! never reads a clipboard: those stay with the person at the screen.
//!
//! JSON-RPC 2.0, one message per line; nothing listens on the network.

use std::{cmp::Ordering, collections::HashMap, path::PathBuf, time::Duration};

use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use rustinput_ipc::{
    ClientHandle, FrontendEvent, FrontendRequest, TransferState, UpdateState, connect_async,
};

use crate::{CliError, Where};

/// the version of RustInput, which [`serve`] is given (this crate has its
/// own)
static VERSION: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

fn version() -> &'static str {
    VERSION.get().copied().unwrap_or("?")
}

/// the versions of the protocol this server speaks, the newest first
const PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// what the agent is told about this server when it connects
const INSTRUCTIONS: &str = "RustInput shares one mouse and keyboard, the clipboard and files \
between the computers of one person on their network. These tools ask the RustInput running \
on this computer: where the mouse and keyboard are, which computers there are, file transfers \
and updates; they move the mouse and keyboard to a computer and send files. Authorizing a \
computer, typing and reading clipboards are left to the person at the screen.";

/// the tools, with the JSON Schema of their arguments
fn tools() -> Value {
    json!([
        {
            "name": "status",
            "title": "Where the mouse and keyboard are",
            "description": "The version of RustInput here, which computer has the mouse and \
                keyboard, every computer (numbered as the keyboard shortcuts number them, \
                whether it is reached), file transfers, updates, and computers waiting to be \
                authorized by the person.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "switch_computer",
            "title": "Move the mouse and keyboard",
            "description": "Moves the mouse and keyboard to another computer, by its name or its \
                number in `status`, or back to this computer with \"home\".",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "computer": {
                        "type": "string",
                        "description": "a name (or its beginning) or number from status, or \"home\""
                    }
                },
                "required": ["computer"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false,
                "idempotentHint": true, "openWorldHint": false }
        },
        {
            "name": "send_files",
            "title": "Send files to the other computers",
            "description": "Sends files and folders of this computer to the connected computers, \
                as \"Copy to RustInput\" in the file manager does: there they wait on the \
                clipboard, ready to be pasted. Large ones may ask the person there first. \
                Follow them with status.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "absolute paths of files and folders on this computer"
                    }
                },
                "required": ["paths"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false,
                "idempotentHint": false, "openWorldHint": false }
        },
        {
            "name": "check_update",
            "title": "Look for a new version",
            "description": "Asks GitHub for the latest version of RustInput, unless the person \
                switched that off, and tells whether it is newer than this one.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true, "openWorldHint": true }
        }
    ])
}

/// what the service says when a client connects, beyond [`Where`]
#[derive(Default)]
struct Report {
    now: Where,
    transfers: Vec<rustinput_ipc::TransferInfo>,
    updates: UpdateState,
    /// computers offered by another one, waiting for the person: who
    /// offered, the name
    introductions: Vec<(String, String)>,
    /// the version of RustInput of each client that described itself
    versions: HashMap<ClientHandle, String>,
}

/// how two versions compare, by their numbers
fn compare(a: &str, b: &str) -> Ordering {
    let numbers = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map_while(|part| part.parse().ok())
            .collect()
    };
    numbers(a).cmp(&numbers(b))
}

impl Report {
    async fn ask() -> Result<Self, CliError> {
        let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
        let mut report = Report::default();
        let now = crate::where_now_with(&mut rx, |event| match event {
            FrontendEvent::Transfers(t) => report.transfers = t.clone(),
            FrontendEvent::Updates(u) => report.updates = u.clone(),
            FrontendEvent::Introduction { by, name, .. } => {
                report.introductions.push((by.clone(), name.clone()))
            }
            _ => {}
        })
        .await?;
        report.now = now;
        // the versions, from the view of the arrangement: what the window
        // asks for too; the computers reached tell theirs within a moment
        tx.request(FrontendRequest::Arrangement).await?;
        let reached: Vec<ClientHandle> = report
            .now
            .clients
            .iter()
            .filter(|c| c.2 && c.3)
            .map(|c| c.0)
            .collect();
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(Ok(event)) = rx.next().await {
                if let FrontendEvent::Arrangement(view) = event {
                    for peer in view.peers.iter().filter(|p| !p.version.is_empty()) {
                        report.versions.insert(peer.handle, peer.version.clone());
                    }
                    if reached.iter().all(|h| report.versions.contains_key(h)) {
                        break;
                    }
                }
            }
        })
        .await;
        Ok(report)
    }

    fn text(&self) -> String {
        let mut out = format!(
            "RustInput {} on this computer.\n{}",
            version(),
            self.now.text()
        );
        let this = version();
        for (handle, name, ..) in &self.now.clients {
            if let Some(version) = self.versions.get(handle) {
                let how = match compare(version, this) {
                    Ordering::Equal => "the same as here",
                    Ordering::Less => "older than this one: update it",
                    Ordering::Greater => "newer than this one",
                };
                out.push_str(&format!("{name} runs RustInput {version} ({how}).\n"));
            }
        }
        let running: Vec<_> = self
            .transfers
            .iter()
            .filter(|t| {
                matches!(
                    t.state,
                    TransferState::Running | TransferState::Waiting | TransferState::Interrupted
                )
            })
            .collect();
        if !running.is_empty() {
            out.push_str("File transfers:\n");
            for t in running {
                let way = if t.sending { "to" } else { "from" };
                let state = match t.state {
                    TransferState::Waiting => " (waiting for another one)",
                    TransferState::Interrupted => {
                        " (interrupted: goes on when it is reached again)"
                    }
                    _ => "",
                };
                out.push_str(&format!(
                    "  {} file(s) {way} {}: {} of {} bytes{state}\n",
                    t.files, t.peer, t.bytes_done, t.bytes
                ));
            }
        }
        for (by, name) in &self.introductions {
            out.push_str(&format!(
                "{by} offers {name}: the person at this computer decides whether to authorize it.\n"
            ));
        }
        if self.updates.available {
            if let Some(latest) = &self.updates.latest {
                out.push_str(&format!(
                    "RustInput {latest} is out: {}\n",
                    self.updates.url
                ));
            }
        }
        out
    }
}

/// a tool that did not work: the agent reads why
struct Failed(String);

impl From<CliError> for Failed {
    fn from(e: CliError) -> Self {
        Failed(match e {
            CliError::ServiceNotRunning(_) => "RustInput is not running on this computer: \
                start it (from the menu of applications, or `rustinput`)"
                .to_owned(),
            e => e.to_string(),
        })
    }
}

async fn call(name: &str, arguments: &Value) -> Option<Result<String, Failed>> {
    Some(match name {
        "status" => Report::ask().await.map(|r| r.text()).map_err(Failed::from),
        "switch_computer" => switch(arguments).await,
        "send_files" => send(arguments).await,
        "check_update" => check_update().await,
        _ => return None,
    })
}

async fn switch(arguments: &Value) -> Result<String, Failed> {
    let Some(computer) = arguments.get("computer").and_then(Value::as_str) else {
        return Err(Failed("which computer: \"computer\" is missing".into()));
    };
    let home = computer.trim().eq_ignore_ascii_case("home");
    let target = (!home).then(|| computer.trim().to_owned());
    let name = crate::go_to(target).await?;
    Ok(match name {
        None => "The mouse and keyboard are back on this computer.".to_owned(),
        Some(name) => format!("The mouse and keyboard went to {name}."),
    })
}

async fn send(arguments: &Value) -> Result<String, Failed> {
    let paths: Vec<PathBuf> = arguments
        .get("paths")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    if paths.is_empty() {
        return Err(Failed("no paths to send".into()));
    }
    if let Some(missing) = paths.iter().find(|p| !p.is_absolute() || !p.exists()) {
        return Err(Failed(format!(
            "{} is not an absolute path of this computer that exists",
            missing.display()
        )));
    }
    let now = Report::ask().await?;
    let connected = now.now.clients.iter().filter(|c| c.2 && c.3).count();
    if connected == 0 {
        return Err(Failed(
            "no other computer is connected to send them to".into(),
        ));
    }
    crate::send_files(paths.clone()).await?;
    Ok(format!(
        "Sending {} item(s) to the connected computers; status follows the transfer.",
        paths.len()
    ))
}

async fn check_update() -> Result<String, Failed> {
    let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500)))
        .await
        .map_err(CliError::from)?;
    let mut updates = None;
    let _ = crate::where_now_with(&mut rx, |event| {
        if let FrontendEvent::Updates(u) = event {
            updates = Some(u.clone());
        }
    })
    .await?;
    let updates = updates.unwrap_or_default();
    if updates.check != Some(true) {
        return Err(Failed(
            "looking for new versions is off in Preferences of RustInput; \
             the person at this computer can switch it on"
                .into(),
        ));
    }
    tx.request(FrontendRequest::CheckUpdates)
        .await
        .map_err(CliError::from)?;
    // the answer of GitHub: an Updates that is not checking any more
    let answer = tokio::time::timeout(Duration::from_secs(20), async {
        let mut seen_checking = false;
        while let Some(Ok(event)) = rx.next().await {
            if let FrontendEvent::Updates(u) = event {
                seen_checking |= u.checking;
                if seen_checking && !u.checking {
                    return Some(u);
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(updates);
    let this = version();
    Ok(match (&answer.error, &answer.latest, answer.available) {
        (Some(e), _, _) => format!("GitHub could not be asked: {e}"),
        (None, Some(latest), true) => {
            format!(
                "RustInput {latest} is out (this one is {this}): {}",
                answer.url
            )
        }
        (None, Some(latest), false) => {
            format!("RustInput {this} is up to date (the latest is {latest}).")
        }
        (None, None, _) => format!("RustInput {this}; the latest version is not known yet."),
    })
}

/// the answer to one message, `None` for a notification
async fn answer(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str);
            let protocol = asked
                .filter(|v| PROTOCOLS.contains(v))
                .unwrap_or(PROTOCOLS[0]);
            Ok(json!({
                "protocolVersion": protocol,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "rustinput", "title": "RustInput",
                    "version": version() },
                "instructions": INSTRUCTIONS
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            match call(name, &arguments).await {
                Some(Ok(text)) => Ok(json!({
                    "content": [{ "type": "text", "text": text }], "isError": false
                })),
                Some(Err(Failed(why))) => Ok(json!({
                    "content": [{ "type": "text", "text": why }], "isError": true
                })),
                None => Err((-32602, format!("no tool {name:?}"))),
            }
        }
        _ => Err((-32601, format!("no method {method:?}"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    })
}

/// the answer to one line: a message, or a batch of them
async fn line(text: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(batch)) => {
            let mut answers = Vec::new();
            for message in &batch {
                answers.extend(answer(message).await);
            }
            (!answers.is_empty()).then_some(Value::Array(answers))
        }
        Ok(message) => answer(&message).await,
        Err(e) => Some(json!({ "jsonrpc": "2.0", "id": null,
            "error": { "code": -32700, "message": e.to_string() } })),
    }
}

/// `rustinput mcp`: answers the messages of stdin on stdout until stdin
/// ends; `version` is the one of RustInput
pub async fn serve(version: &'static str) -> Result<(), CliError> {
    let _ = VERSION.set(version);
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Ok(Some(text)) = lines.next_line().await {
        if text.trim().is_empty() {
            continue;
        }
        if let Some(answer) = line(&text).await {
            let mut out = answer.to_string();
            out.push('\n');
            if stdout.write_all(out.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> Option<Value> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(line(text))
    }

    #[test]
    fn it_introduces_itself() {
        let hello = run(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#).unwrap();
        assert_eq!(hello["id"], 1);
        assert_eq!(hello["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(hello["result"]["serverInfo"]["name"], "rustinput");
        // a version it does not know: its own
        let hello = run(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#).unwrap();
        assert_eq!(hello["result"]["protocolVersion"], PROTOCOLS[0]);
        // notifications have no answer
        assert_eq!(
            run(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            None
        );
    }

    #[test]
    fn its_tools_and_nothing_more() {
        let list = run(r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#).unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["status", "switch_computer", "send_files", "check_update"]
        );
        for tool in list["result"]["tools"].as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        }
        let no = run(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"pair","arguments":{}}}"#).unwrap();
        assert_eq!(no["error"]["code"], -32602);
        let no = run(r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#).unwrap();
        assert_eq!(no["error"]["code"], -32601);
        let bad = run("{not json").unwrap();
        assert_eq!(bad["error"]["code"], -32700);
        assert_eq!(bad["id"], Value::Null);
    }

    #[test]
    fn versions_compare_by_their_numbers() {
        assert_eq!(compare("0.9.0", "0.10.0"), Ordering::Less);
        assert_eq!(compare("0.50.0", "0.50.0"), Ordering::Equal);
    }

    #[test]
    fn arguments_are_checked_before_asking() {
        let no = run(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"send_files","arguments":{"paths":["relative/file"]}}}"#).unwrap();
        assert_eq!(no["result"]["isError"], true);
        let no = run(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"switch_computer","arguments":{}}}"#).unwrap();
        assert_eq!(no["result"]["isError"], true);
    }
}
