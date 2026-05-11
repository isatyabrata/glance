//! Glance Chrome Bridge — native messaging host (Rust drop-in replacement
//! for the original Node.js host).
//!
//! Spawned by Chrome via the manifest at
//!   ~/Library/Application Support/Google/Chrome/NativeMessagingHosts/com.glance.chrome.json
//!
//! Reads/writes 4-byte LE length-prefixed JSON frames on stdio with the
//! extension, and proxies them line-delimited over a unix socket to the
//! glance MCP server. The MCP server is the listener; we are the client.
//!
//! stdout MUST only contain native messaging frames — anything else closes
//! Chrome's pipe. All logs go to stderr (which Chrome buffers safely) and to
//! `~/.glance/chrome-host.log`.
//!
//! This binary is a strict behavioural clone of the previous
//! `assets/chrome-bridge/host/host.js`:
//!   * env `GLANCE_CHROME_SOCKET` overrides socket path; default is
//!     `${TMPDIR or /tmp}/glance-chrome-${USER}.sock`.
//!   * heartbeat `~/.glance/chrome-bridge.alive` written every 5 s, removed on
//!     graceful exit.
//!   * 1.5 s reconnect delay; queues outbound up to 1000 messages.
//!   * suppresses incoming `pong` / `hello` extension frames (chatty).

use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::fs::OpenOptions;
use tokio::io::{
    AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter,
};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;
use tracing::{error, info, warn};
use tracing_subscriber::fmt::writer::MakeWriterExt;

const RECONNECT_DELAY_MS: u64 = 1500;
const HEARTBEAT_INTERVAL_MS: u64 = 5_000;
const MAX_PENDING_OUTBOUND: usize = 1000;

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("GLANCE_CHROME_SOCKET") {
        return PathBuf::from(p);
    }
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    PathBuf::from(tmp).join(format!("glance-chrome-{}.sock", user))
}

fn log_path() -> PathBuf {
    home_dir().join(".glance/chrome-host.log")
}

fn heartbeat_path() -> PathBuf {
    home_dir().join(".glance/chrome-bridge.alive")
}

/// Configure tracing-subscriber to emit to stderr **and** append to
/// `~/.glance/chrome-host.log`. Best-effort — if the log file can't be opened
/// (e.g. fs permissions), fall back to stderr only.
fn init_logging() {
    let log_file = log_path();
    if let Some(parent) = log_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file_writer = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .ok();

    let stderr = std::io::stderr;
    if let Some(file) = file_writer {
        let writer = stderr.and(file);
        tracing_subscriber::fmt()
            .with_writer(writer)
            .with_target(false)
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_writer(stderr)
            .with_target(false)
            .with_ansi(false)
            .init();
    }
}

// ---------- stdio framing ----------
//
// Native messaging frames: 4-byte little-endian length + UTF-8 JSON body.
// We serialize all stdout writes through a Mutex<BufWriter<Stdout>> so that
// the length prefix and body always land in the same write+flush sequence and
// can never interleave with another writer.

type StdoutWriter = Arc<Mutex<BufWriter<tokio::io::Stdout>>>;

async fn write_frame(stdout: &StdoutWriter, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    if body.len() > u32::MAX as usize {
        anyhow::bail!("frame body too large: {} bytes", body.len());
    }
    let mut guard = stdout.lock().await;
    guard.write_all(&(body.len() as u32).to_le_bytes()).await?;
    guard.write_all(&body).await?;
    guard.flush().await?;
    Ok(())
}

/// Read length-prefixed frames from stdin and forward via `out_tx` until EOF.
async fn run_stdin_reader(out_tx: mpsc::UnboundedSender<Value>) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut len_buf = [0u8; 4];
    loop {
        match stdin.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                info!("stdin closed; exiting");
                return Ok(());
            }
            Err(e) => return Err(e).context("read frame length"),
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        // Sanity guard: Chrome's hard limit is 1 MB per message. Allow 8 MB
        // for headroom but reject obvious garbage.
        if len > 8 * 1024 * 1024 {
            error!("oversized frame ({} bytes); aborting", len);
            anyhow::bail!("oversized frame");
        }
        let mut body = vec![0u8; len];
        stdin.read_exact(&mut body).await.context("read frame body")?;
        match serde_json::from_slice::<Value>(&body) {
            Ok(msg) => {
                if out_tx.send(msg).is_err() {
                    return Ok(()); // receiver dropped; we're shutting down
                }
            }
            Err(e) => {
                warn!("bad frame from extension: {}", e);
                continue;
            }
        }
    }
}

// ---------- unix socket client ----------

/// State shared between the socket task and the main loop, used to publish
/// "are we connected to glance" for the heartbeat file.
#[derive(Clone)]
struct SocketState {
    connected: Arc<std::sync::atomic::AtomicBool>,
}

impl SocketState {
    fn new() -> Self {
        Self {
            connected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
    fn set(&self, v: bool) {
        self.connected
            .store(v, std::sync::atomic::Ordering::Relaxed);
    }
    fn get(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Run the unix-socket client task. Owns a queue of outbound messages from
/// the extension and a stdout writer for incoming socket frames. Loops
/// forever, reconnecting after `RECONNECT_DELAY_MS` whenever the socket drops
/// or the connect attempt fails.
async fn run_socket_client(
    socket: PathBuf,
    stdout: StdoutWriter,
    mut from_ext_rx: mpsc::UnboundedReceiver<Value>,
    state: SocketState,
) -> Result<()> {
    // Pending queue, used while we are not connected. Bounded — drop the
    // oldest entry past `MAX_PENDING_OUTBOUND` (matches the Node host).
    let mut pending: std::collections::VecDeque<String> =
        std::collections::VecDeque::new();

    loop {
        info!("connecting to glance socket {}", socket.display());
        let stream = match UnixStream::connect(&socket).await {
            Ok(s) => s,
            Err(e) => {
                warn!("socket connect failed: {}; retry in {}ms", e, RECONNECT_DELAY_MS);
                // Drain anything that arrived while we were trying so the
                // queue doesn't grow without bound.
                while let Ok(msg) = from_ext_rx.try_recv() {
                    if pending.len() >= MAX_PENDING_OUTBOUND {
                        pending.pop_front();
                    }
                    pending.push_back(serde_json::to_string(&msg).unwrap_or_default() + "\n");
                }
                sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
                continue;
            }
        };

        info!("socket connected");
        state.set(true);
        let _ = write_frame(
            &stdout,
            &serde_json::json!({"kind": "host_status", "connected": true}),
        )
        .await;

        let (read_half, write_half) = stream.into_split();
        let mut writer = write_half;
        let mut reader = BufReader::new(read_half);

        // Identify ourselves to glance.
        let hello = serde_json::json!({
            "kind": "hello",
            "role": "chrome-host",
            "pid": process::id()
        });
        let hello_line = serde_json::to_string(&hello).unwrap() + "\n";
        if let Err(e) = writer.write_all(hello_line.as_bytes()).await {
            warn!("hello write failed: {}", e);
            state.set(false);
            let _ = write_frame(
                &stdout,
                &serde_json::json!({"kind": "host_status", "connected": false}),
            )
            .await;
            sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
            continue;
        }

        // Drain queued outbound.
        while let Some(line) = pending.pop_front() {
            if let Err(e) = writer.write_all(line.as_bytes()).await {
                warn!("drain write failed: {}", e);
                pending.push_front(line);
                break;
            }
        }

        // Now multiplex: forward extension→socket and socket→extension.
        let mut line_buf = String::new();
        let disconnected = loop {
            tokio::select! {
                // socket → extension
                read = reader.read_line(&mut line_buf) => {
                    match read {
                        Ok(0) => break "eof",
                        Ok(_) => {
                            let trimmed = line_buf.trim();
                            if !trimmed.is_empty() {
                                match serde_json::from_str::<Value>(trimmed) {
                                    Ok(msg) => {
                                        if let Err(e) = write_frame(&stdout, &msg).await {
                                            error!("stdout write failed: {}", e);
                                            // Lost stdout = Chrome's pipe is gone.
                                            return Err(e);
                                        }
                                    }
                                    Err(e) => warn!("bad line from glance: {}", e),
                                }
                            }
                            line_buf.clear();
                        }
                        Err(e) => {
                            warn!("socket read error: {}", e);
                            break "err";
                        }
                    }
                }
                // extension → socket
                msg = from_ext_rx.recv() => {
                    match msg {
                        Some(v) => {
                            let mut line = serde_json::to_string(&v).unwrap_or_default();
                            line.push('\n');
                            if let Err(e) = writer.write_all(line.as_bytes()).await {
                                warn!("socket write failed: {}; requeueing", e);
                                if pending.len() >= MAX_PENDING_OUTBOUND {
                                    pending.pop_front();
                                }
                                pending.push_back(line);
                                break "err";
                            }
                        }
                        None => {
                            // stdin reader signalled exit.
                            info!("extension channel closed; shutting down socket task");
                            return Ok(());
                        }
                    }
                }
            }
        };

        info!("socket disconnected ({}); will retry in {}ms", disconnected, RECONNECT_DELAY_MS);
        state.set(false);
        let _ = write_frame(
            &stdout,
            &serde_json::json!({"kind": "host_status", "connected": false}),
        )
        .await;
        sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
    }
}

// ---------- heartbeat ----------

async fn write_heartbeat(socket: &Path, glance_connected: bool) {
    let path = heartbeat_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            // Best-effort; never crash the process for fs hiccups.
            warn!("heartbeat mkdir failed: {}", e);
            return;
        }
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let body = serde_json::json!({
        "ts": now,
        "pid": process::id(),
        "socket": socket.to_string_lossy(),
        "glance_connected": glance_connected,
    });
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .await
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(&bytes).await {
                warn!("heartbeat write failed: {}", e);
            }
        }
        Err(e) => warn!("heartbeat open failed: {}", e),
    }
}

fn clear_heartbeat() {
    let _ = std::fs::remove_file(heartbeat_path());
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    init_logging();

    let socket = socket_path();
    info!(
        "glance chrome native host starting; pid {} socket {}",
        process::id(),
        socket.display()
    );

    // Stdout writer shared between socket task, host_hello, and heartbeat
    // status frames. Wrapping in BufWriter+Mutex guarantees frame writes are
    // atomic w.r.t. each other.
    let stdout: StdoutWriter = Arc::new(Mutex::new(BufWriter::new(tokio::io::stdout())));

    // Greeting frame so the extension can confirm host bring-up.
    write_frame(
        &stdout,
        &serde_json::json!({
            "kind": "host_hello",
            "pid": process::id(),
            "socket": socket.to_string_lossy(),
        }),
    )
    .await?;

    // Best-effort initial heartbeat (matches the Node host).
    write_heartbeat(&socket, false).await;

    let (out_tx, out_rx) = mpsc::unbounded_channel::<Value>();
    let state = SocketState::new();

    // stdin reader → drops `pong` / `hello` extension frames the same way the
    // Node host did (chatty, never need to round-trip to glance).
    let stdin_tx = out_tx.clone();
    let stdin_task = tokio::spawn(async move {
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<Value>();
        let reader_handle = tokio::spawn(run_stdin_reader(raw_tx));
        while let Some(msg) = raw_rx.recv().await {
            let kind = msg.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if kind == "pong" || kind == "hello" {
                continue;
            }
            if stdin_tx.send(msg).is_err() {
                break;
            }
        }
        // Drain reader; closes on stdin EOF.
        let _ = reader_handle.await;
        // Dropping out_tx here signals the socket task to exit.
    });

    // Socket task.
    let socket_state = state.clone();
    let socket_stdout = stdout.clone();
    let socket_path_clone = socket.clone();
    let socket_task = tokio::spawn(async move {
        if let Err(e) =
            run_socket_client(socket_path_clone, socket_stdout, out_rx, socket_state).await
        {
            error!("socket task aborted: {}", e);
        }
    });

    // Heartbeat ticker.
    let hb_socket = socket.clone();
    let hb_state = state.clone();
    let heartbeat_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(HEARTBEAT_INTERVAL_MS));
        // First tick fires immediately; we already wrote one synchronously
        // above, so consume it.
        tick.tick().await;
        loop {
            tick.tick().await;
            write_heartbeat(&hb_socket, hb_state.get()).await;
        }
    });

    // Signal handling — graceful shutdown on SIGINT / SIGTERM / SIGHUP.
    let shutdown = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigint = signal(SignalKind::interrupt()).ok();
            let mut sigterm = signal(SignalKind::terminate()).ok();
            let mut sighup = signal(SignalKind::hangup()).ok();
            tokio::select! {
                _ = async { if let Some(s) = sigint.as_mut() { s.recv().await; } else { std::future::pending::<()>().await; } } => "SIGINT",
                _ = async { if let Some(s) = sigterm.as_mut() { s.recv().await; } else { std::future::pending::<()>().await; } } => "SIGTERM",
                _ = async { if let Some(s) = sighup.as_mut() { s.recv().await; } else { std::future::pending::<()>().await; } } => "SIGHUP",
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            "ctrl_c"
        }
    };

    tokio::select! {
        _ = stdin_task => {
            info!("stdin task ended");
        }
        _ = socket_task => {
            info!("socket task ended");
        }
        _ = heartbeat_task => {
            // never returns under normal operation
        }
        sig = shutdown => {
            info!("got signal {}; shutting down", sig);
        }
    }

    clear_heartbeat();
    Ok(())
}
