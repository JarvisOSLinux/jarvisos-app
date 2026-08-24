//! JARVIS IPC Client
//!
//! Runs a background thread that connects to the JARVIS daemon.
//! Unix: connects via Unix domain socket.
//! Windows: reads a `.port` file and connects via TCP to 127.0.0.1.
//! Auto-reconnects every 2s on disconnect.
//!
//! IPC thread  -> Qt thread : event_rx  (mpsc Receiver<IpcEvent>)
//! Qt thread   -> IPC thread: command_tx (mpsc Sender<IpcCommand>)

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
type IpcStream = UnixStream;

#[cfg(windows)]
use std::net::TcpStream;
#[cfg(windows)]
type IpcStream = TcpStream;

const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const COMMAND_POLL: Duration = Duration::from_millis(50);

fn default_socket_path() -> String {
    let dir = jarvis_data_dir();
    format!("{}{}{}", dir, std::path::MAIN_SEPARATOR, "jarvis.sock")
}

/// Mirrors Project-JARVIS's per-platform data_dir() so sockets land in the
/// same directory the daemon uses.
pub fn jarvis_data_dir() -> String {
    if let Ok(d) = std::env::var("JARVIS_DATA_DIR") {
        return d;
    }
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            format!("{}/.local/share", home)
        });
        format!("{}/jarvis", base)
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        format!("{}/Library/Application Support/jarvis", home)
    }
    #[cfg(windows)]
    {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| {
            let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".into());
            format!("{}\\AppData\\Local", home)
        });
        format!("{}\\jarvis", base)
    }
}

pub fn socket_path() -> String {
    std::env::var("JARVIS_SOCKET").unwrap_or_else(|_| default_socket_path())
}

#[cfg(unix)]
fn connect_to_daemon() -> std::io::Result<IpcStream> {
    IpcStream::connect(socket_path())
}

#[cfg(windows)]
fn connect_to_daemon() -> std::io::Result<IpcStream> {
    let path = socket_path();
    let port_file = format!("{}.port", path);
    let port: u16 = std::fs::read_to_string(&port_file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e))?
        .trim()
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    IpcStream::connect(("127.0.0.1", port))
}

#[derive(Debug, Clone)]
pub enum IpcEvent {
    Connected,
    Disconnected,
    State(String),
    ResponseChunk {
        content: String,
        done: bool,
    },
    WakeWordDetected,
    ConfirmationRequest {
        id: String,
        tool_names: Vec<String>,
    },
    ConfirmationList(Vec<serde_json::Value>),
    Ack,
    ConfigUpdated {
        key: String,
        value: String,
    },
    ConfigError {
        key: String,
        message: String,
    },
    Error(String),
    SessionList(Vec<serde_json::Value>),
    SessionSwitched {
        session: serde_json::Value,
        messages: Vec<serde_json::Value>,
    },
    SessionError(String),
    Settings {
        confirmation_mode: String,
        wake_chime_path: String,
    },
    ProviderList(Vec<serde_json::Value>),
    ProviderError(String),
    ClientList(Vec<String>),
    DaemonShutdown {
        state: String,
        goals: Vec<serde_json::Value>,
        session_id: Option<String>,
        timestamp: f64,
    },
}

#[derive(Debug)]
pub enum IpcCommand {
    SendMessage(String),
    StartListening,
    StopListening,
    StopStream,
    ConfirmationResponse {
        id: String,
        approved: bool,
    },
    ListConfirmations,
    ApproveConfirmation {
        id: String,
    },
    DenyConfirmation {
        id: String,
    },
    PartialApproveConfirmation {
        id: String,
        approved_indices: Vec<usize>,
    },
    ApproveAllConfirmations,
    SetWakeChimePath {
        path: String,
    },
    ResetWakeChimePath,
    ListSessions,
    CreateSession {
        title: Option<String>,
    },
    SwitchSession {
        id: String,
    },
    RenameSession {
        id: String,
        title: String,
    },
    DeleteSession {
        id: String,
    },
    GetSettings,
    SetConfirmationMode {
        mode: String,
    },
    ListProviders,
    AddProvider {
        ptype: String,
        model: String,
        name: Option<String>,
        url: Option<String>,
        api_key: Option<String>,
        temperature: Option<f64>,
    },
    EditProvider {
        name: String,
        fields: serde_json::Value,
    },
    RemoveProvider {
        name: String,
    },
    ListClients,
    RequestDaemonShutdown,
}

pub struct IpcClient {
    event_rx: Receiver<IpcEvent>,
    command_tx: Sender<IpcCommand>,
}

impl IpcClient {
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel::<IpcEvent>();
        let (command_tx, command_rx) = mpsc::channel::<IpcCommand>();

        thread::Builder::new()
            .name("jarvis-ipc".into())
            .spawn(move || ipc_thread(event_tx, command_rx))
            .expect("failed to spawn IPC thread");

        Self {
            event_rx,
            command_tx,
        }
    }

    pub fn try_recv(&self) -> Option<IpcEvent> {
        match self.event_rx.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(IpcEvent::Disconnected),
        }
    }

    pub fn send_message(&self, content: String) {
        let _ = self.command_tx.send(IpcCommand::SendMessage(content));
    }

    pub fn start_listening(&self) {
        let _ = self.command_tx.send(IpcCommand::StartListening);
    }

    pub fn stop_listening(&self) {
        let _ = self.command_tx.send(IpcCommand::StopListening);
    }

    pub fn stop_stream(&self) {
        let _ = self.command_tx.send(IpcCommand::StopStream);
    }

    pub fn confirmation_response(&self, id: String, approved: bool) {
        let _ = self
            .command_tx
            .send(IpcCommand::ConfirmationResponse { id, approved });
    }

    pub fn list_confirmations(&self) {
        let _ = self.command_tx.send(IpcCommand::ListConfirmations);
    }

    pub fn approve_confirmation(&self, id: String) {
        let _ = self.command_tx.send(IpcCommand::ApproveConfirmation { id });
    }

    pub fn deny_confirmation(&self, id: String) {
        let _ = self.command_tx.send(IpcCommand::DenyConfirmation { id });
    }

    /// Approve only the listed items of a batch; the rest are denied (#187).
    pub fn partial_approve_confirmation(&self, id: String, approved_indices: Vec<usize>) {
        let _ = self
            .command_tx
            .send(IpcCommand::PartialApproveConfirmation {
                id,
                approved_indices,
            });
    }

    pub fn approve_all_confirmations(&self) {
        let _ = self.command_tx.send(IpcCommand::ApproveAllConfirmations);
    }

    pub fn set_wake_chime_path(&self, path: String) {
        let _ = self.command_tx.send(IpcCommand::SetWakeChimePath { path });
    }

    pub fn reset_wake_chime_path(&self) {
        let _ = self.command_tx.send(IpcCommand::ResetWakeChimePath);
    }

    pub fn get_settings(&self) {
        let _ = self.command_tx.send(IpcCommand::GetSettings);
    }

    pub fn set_confirmation_mode(&self, mode: String) {
        let _ = self
            .command_tx
            .send(IpcCommand::SetConfirmationMode { mode });
    }

    pub fn list_providers(&self) {
        let _ = self.command_tx.send(IpcCommand::ListProviders);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_provider(
        &self,
        ptype: String,
        model: String,
        name: Option<String>,
        url: Option<String>,
        api_key: Option<String>,
        temperature: Option<f64>,
    ) {
        let _ = self.command_tx.send(IpcCommand::AddProvider {
            ptype,
            model,
            name,
            url,
            api_key,
            temperature,
        });
    }

    pub fn edit_provider(&self, name: String, fields: serde_json::Value) {
        let _ = self
            .command_tx
            .send(IpcCommand::EditProvider { name, fields });
    }

    pub fn remove_provider(&self, name: String) {
        let _ = self.command_tx.send(IpcCommand::RemoveProvider { name });
    }

    pub fn list_clients(&self) {
        let _ = self.command_tx.send(IpcCommand::ListClients);
    }

    pub fn request_daemon_shutdown(&self) {
        let _ = self.command_tx.send(IpcCommand::RequestDaemonShutdown);
    }

    pub fn list_sessions(&self) {
        let _ = self.command_tx.send(IpcCommand::ListSessions);
    }

    pub fn create_session(&self, title: Option<String>) {
        let _ = self.command_tx.send(IpcCommand::CreateSession { title });
    }

    pub fn switch_session(&self, id: String) {
        let _ = self.command_tx.send(IpcCommand::SwitchSession { id });
    }

    pub fn rename_session(&self, id: String, title: String) {
        let _ = self
            .command_tx
            .send(IpcCommand::RenameSession { id, title });
    }

    pub fn delete_session(&self, id: String) {
        let _ = self.command_tx.send(IpcCommand::DeleteSession { id });
    }
}

impl Default for IpcClient {
    fn default() -> Self {
        Self::new()
    }
}

fn ipc_thread(event_tx: Sender<IpcEvent>, command_rx: Receiver<IpcCommand>) {
    loop {
        if let Ok(stream) = connect_to_daemon() {
            let _ = event_tx.send(IpcEvent::Connected);
            run_connected(&stream, &event_tx, &command_rx);
            let _ = event_tx.send(IpcEvent::Disconnected);
        }

        // The client dropped its sender: nothing will ever ask again, so stop
        // reconnecting instead of spinning for the life of the process.
        if let Err(TryRecvError::Disconnected) = command_rx.try_recv() {
            return;
        }

        thread::sleep(RECONNECT_DELAY);
    }
}

fn run_connected(
    stream: &IpcStream,
    event_tx: &Sender<IpcEvent>,
    command_rx: &Receiver<IpcCommand>,
) {
    let read_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("jarvis-ipc: clone failed: {e}");
            return;
        }
    };

    let (read_tx, read_rx) = mpsc::channel::<Option<IpcEvent>>();
    thread::Builder::new()
        .name("jarvis-ipc-reader".into())
        .spawn(move || {
            let reader = BufReader::new(read_stream);
            for line in reader.lines() {
                match line {
                    Ok(l) if !l.is_empty() => {
                        if read_tx.send(Some(parse_daemon_message(&l))).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {
                        let _ = read_tx.send(None);
                        break;
                    }
                }
            }
        })
        .expect("failed to spawn reader thread");

    let mut write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("jarvis-ipc: write clone failed: {e}");
            return;
        }
    };

    loop {
        loop {
            match read_rx.try_recv() {
                Ok(Some(event)) => {
                    if event_tx.send(event).is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        match command_rx.recv_timeout(COMMAND_POLL) {
            Ok(IpcCommand::SendMessage(content)) => {
                let msg = format!(
                    "{}\n",
                    serde_json::json!({"type":"message","content":content})
                );
                if write_stream.write_all(msg.as_bytes()).is_err() {
                    return;
                }
            }
            Ok(IpcCommand::StartListening) => {
                let msg = format!("{}\n", serde_json::json!({"type":"start_listening"}));
                if write_stream.write_all(msg.as_bytes()).is_err() {
                    return;
                }
            }
            Ok(IpcCommand::StopListening) => {
                let msg = format!("{}\n", serde_json::json!({"type":"stop_listening"}));
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::StopStream) => {
                let msg = format!("{}\n", serde_json::json!({"type":"stop_stream"}));
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::ConfirmationResponse { id, approved }) => {
                let msg = format!(
                    "{}\n",
                    serde_json::json!({"type":"confirmation_response","id":id,"approved":approved})
                );
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::ListConfirmations) => {
                let msg = format!("{}\n", serde_json::json!({"type":"list_confirmations"}));
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::ApproveConfirmation { id }) => {
                let msg = format!(
                    "{}\n",
                    serde_json::json!({"type":"approve_confirmation","id":id})
                );
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::PartialApproveConfirmation {
                id,
                approved_indices,
            }) => {
                let msg = format!(
                    "{}\n",
                    serde_json::json!({
                        "type": "partial_approve_confirmation",
                        "id": id,
                        "approved_indices": approved_indices,
                    })
                );
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::DenyConfirmation { id }) => {
                let msg = format!(
                    "{}\n",
                    serde_json::json!({"type":"deny_confirmation","id":id})
                );
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::ApproveAllConfirmations) => {
                let msg = format!(
                    "{}\n",
                    serde_json::json!({"type":"approve_all_confirmations"})
                );
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::SetWakeChimePath { path }) => {
                let msg = format!(
                    "{}\n",
                    serde_json::json!({"type":"set_wake_chime_path","path":path})
                );
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::ResetWakeChimePath) => {
                let msg = format!("{}\n", serde_json::json!({"type":"reset_wake_chime_path"}));
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::GetSettings) => {
                let msg = format!("{}\n", serde_json::json!({"type":"get_settings"}));
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::SetConfirmationMode { mode }) => {
                let msg = format!(
                    "{}\n",
                    serde_json::json!({"type":"set_confirmation_mode","mode":mode})
                );
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::ListProviders) => {
                let msg = format!("{}\n", serde_json::json!({"type":"list_providers"}));
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::AddProvider {
                ptype,
                model,
                name,
                url,
                api_key,
                temperature,
            }) => {
                let msg = format!(
                    "{}\n",
                    serde_json::json!({
                        "type": "add_provider",
                        "ptype": ptype,
                        "model": model,
                        "name": name,
                        "url": url,
                        "api_key": api_key,
                        "temperature": temperature,
                    })
                );
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::EditProvider { name, fields }) => {
                let msg = format!(
                    "{}\n",
                    serde_json::json!({"type":"edit_provider","name":name,"fields":fields})
                );
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::RemoveProvider { name }) => {
                let msg = format!(
                    "{}\n",
                    serde_json::json!({"type":"remove_provider","name":name})
                );
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::ListClients) => {
                let msg = format!("{}\n", serde_json::json!({"type":"list_clients"}));
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::RequestDaemonShutdown) => {
                let msg = format!("{}\n", serde_json::json!({"type":"shutdown_request"}));
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::ListSessions) => {
                let msg = format!("{}\n", serde_json::json!({"type":"list_sessions"}));
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::CreateSession { title }) => {
                let msg = format!(
                    "{}\n",
                    serde_json::json!({"type":"create_session","title":title})
                );
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::SwitchSession { id }) => {
                let msg = format!("{}\n", serde_json::json!({"type":"switch_session","id":id}));
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::RenameSession { id, title }) => {
                let msg = format!(
                    "{}\n",
                    serde_json::json!({"type":"rename_session","id":id,"title":title})
                );
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Ok(IpcCommand::DeleteSession { id }) => {
                let msg = format!("{}\n", serde_json::json!({"type":"delete_session","id":id}));
                let _ = write_stream.write_all(msg.as_bytes());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn parse_daemon_message(line: &str) -> IpcEvent {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return IpcEvent::Error(format!("invalid JSON: {line}"));
    };

    match value.get("type").and_then(|t| t.as_str()) {
        Some("state") => IpcEvent::State(
            value
                .get("state")
                .and_then(|s| s.as_str())
                .unwrap_or("idle")
                .to_string(),
        ),
        Some("response") => IpcEvent::ResponseChunk {
            content: value
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            done: value.get("done").and_then(|d| d.as_bool()).unwrap_or(true),
        },
        Some("wake_word_detected") => IpcEvent::WakeWordDetected,
        Some("confirmation_request") => IpcEvent::ConfirmationRequest {
            id: value
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            tool_names: value
                .get("tools")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        },
        Some("confirmation_list") => IpcEvent::ConfirmationList(
            value
                .get("confirmations")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default(),
        ),
        Some("config_updated") => IpcEvent::ConfigUpdated {
            key: value
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            value: value
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Some("config_error") => IpcEvent::ConfigError {
            key: value
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            message: value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Some("ack") => IpcEvent::Ack,
        Some("error") => IpcEvent::Error(
            value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown error")
                .to_string(),
        ),
        Some("session_list") => IpcEvent::SessionList(
            value
                .get("sessions")
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default(),
        ),
        Some("session_switched") => IpcEvent::SessionSwitched {
            session: value
                .get("session")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            messages: value
                .get("messages")
                .and_then(|m| m.as_array())
                .cloned()
                .unwrap_or_default(),
        },
        Some("session_error") => IpcEvent::SessionError(
            value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown session error")
                .to_string(),
        ),
        Some("settings") => IpcEvent::Settings {
            confirmation_mode: value
                .get("confirmation_mode")
                .and_then(|v| v.as_str())
                .unwrap_or("smart")
                .to_string(),
            wake_chime_path: value
                .get("wake_chime_path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Some("provider_list") => IpcEvent::ProviderList(
            value
                .get("providers")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default(),
        ),
        Some("provider_error") => IpcEvent::ProviderError(
            value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown provider error")
                .to_string(),
        ),
        Some("client_list") => IpcEvent::ClientList(
            value
                .get("clients")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| c.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        ),
        Some("DAEMON_SHUTDOWN") => IpcEvent::DaemonShutdown {
            state: value
                .get("state")
                .and_then(|s| s.as_str())
                .unwrap_or("idle")
                .to_string(),
            goals: value
                .get("goals")
                .and_then(|g| g.as_array())
                .cloned()
                .unwrap_or_default(),
            session_id: value
                .get("session_id")
                .and_then(|s| s.as_str())
                .map(String::from),
            timestamp: value
                .get("timestamp")
                .and_then(|t| t.as_f64())
                .unwrap_or(0.0),
        },
        Some("ping") | Some("pong") => IpcEvent::State("idle".into()),
        other => IpcEvent::Error(format!("unknown type: {other:?}")),
    }
}

// These drive a real AF_UNIX socket, so they are Unix-only: on Windows the
// transport is TCP-on-localhost plus a port file (see connect_to_daemon's
// cfg(windows) arm), which this harness does not stand up. The Windows CI job
// therefore COMPILE-checks the Windows IPC path but does not round-trip it --
// a known gap, and the reason the job still earns its place is that the break
// which shipped here before (the time/cookie pin) was a compile break.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::os::unix::net::UnixStream as TestUnixStream;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    // JARVIS_SOCKET is a process-global env var, but cargo test runs #[test]
    // functions concurrently on separate threads within the same process --
    // two real-socket tests mutating it at once race (one test's IpcClient
    // reads the other's path) and hang forever waiting on a connection that
    // never arrives. Every test that touches JARVIS_SOCKET must hold this
    // for its full duration, not just around the set/remove calls.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn unique_sock_path(name: &str) -> String {
        format!(
            "/tmp/jarvis-ipc-test-{}-{}-{}.sock",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    fn read_line(reader: &mut BufReader<TestUnixStream>) -> serde_json::Value {
        let mut buf = String::new();
        reader.read_line(&mut buf).unwrap();
        serde_json::from_str(buf.trim_end()).unwrap()
    }

    fn wait_for_event(client: &IpcClient, pred: impl Fn(&IpcEvent) -> bool) -> IpcEvent {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(ev) = client.try_recv() {
                if pred(&ev) {
                    return ev;
                }
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for expected IpcEvent");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // Exercises the real IpcClient background threads end-to-end over an
    // actual Unix socket -- not a mock -- so it catches wire-format drift
    // between this client and jarvis's confirmation-query protocol.
    #[test]
    fn confirmation_commands_round_trip_over_real_socket() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = unique_sock_path("confirmations");
        let _ = std::fs::remove_file(&path);
        std::env::set_var("JARVIS_SOCKET", &path);

        let listener = UnixListener::bind(&path).unwrap();
        let client = IpcClient::new();

        let (stream, _) = listener.accept().unwrap();
        let mut write_half = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);

        wait_for_event(&client, |e| matches!(e, IpcEvent::Connected));

        client.list_confirmations();
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({"type": "list_confirmations"})
        );

        client.approve_confirmation("req1".into());
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({"type": "approve_confirmation", "id": "req1"})
        );

        client.deny_confirmation("req2".into());
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({"type": "deny_confirmation", "id": "req2"})
        );

        // Per-task approve (#187): the daemon denies the unlisted items, so the
        // indices must reach the wire exactly as chosen.
        client.partial_approve_confirmation("req5".into(), vec![0, 2]);
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({
                "type": "partial_approve_confirmation",
                "id": "req5",
                "approved_indices": [0, 2],
            })
        );

        client.approve_all_confirmations();
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({"type": "approve_all_confirmations"})
        );

        write_half
            .write_all(
                b"{\"type\":\"confirmation_list\",\"confirmations\":\
                   [{\"id\":\"req1\",\"tool_names\":[\"fs.delete\"],\"created_at\":1.0}]}\n",
            )
            .unwrap();
        match wait_for_event(&client, |e| matches!(e, IpcEvent::ConfirmationList(_))) {
            IpcEvent::ConfirmationList(list) => {
                assert_eq!(list.len(), 1);
                assert_eq!(list[0]["id"], "req1");
            }
            _ => unreachable!(),
        }

        // Covers the PARSE path only. This assertion passes against the
        // pre-fix tree too, because parsing was never broken -- lib.rs
        // discarded the parsed event instead of emitting it. Kept as
        // regression cover for the wire format; the forwarding fix itself is
        // not provable here (it needs a Tauri AppHandle) and was verified by
        // inspection of the emit arm.
        write_half
            .write_all(b"{\"type\":\"error\",\"message\":\"tool exploded\"}\n")
            .unwrap();
        match wait_for_event(&client, |e| matches!(e, IpcEvent::Error(_))) {
            IpcEvent::Error(message) => assert!(message.contains("tool exploded")),
            _ => unreachable!(),
        }

        write_half
            .write_all(b"{\"type\":\"ack\",\"message\":\"Approved req1\"}\n")
            .unwrap();
        assert!(matches!(
            wait_for_event(&client, |e| matches!(e, IpcEvent::Ack)),
            IpcEvent::Ack
        ));

        write_half
            .write_all(b"{\"type\":\"confirmation_request\",\"id\":\"req3\",\"tools\":[\"net.request\"]}\n")
            .unwrap();
        match wait_for_event(&client, |e| {
            matches!(e, IpcEvent::ConfirmationRequest { .. })
        }) {
            IpcEvent::ConfirmationRequest { id, tool_names } => {
                assert_eq!(id, "req3");
                assert_eq!(tool_names, vec!["net.request".to_string()]);
            }
            _ => unreachable!(),
        }

        client.set_wake_chime_path("/tmp/my-chime.wav".into());
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({"type": "set_wake_chime_path", "path": "/tmp/my-chime.wav"})
        );

        write_half
            .write_all(
                b"{\"type\":\"config_updated\",\"key\":\"WAKE_CHIME_PATH\",\"value\":\"/tmp/my-chime.wav\"}\n",
            )
            .unwrap();
        match wait_for_event(&client, |e| matches!(e, IpcEvent::ConfigUpdated { .. })) {
            IpcEvent::ConfigUpdated { key, value } => {
                assert_eq!(key, "WAKE_CHIME_PATH");
                assert_eq!(value, "/tmp/my-chime.wav");
            }
            _ => unreachable!(),
        }

        write_half
            .write_all(
                b"{\"type\":\"config_error\",\"key\":\"WAKE_CHIME_PATH\",\"message\":\"not a file: x\"}\n",
            )
            .unwrap();
        match wait_for_event(&client, |e| matches!(e, IpcEvent::ConfigError { .. })) {
            IpcEvent::ConfigError { key, message } => {
                assert_eq!(key, "WAKE_CHIME_PATH");
                assert_eq!(message, "not a file: x");
            }
            _ => unreachable!(),
        }

        std::env::remove_var("JARVIS_SOCKET");
        let _ = std::fs::remove_file(&path);
    }

    // Same rationale as confirmation_commands_round_trip_over_real_socket --
    // proves the settings-panel (jarvisos-app#12) wire format byte-for-byte
    // against the actual IpcClient background threads.
    #[test]
    fn settings_and_provider_commands_round_trip_over_real_socket() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = unique_sock_path("settings");
        let _ = std::fs::remove_file(&path);
        std::env::set_var("JARVIS_SOCKET", &path);

        let listener = UnixListener::bind(&path).unwrap();
        let client = IpcClient::new();

        let (stream, _) = listener.accept().unwrap();
        let mut write_half = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);

        wait_for_event(&client, |e| matches!(e, IpcEvent::Connected));

        client.get_settings();
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({"type": "get_settings"})
        );

        write_half
            .write_all(
                b"{\"type\":\"settings\",\"confirmation_mode\":\"smart\",\"wake_chime_path\":\"/x.wav\"}\n",
            )
            .unwrap();
        match wait_for_event(&client, |e| matches!(e, IpcEvent::Settings { .. })) {
            IpcEvent::Settings {
                confirmation_mode,
                wake_chime_path,
            } => {
                assert_eq!(confirmation_mode, "smart");
                assert_eq!(wake_chime_path, "/x.wav");
            }
            _ => unreachable!(),
        }

        client.set_confirmation_mode("ask_all".into());
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({"type": "set_confirmation_mode", "mode": "ask_all"})
        );

        client.reset_wake_chime_path();
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({"type": "reset_wake_chime_path"})
        );

        client.list_providers();
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({"type": "list_providers"})
        );

        client.add_provider(
            "ollama".into(),
            "qwen3:8b".into(),
            Some("my-ollama".into()),
            None,
            None,
            None,
        );
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({
                "type": "add_provider",
                "ptype": "ollama",
                "model": "qwen3:8b",
                "name": "my-ollama",
                "url": null,
                "api_key": null,
                "temperature": null,
            })
        );

        client.edit_provider(
            "my-ollama".into(),
            serde_json::json!({"model": "qwen3:14b"}),
        );
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({
                "type": "edit_provider",
                "name": "my-ollama",
                "fields": {"model": "qwen3:14b"},
            })
        );

        client.remove_provider("my-ollama".into());
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({"type": "remove_provider", "name": "my-ollama"})
        );

        write_half
            .write_all(
                b"{\"type\":\"provider_list\",\"providers\":[{\"name\":\"my-ollama\",\"type\":\"ollama\"}]}\n",
            )
            .unwrap();
        match wait_for_event(&client, |e| matches!(e, IpcEvent::ProviderList(_))) {
            IpcEvent::ProviderList(list) => {
                assert_eq!(list.len(), 1);
                assert_eq!(list[0]["name"], "my-ollama");
            }
            _ => unreachable!(),
        }

        write_half
            .write_all(b"{\"type\":\"provider_error\",\"message\":\"boom\"}\n")
            .unwrap();
        match wait_for_event(&client, |e| matches!(e, IpcEvent::ProviderError(_))) {
            IpcEvent::ProviderError(message) => assert_eq!(message, "boom"),
            _ => unreachable!(),
        }

        std::env::remove_var("JARVIS_SOCKET");
        let _ = std::fs::remove_file(&path);
    }

    // Same rationale as the two tests above -- proves jarvisos-app#17's
    // list_clients/shutdown_request/DAEMON_SHUTDOWN wire format against the
    // real IpcClient background threads, not a reimplementation.
    #[test]
    fn shutdown_protocol_round_trips_over_real_socket() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = unique_sock_path("shutdown");
        let _ = std::fs::remove_file(&path);
        std::env::set_var("JARVIS_SOCKET", &path);

        let listener = UnixListener::bind(&path).unwrap();
        let client = IpcClient::new();

        let (stream, _) = listener.accept().unwrap();
        let mut write_half = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);

        wait_for_event(&client, |e| matches!(e, IpcEvent::Connected));

        client.list_clients();
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({"type": "list_clients"})
        );

        write_half
            .write_all(b"{\"type\":\"client_list\",\"clients\":[\"jarvis-app\",\"TUI\"]}\n")
            .unwrap();
        match wait_for_event(&client, |e| matches!(e, IpcEvent::ClientList(_))) {
            IpcEvent::ClientList(clients) => {
                assert_eq!(clients, vec!["jarvis-app".to_string(), "TUI".to_string()])
            }
            _ => unreachable!(),
        }

        client.request_daemon_shutdown();
        assert_eq!(
            read_line(&mut reader),
            serde_json::json!({"type": "shutdown_request"})
        );

        write_half
            .write_all(
                b"{\"type\":\"DAEMON_SHUTDOWN\",\"state\":\"idle\",\"goals\":[],\
                   \"session_id\":\"sess-1\",\"timestamp\":1700000000.5}\n",
            )
            .unwrap();
        match wait_for_event(&client, |e| matches!(e, IpcEvent::DaemonShutdown { .. })) {
            IpcEvent::DaemonShutdown {
                state,
                goals,
                session_id,
                timestamp,
            } => {
                assert_eq!(state, "idle");
                assert!(goals.is_empty());
                assert_eq!(session_id, Some("sess-1".to_string()));
                assert_eq!(timestamp, 1700000000.5);
            }
            _ => unreachable!(),
        }

        std::env::remove_var("JARVIS_SOCKET");
        let _ = std::fs::remove_file(&path);
    }
}
