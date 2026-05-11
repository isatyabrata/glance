//! Long-lived bridge to Chrome via our native-messaging host.
//!
//! Architecture
//! ------------
//!
//! ```text
//! Chrome extension  <-stdio (4-byte LE framed JSON)->  native host (Node)
//!                                                          │
//!                                                          ▼  (line-delimited JSON over unix socket)
//!                                              this module (server)
//!                                                          ▲
//!                                                          │
//!                                                  glance.chrome tool
//! ```
//!
//! We bind a unix socket on first use. Whichever process binds first owns the
//! bridge for the life of that socket. Subsequent glance instances that try to
//! bind get `EADDRINUSE` and surface an error to the caller — only one MCP
//! server can drive Chrome at a time. That matches how native messaging works
//! anyway: Chrome only spawns one host per extension connection.
//!
//! Wire format on the unix socket (both directions): one JSON object per line.
//! - glance → host:        `{"id":N,"method":"...","params":{...}}`
//! - host → glance reply:  `{"id":N,"result":...}`  or  `{"id":N,"error":"..."}`
//! - host → glance status: `{"kind":"host_hello"|"host_status"|...}`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use once_cell::sync::OnceCell;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Mutex, Notify};

const CONNECT_WAIT_MS: u64 = 1500;
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// Default path on macOS/Linux. Override via `GLANCE_CHROME_SOCKET`.
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("GLANCE_CHROME_SOCKET") {
        return PathBuf::from(p);
    }
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    PathBuf::from(tmp).join(format!("glance-chrome-{}.sock", user))
}

struct Bridge {
    write_half: Mutex<Option<tokio::net::unix::OwnedWriteHalf>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: Mutex<u64>,
    connected: Notify,
}

static BRIDGE: OnceCell<Arc<Bridge>> = OnceCell::new();

fn bridge() -> Arc<Bridge> {
    BRIDGE
        .get_or_init(|| {
            Arc::new(Bridge {
                write_half: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                next_id: Mutex::new(1),
                connected: Notify::new(),
            })
        })
        .clone()
}

/// Start (or reuse) the unix-socket listener. Idempotent — first caller binds.
pub async fn ensure_started() -> Result<PathBuf> {
    static STARTED: OnceCell<()> = OnceCell::new();
    let path = socket_path();
    if STARTED.get().is_some() {
        return Ok(path);
    }

    // Try a fresh bind. If a stale socket file exists from a previous crashed
    // instance, attempt to remove it and rebind. If a *live* peer is already
    // bound, that means another glance is owning the channel — surface the
    // error rather than silently swiping it.
    let _ = tokio::fs::remove_file(&path).await;
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind {}", path.display()))?;

    let _ = STARTED.set(());
    let p2 = path.clone();
    tokio::spawn(async move {
        accept_loop(listener, p2).await;
    });
    Ok(path)
}

async fn accept_loop(listener: UnixListener, path: PathBuf) {
    tracing::info!(socket = %path.display(), "chrome bridge listening");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tracing::info!("chrome native host connected");
                handle_host(stream).await;
                tracing::warn!("chrome native host disconnected; awaiting reconnect");
            }
            Err(e) => {
                tracing::error!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

async fn handle_host(stream: UnixStream) {
    let (rd, wr) = stream.into_split();
    let b = bridge();
    {
        let mut g = b.write_half.lock().await;
        *g = Some(wr);
    }
    b.connected.notify_waiters();

    let mut lines = BufReader::new(rd).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, raw = %line, "bad json from host");
                continue;
            }
        };
        // Status frames from the host (host_hello, host_status, hello).
        if msg.get("kind").is_some() && msg.get("id").is_none() {
            tracing::debug!(?msg, "host status");
            continue;
        }
        if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
            let sender = {
                let mut g = b.pending.lock().await;
                g.remove(&id)
            };
            if let Some(tx) = sender {
                let _ = tx.send(msg);
            }
        }
    }

    // Clear writer so subsequent calls block until reconnect.
    let mut g = b.write_half.lock().await;
    *g = None;
}

async fn next_id() -> u64 {
    let b = bridge();
    let mut g = b.next_id.lock().await;
    let id = *g;
    *g = g.wrapping_add(1);
    id
}

async fn await_connection(timeout_ms: u64) -> Result<()> {
    let b = bridge();
    {
        let g = b.write_half.lock().await;
        if g.is_some() {
            return Ok(());
        }
    }
    tokio::select! {
        _ = b.connected.notified() => Ok(()),
        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
            Err(anyhow!(
                "chrome bridge: no native host connected within {}ms (is the Glance Chrome extension installed and Chrome running?)",
                timeout_ms
            ))
        }
    }
}

/// Send a JSON-RPC request to the extension and await its reply.
pub async fn call(method: &str, params: Value) -> Result<Value> {
    ensure_started().await?;
    await_connection(CONNECT_WAIT_MS).await?;

    let id = next_id().await;
    let payload = json!({ "id": id, "method": method, "params": params });
    let line = serde_json::to_string(&payload)? + "\n";

    let (tx, rx) = oneshot::channel();
    {
        let b = bridge();
        let mut g = b.pending.lock().await;
        g.insert(id, tx);
    }

    {
        let b = bridge();
        let mut g = b.write_half.lock().await;
        let wr = g
            .as_mut()
            .ok_or_else(|| anyhow!("chrome bridge: not connected"))?;
        wr.write_all(line.as_bytes()).await?;
        wr.flush().await?;
    }

    let resp = tokio::time::timeout(Duration::from_millis(REQUEST_TIMEOUT_MS), rx)
        .await
        .map_err(|_| anyhow!("chrome bridge: request {} timed out", method))?
        .map_err(|_| anyhow!("chrome bridge: response channel dropped"))?;

    if let Some(err) = resp.get("error") {
        return Err(anyhow!(
            "chrome.{} failed: {}",
            method,
            err.as_str().unwrap_or(&err.to_string())
        ));
    }
    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}
