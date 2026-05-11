//! Install / inspect / remove the glance Chrome bridge (extension files,
//! native messaging host binary, and the Chrome NativeMessagingHosts
//! manifest).
//!
//! Shared by both the `glance chrome ...` CLI and the Glance app's GUI
//! (`glance-app/src-tauri/src/commands.rs`). Treat this as the single source
//! of truth for paths and file contents.
//!
//! As of v0.42 the native host is the Rust binary `glance-chrome-host`
//! (built from `src/bin/glance_chrome_host.rs`) and is expected to live on
//! the user's PATH (typically `~/.cargo/bin/glance-chrome-host` after
//! `cargo install --path .`). We no longer ship a Node.js host script.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;

pub const HOST_NAME: &str = "com.glance.chrome";

/// Deterministic extension ID derived from the RSA public key embedded as
/// `key` in `assets/chrome-bridge/extension/manifest.json`. Changes require
/// regenerating the keypair and updating both this constant and the manifest
/// `key` field.
pub const FIXED_EXTENSION_ID: &str = "eofgbpadckhmkhhbbhekngmkgagfifhe";

/// Path the GUI / native host both watch to publish "the bridge is alive".
/// The host writes its pid + a fresh timestamp every few seconds; absence or
/// staleness (> ~15 s) means "not connected".
pub fn heartbeat_path() -> Result<PathBuf> {
    Ok(home()?.join(".glance/chrome-bridge.alive"))
}

const EXT_MANIFEST: &str = include_str!("../../assets/chrome-bridge/extension/manifest.json");
const EXT_BACKGROUND: &str = include_str!("../../assets/chrome-bridge/extension/background.js");
const EXT_POPUP_HTML: &str = include_str!("../../assets/chrome-bridge/extension/popup.html");
const EXT_POPUP_JS: &str = include_str!("../../assets/chrome-bridge/extension/popup.js");

/// Native host binary name. Must match the `[[bin]]` entry in Cargo.toml.
const HOST_BIN_NAME: &str = "glance-chrome-host";

// Example adapters seeded into ~/.glance/chrome-adapters/ on first install.
// Existing files are NOT overwritten so the user's own edits are safe.
// Most adapters live in `examples/chrome-adapters/` and are imported via
// `glance chrome import`. Public-safe natively-written ones are seeded here.
const EXAMPLE_ADAPTERS: &[(&str, &str)] = &[(
    "x_post",
    include_str!("../../examples/chrome-adapters-native/x_post.yaml"),
)];

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("cannot resolve $HOME"))
}

pub fn bridge_root() -> Result<PathBuf> {
    Ok(home()?.join(".glance/chrome-bridge"))
}

pub fn ext_dir() -> Result<PathBuf> {
    Ok(bridge_root()?.join("extension"))
}

/// Legacy "host" directory under `~/.glance/chrome-bridge/host`. Kept for
/// backwards-compat status reporting and as a stable spot for log artifacts;
/// no scripts are written there any more.
pub fn host_dir() -> Result<PathBuf> {
    Ok(bridge_root()?.join("host"))
}

/// Resolve the absolute path to the `glance-chrome-host` binary that should
/// be referenced from the Chrome native-messaging manifest.
///
/// Search order:
///   1. `which glance-chrome-host` (covers any custom install location on
///      PATH).
///   2. `$CARGO_HOME/bin/glance-chrome-host` if `CARGO_HOME` is set.
///   3. `~/.cargo/bin/glance-chrome-host` (the default `cargo install` dest).
///
/// Returns an error with install instructions if none of those exist.
pub fn resolve_host_binary() -> Result<PathBuf> {
    if let Ok(out) = Command::new("which").arg(HOST_BIN_NAME).output() {
        if out.status.success() {
            let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !line.is_empty() {
                let p = PathBuf::from(line);
                if p.exists() {
                    return Ok(p);
                }
            }
        }
    }
    let cargo_bin = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".cargo")))
        .map(|d| d.join("bin").join(HOST_BIN_NAME));
    if let Some(p) = cargo_bin {
        if p.exists() {
            return Ok(p);
        }
    }
    Err(anyhow!(
        "{HOST_BIN_NAME} not found on PATH or in ~/.cargo/bin. \
         Install it with:\n  \
         cargo install --git https://github.com/xtftbwvfp/glance\n\
         or, from a checkout:\n  \
         cargo install --path . --bin {HOST_BIN_NAME} --force"
    ))
}

pub fn manifest_path() -> Result<PathBuf> {
    // macOS Chrome stable path. Edge / Brave have analogous paths but we
    // don't write those automatically yet.
    Ok(home()?.join(format!(
        "Library/Application Support/Google/Chrome/NativeMessagingHosts/{}.json",
        HOST_NAME
    )))
}

fn write_with_perms(path: &Path, body: &str, mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(mode);
        fs::set_permissions(path, perms)?;
    }
    let _ = mode;
    Ok(())
}

pub fn write_native_host_manifest(extension_id: &str) -> Result<()> {
    let host_bin = resolve_host_binary()?;
    let manifest = serde_json::json!({
        "name": HOST_NAME,
        "description": "Glance Chrome bridge native host",
        "type": "stdio",
        "path": host_bin,
        "allowed_origins": [format!("chrome-extension://{}/", extension_id)]
    });
    let body = serde_json::to_string_pretty(&manifest)? + "\n";
    let mp = manifest_path()?;
    write_with_perms(&mp, &body, 0o644)
        .with_context(|| format!("write {}", mp.display()))?;
    Ok(())
}

/// Drop the extension files, native host launcher, and the Chrome
/// `NativeMessagingHosts` manifest. The manifest is bound to the
/// deterministic `FIXED_EXTENSION_ID` (see baked `key` in `manifest.json`),
/// so no separate "bind" step is required after Chrome loads the unpacked
/// extension.
pub fn install() -> Result<InstallReport> {
    let edir = ext_dir()?;
    let hdir = host_dir()?;
    // Resolve the binary up front so we fail fast with a clear message if the
    // user hasn't installed glance-chrome-host yet.
    let host_bin = resolve_host_binary()?;
    write_with_perms(&edir.join("manifest.json"), EXT_MANIFEST, 0o644)?;
    write_with_perms(&edir.join("background.js"), EXT_BACKGROUND, 0o644)?;
    write_with_perms(&edir.join("popup.html"), EXT_POPUP_HTML, 0o644)?;
    write_with_perms(&edir.join("popup.js"), EXT_POPUP_JS, 0o644)?;
    // Keep the legacy host_dir around as a stable place to point users at
    // (the chrome-host.log lives in ~/.glance/, but some docs still reference
    // this directory). Nothing executable goes here any more.
    fs::create_dir_all(&hdir).ok();
    write_native_host_manifest(FIXED_EXTENSION_ID)?;
    // Seed example adapters on first install only — leave the user's own files alone.
    let adir = crate::install::chrome_adapters::adapters_dir()?;
    fs::create_dir_all(&adir).ok();
    for (name, body) in EXAMPLE_ADAPTERS {
        let p = adir.join(name);
        if !p.exists() {
            let _ = fs::write(&p, body);
        }
    }
    Ok(InstallReport {
        ext_dir: edir,
        host_dir: hdir,
        host_binary: host_bin,
        manifest_path: manifest_path()?,
        extension_id: FIXED_EXTENSION_ID.into(),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub ext_dir: PathBuf,
    pub host_dir: PathBuf,
    /// Resolved path to the `glance-chrome-host` binary that the Chrome
    /// native-messaging manifest now points at.
    pub host_binary: PathBuf,
    pub manifest_path: PathBuf,
    pub extension_id: String,
}

/// Manually rebind the native messaging manifest to a different extension
/// id (e.g. if the user edits `manifest.json` and removes the baked key).
pub fn bind(extension_id: &str) -> Result<PathBuf> {
    let trimmed = extension_id.trim().trim_matches('/');
    if !trimmed.chars().all(|c| c.is_ascii_lowercase()) || trimmed.len() != 32 {
        return Err(anyhow!(
            "extension id must be 32 lowercase letters, got: {}",
            trimmed
        ));
    }
    // Verify the binary exists before writing the manifest so we don't bind
    // to a missing path.
    let _ = resolve_host_binary()?;
    write_native_host_manifest(trimmed)?;
    Ok(manifest_path()?)
}

/// Remove every file install() laid down. Leaves `~/.glance/` itself alone.
pub fn uninstall() -> Result<()> {
    let _ = fs::remove_dir_all(ext_dir()?);
    let _ = fs::remove_dir_all(host_dir()?);
    let _ = fs::remove_file(manifest_path()?);
    let _ = fs::remove_file(heartbeat_path()?);
    Ok(())
}

/// Snapshot of bridge installation + runtime state. Cheap to compute — used
/// by the GUI's polling loop.
#[derive(Debug, Clone, Serialize)]
pub struct ChromeStatus {
    pub ext_dir: PathBuf,
    pub ext_dir_exists: bool,
    pub host_dir: PathBuf,
    pub host_dir_exists: bool,
    pub manifest_path: PathBuf,
    pub manifest_present: bool,
    /// Extension id the manifest is currently bound to, parsed out of the
    /// `allowed_origins` URL. None if manifest missing/malformed.
    pub bound_extension_id: Option<String>,
    pub expected_extension_id: String,
    pub socket_path: PathBuf,
    pub socket_present: bool,
    /// True iff the heartbeat file was touched in the last ~15 s — meaning
    /// the Chrome extension is actively connected through our native host.
    pub bridge_connected: bool,
    /// Age of the heartbeat in seconds (None if file missing).
    pub heartbeat_age_secs: Option<u64>,
    pub heartbeat_pid: Option<u32>,
}

const HEARTBEAT_FRESH_SECS: u64 = 15;

pub fn status() -> Result<ChromeStatus> {
    let ext = ext_dir()?;
    let host = host_dir()?;
    let mp = manifest_path()?;
    let socket = crate::backend::chrome_bridge::socket_path();

    let manifest_present = mp.exists();
    let bound = if manifest_present {
        fs::read_to_string(&mp)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.get("allowed_origins")
                    .and_then(|x| x.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|x| x.as_str())
                    .and_then(|s| {
                        s.strip_prefix("chrome-extension://")
                            .and_then(|s| s.strip_suffix('/'))
                            .map(|s| s.to_string())
                    })
            })
    } else {
        None
    };

    let mut hb_age = None;
    let mut hb_pid = None;
    let hb = heartbeat_path()?;
    if let Ok(text) = fs::read_to_string(&hb) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            let ts = v.get("ts").and_then(|x| x.as_u64()).unwrap_or(0);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if ts > 0 && ts <= now {
                hb_age = Some(now - ts);
            }
            hb_pid = v.get("pid").and_then(|x| x.as_u64()).map(|n| n as u32);
        }
    }
    let bridge_connected = matches!(hb_age, Some(age) if age <= HEARTBEAT_FRESH_SECS);

    Ok(ChromeStatus {
        ext_dir: ext.clone(),
        ext_dir_exists: ext.exists(),
        host_dir: host.clone(),
        host_dir_exists: host.exists(),
        manifest_path: mp,
        manifest_present,
        bound_extension_id: bound,
        expected_extension_id: FIXED_EXTENSION_ID.into(),
        socket_path: socket.clone(),
        socket_present: socket.exists(),
        bridge_connected,
        heartbeat_age_secs: hb_age,
        heartbeat_pid: hb_pid,
    })
}
