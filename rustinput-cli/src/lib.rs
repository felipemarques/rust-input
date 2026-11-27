use clap::{Args, Parser, Subcommand};
use futures::StreamExt;

use std::{net::IpAddr, time::Duration};

pub mod mcp;
use thiserror::Error;

use rustinput_ipc::{
    ClientHandle, ConnectionError, FrontendEvent, FrontendRequest, IpcError, Position,
    connect_async,
};

#[derive(Debug, Error)]
pub enum CliError {
    /// is the service running?
    #[error("could not connect: `{0}` - is the service running?")]
    ServiceNotRunning(#[from] ConnectionError),
    #[error("error communicating with service: {0}")]
    Ipc(#[from] IpcError),
    /// `rustinput go` with a computer that is not there
    #[error("{0}")]
    NotFound(String),
}

/// what the service tells about the computers, as `status`, `go` and the
/// Waybar module need it (0.46.0)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Where {
    /// the clients, by handle: name, switched on, reachable
    clients: Vec<(ClientHandle, String, bool, bool)>,
    /// the client the mouse and keyboard are on
    pointer_at: Option<ClientHandle>,
    paused: bool,
}

impl Where {
    /// takes in an event of the service; whether it changed something
    fn event(&mut self, event: &FrontendEvent) -> bool {
        let before = self.clone();
        match event {
            FrontendEvent::Enumerate(clients) => {
                self.clients = clients
                    .iter()
                    .map(|(handle, config, state)| {
                        let name = config
                            .hostname
                            .clone()
                            .or_else(|| config.fix_ips.first().map(|ip| ip.to_string()))
                            .unwrap_or_else(|| handle.to_string());
                        (*handle, name, state.active, state.alive)
                    })
                    .collect();
                self.clients.sort_by_key(|c| c.0);
            }
            FrontendEvent::State(handle, config, state) => {
                if let Some(c) = self.clients.iter_mut().find(|c| c.0 == *handle) {
                    if let Some(name) = &config.hostname {
                        c.1 = name.clone();
                    }
                    c.2 = state.active;
                    c.3 = state.alive;
                }
            }
            FrontendEvent::Deleted(handle) => self.clients.retain(|c| c.0 != *handle),
            FrontendEvent::PointerAt(at) => self.pointer_at = *at,
            FrontendEvent::Paused(paused) => self.paused = *paused,
            _ => {}
        }
        *self != before
    }

    /// the switched on clients, numbered from 1 as the keyboard shortcuts
    /// number them
    fn numbered(&self) -> Vec<(usize, ClientHandle, &str)> {
        self.clients
            .iter()
            .filter(|c| c.2)
            .enumerate()
            .map(|(i, c)| (i + 1, c.0, c.1.as_str()))
            .collect()
    }

    fn name(&self, handle: ClientHandle) -> &str {
        self.clients
            .iter()
            .find(|c| c.0 == handle)
            .map_or("?", |c| c.1.as_str())
    }

    /// one line: where the mouse and keyboard are
    fn line(&self) -> String {
        match (self.paused, self.pointer_at) {
            (true, _) => "RustInput is paused: the mouse and keyboard stay here".into(),
            (false, Some(at)) => format!("The mouse and keyboard are on {}", self.name(at)),
            (false, None) => "The mouse and keyboard are here".into(),
        }
    }

    /// what `rustinput status` prints: where the mouse and keyboard are,
    /// and the computers
    fn text(&self) -> String {
        let mut out = self.line();
        out.push('\n');
        for (number, handle, name) in self.numbered() {
            let c = self.clients.iter().find(|c| c.0 == handle).expect("listed");
            let here = if self.pointer_at == Some(handle) {
                "  ←"
            } else {
                ""
            };
            let reachable = if c.3 { "" } else { " (not reached)" };
            out.push_str(&format!("  {number}  {name}{reachable}{here}\n"));
        }
        for c in self.clients.iter().filter(|c| !c.2) {
            out.push_str(&format!("  -  {} (switched off)\n", c.1));
        }
        out
    }

    /// the JSON of a custom module of Waybar
    fn waybar(&self) -> String {
        let (text, class) = match (self.paused, self.pointer_at) {
            (true, _) => ("⏸".to_owned(), "paused"),
            (false, Some(at)) => (format!("⌨ {}", self.name(at)), "remote"),
            (false, None) => ("⌨".to_owned(), "local"),
        };
        serde_json::json!({ "text": text, "tooltip": self.line(), "class": class, "alt": class })
            .to_string()
    }
}

/// reads what the service sends until it said where everything is (it
/// answers a new connection with all it knows)
async fn where_now(rx: &mut rustinput_ipc::AsyncFrontendEventReader) -> Result<Where, CliError> {
    where_now_with(rx, |_| {}).await
}

/// [`where_now`], showing `also` every event on the way
async fn where_now_with(
    rx: &mut rustinput_ipc::AsyncFrontendEventReader,
    mut also: impl FnMut(&FrontendEvent),
) -> Result<Where, CliError> {
    let mut now = Where::default();
    let mut listed = false;
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = rx.next().await {
            let Ok(event) = event else { break };
            listed |= matches!(event, FrontendEvent::Enumerate(_));
            now.event(&event);
            also(&event);
            // the last thing a connecting window gets
            if listed && matches!(event, FrontendEvent::PointerAt(_)) {
                break;
            }
        }
    })
    .await;
    Ok(now)
}

/// `rustinput status`: where the mouse and keyboard are and which computers
/// there are; with `waybar`, a line of JSON for Waybar at every change,
/// waiting for RustInput when it does not run
pub async fn status(waybar: bool) -> Result<(), CliError> {
    if !waybar {
        let (mut rx, _tx) = connect_async(Some(Duration::from_millis(500))).await?;
        let now = where_now(&mut rx).await?;
        print!("{}", now.text());
        return Ok(());
    }
    loop {
        match connect_async(Some(Duration::from_millis(500))).await {
            Ok((mut rx, _tx)) => {
                let mut now = where_now(&mut rx).await?;
                println!("{}", now.waybar());
                while let Some(Ok(event)) = rx.next().await {
                    if now.event(&event) {
                        println!("{}", now.waybar());
                    }
                }
            }
            Err(_) => {
                println!(
                    "{}",
                    serde_json::json!({ "text": "", "tooltip": "RustInput is not running",
                        "class": "off", "alt": "off" })
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// `rustinput go <computer>` and `rustinput home`: the mouse and keyboard
/// to a computer, by its number (as for the keyboard shortcuts) or name, or
/// back here (`None`)
pub async fn go(computer: Option<String>) -> Result<(), CliError> {
    go_to(computer).await.map(|_| ())
}

/// [`go`]; the name of the computer the mouse and keyboard went to, `None`
/// for this one
async fn go_to(computer: Option<String>) -> Result<Option<String>, CliError> {
    let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    let mut name = None;
    let target = match computer {
        None => None,
        Some(computer) => {
            let now = where_now(&mut rx).await?;
            let numbered = now.numbered();
            let wanted = computer.to_lowercase();
            let found = match computer.parse::<usize>() {
                Ok(n) => numbered.iter().find(|c| c.0 == n),
                Err(_) => numbered
                    .iter()
                    .find(|c| c.2.to_lowercase() == wanted)
                    .or_else(|| {
                        numbered
                            .iter()
                            .find(|c| c.2.to_lowercase().starts_with(&wanted))
                    }),
            };
            match found {
                Some(&(_, handle, found)) => {
                    name = Some(found.to_owned());
                    Some(handle)
                }
                None => {
                    let names: Vec<String> = numbered
                        .iter()
                        .map(|c| format!("{} {}", c.0, c.2))
                        .collect();
                    return Err(CliError::NotFound(format!(
                        "no switched on computer {computer:?}; there are: {}",
                        if names.is_empty() {
                            "none".into()
                        } else {
                            names.join(", ")
                        }
                    )));
                }
            }
        }
    };
    tx.request(FrontendRequest::GoTo(target)).await?;
    // as for send_files: the service reads it before the connection closes
    let _ = tokio::time::timeout(Duration::from_millis(300), async {
        while rx.next().await.is_some() {}
    })
    .await;
    Ok(name)
}

#[derive(Parser, Clone, Debug, PartialEq, Eq)]
#[command(name = "rustinput-cli", about = "RustInput CLI interface")]
pub struct CliArgs {
    #[command(subcommand)]
    command: CliSubcommand,
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
struct Client {
    #[arg(long)]
    hostname: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    ips: Option<Vec<IpAddr>>,
    #[arg(long)]
    enter_hook: Option<String>,
    #[arg(long)]
    leave_hook: Option<String>,
}

#[derive(Clone, Subcommand, Debug, PartialEq, Eq)]
enum CliSubcommand {
    /// add a new client
    AddClient(Client),
    /// remove an existing client
    RemoveClient { id: ClientHandle },
    /// activate a client
    Activate { id: ClientHandle },
    /// deactivate a client
    Deactivate { id: ClientHandle },
    /// list configured clients
    List,
    /// change hostname
    SetHost {
        id: ClientHandle,
        host: Option<String>,
    },
    /// change port
    SetPort { id: ClientHandle, port: u16 },
    /// set position
    SetPosition { id: ClientHandle, pos: Position },
    /// set ips
    SetIps { id: ClientHandle, ips: Vec<IpAddr> },
    /// re-enable capture
    EnableCapture,
    /// re-enable emulation
    EnableEmulation,
    /// authorize a public key
    AuthorizeKey {
        description: String,
        sha256_fingerprint: String,
    },
    /// deauthorize a public key
    RemoveAuthorizedKey { sha256_fingerprint: String },
    /// save configuration to file
    SaveConfig,
}

/// Sends files and folders to the connected devices through the running
/// service. File managers start one process per selected item, so the service
/// gathers what arrives in quick succession into one transfer.
pub async fn send_files(paths: Vec<std::path::PathBuf>) -> Result<(), CliError> {
    let paths: Vec<_> = paths
        .into_iter()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect();
    let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    tx.request(FrontendRequest::SendFiles(paths)).await?;
    // Leave the connection open until the service surely read the request,
    // reading what it sends meanwhile: closing a TCP connection (the service
    // socket on Windows) with unread data resets it, which can lose the
    // request on the other side.
    let _ = tokio::time::timeout(Duration::from_millis(300), async {
        while rx.next().await.is_some() {}
    })
    .await;
    Ok(())
}

pub async fn run(args: CliArgs) -> Result<(), CliError> {
    execute(args.command).await?;
    Ok(())
}

async fn execute(cmd: CliSubcommand) -> Result<(), CliError> {
    let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    match cmd {
        CliSubcommand::AddClient(Client {
            hostname,
            port,
            ips,
            enter_hook,
            leave_hook,
        }) => {
            tx.request(FrontendRequest::Create).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Created(handle, _, _) = e? {
                    if let Some(hostname) = hostname {
                        tx.request(FrontendRequest::UpdateHostname(handle, Some(hostname)))
                            .await?;
                    }
                    if let Some(port) = port {
                        tx.request(FrontendRequest::UpdatePort(handle, port))
                            .await?;
                    }
                    if let Some(ips) = ips {
                        tx.request(FrontendRequest::UpdateFixIps(handle, ips))
                            .await?;
                    }
                    if let Some(enter_hook) = enter_hook {
                        tx.request(FrontendRequest::UpdateEnterHook(handle, Some(enter_hook)))
                            .await?;
                    }
                    if let Some(leave_hook) = leave_hook {
                        tx.request(FrontendRequest::UpdateLeaveHook(handle, Some(leave_hook)))
                            .await?;
                    }
                    break;
                }
            }
        }
        CliSubcommand::RemoveClient { id } => tx.request(FrontendRequest::Delete(id)).await?,
        CliSubcommand::Activate { id } => tx.request(FrontendRequest::Activate(id, true)).await?,
        CliSubcommand::Deactivate { id } => {
            tx.request(FrontendRequest::Activate(id, false)).await?
        }
        CliSubcommand::List => {
            tx.request(FrontendRequest::Enumerate()).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Enumerate(clients) = e? {
                    for (handle, config, state) in clients {
                        let host = config.hostname.unwrap_or("unknown".to_owned());
                        let port = config.port;
                        let pos = config.pos;
                        let active = state.active;
                        let ips = state.ips;
                        println!(
                            "id {handle}: {host}:{port} ({pos}) active: {active}, ips: {ips:?}"
                        );
                    }
                    break;
                }
            }
        }
        CliSubcommand::SetHost { id, host } => {
            tx.request(FrontendRequest::UpdateHostname(id, host))
                .await?
        }
        CliSubcommand::SetPort { id, port } => {
            tx.request(FrontendRequest::UpdatePort(id, port)).await?
        }
        CliSubcommand::SetPosition { id, pos } => {
            tx.request(FrontendRequest::UpdatePosition(id, pos)).await?
        }
        CliSubcommand::SetIps { id, ips } => {
            tx.request(FrontendRequest::UpdateFixIps(id, ips)).await?
        }
        CliSubcommand::EnableCapture => tx.request(FrontendRequest::EnableCapture).await?,
        CliSubcommand::EnableEmulation => tx.request(FrontendRequest::EnableEmulation).await?,
        CliSubcommand::AuthorizeKey {
            description,
            sha256_fingerprint,
        } => {
            tx.request(FrontendRequest::AuthorizeKey(
                description,
                sha256_fingerprint,
            ))
            .await?
        }
        CliSubcommand::RemoveAuthorizedKey { sha256_fingerprint } => {
            tx.request(FrontendRequest::RemoveAuthorizedKey(sha256_fingerprint))
                .await?
        }
        CliSubcommand::SaveConfig => tx.request(FrontendRequest::SaveConfiguration).await?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn here() -> Where {
        Where {
            clients: vec![
                (0, "laptop".into(), true, true),
                (1, "velho".into(), false, false),
                (2, "desk".into(), true, false),
            ],
            pointer_at: None,
            paused: false,
        }
    }

    #[test]
    fn computers_are_numbered_as_for_the_shortcuts() {
        let w = here();
        let numbered: Vec<_> = w.numbered().into_iter().map(|(n, h, _)| (n, h)).collect();
        assert_eq!(
            numbered,
            [(1, 0), (2, 2)],
            "the switched off one has no number"
        );
    }

    #[test]
    fn where_the_mouse_and_keyboard_are() {
        let mut w = here();
        assert_eq!(w.line(), "The mouse and keyboard are here");
        assert!(w.waybar().contains(r#""class":"local""#));
        assert!(w.event(&FrontendEvent::PointerAt(Some(2))));
        assert_eq!(w.line(), "The mouse and keyboard are on desk");
        assert!(w.waybar().contains(r#""class":"remote""#));
        assert!(!w.event(&FrontendEvent::PointerAt(Some(2))), "no change");
        assert!(w.event(&FrontendEvent::Paused(true)));
        assert!(w.waybar().contains(r#""class":"paused""#));
        assert!(w.event(&FrontendEvent::Deleted(2)));
        assert_eq!(w.numbered().len(), 1);
    }
}
