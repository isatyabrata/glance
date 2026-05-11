//! `glance` — UX wrapper around the `glance-mcp` server.
//!
//! Subcommands:
//! - `glance install --client {codex|claude|cursor|all} [--remove] [--no-agents-md]`
//! - `glance doctor`
//! - `glance stats [--days N] [--json]`
//! - `glance chrome install` / `glance chrome bind <ext-id>` / `glance chrome status`
//! - `glance chrome import <path-or-dir>` (translate opencli/autocli YAMLs)
//!
//! `glance-mcp` is the actual stdio MCP server; the entries written by
//! `install` always reference `glance-mcp`, never `glance` itself.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "glance",
    version,
    about = "Install / inspect the glance MCP server in codex / claude / cursor."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Register glance-mcp in a client's MCP server config.
    Install {
        /// Which client to target.
        #[arg(long, value_name = "CLIENT", default_value = "all")]
        client: String,
        /// Remove the glance entry instead of adding it.
        #[arg(long, default_value_t = false)]
        remove: bool,
        /// Skip appending the glance hint into AGENTS.md / CLAUDE.md.
        #[arg(long, default_value_t = false)]
        no_agents_md: bool,
    },
    /// Diagnose config / backend / client registration health.
    Doctor,
    /// Per-tool aggregate stats from `~/.glance/events.jsonl`.
    Stats {
        /// Look back this many days. Default 7.
        #[arg(long, default_value_t = 7)]
        days: i64,
        /// Emit JSON instead of a text table.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Manage the Chrome bridge extension + native host.
    Chrome {
        #[command(subcommand)]
        action: ChromeCmd,
    },
}

#[derive(Debug, Subcommand)]
enum ChromeCmd {
    /// Copy the extension + native host into ~/.glance/chrome-bridge/
    /// and print the steps to load the unpacked extension in Chrome.
    Install,
    /// After loading the extension, run this with the extension ID Chrome
    /// shows on chrome://extensions to write a binding native-host manifest.
    Bind {
        /// Chrome extension ID (the 32-char id shown on chrome://extensions).
        extension_id: String,
    },
    /// Show current bridge file paths, manifest binding, and socket path.
    Status,
    /// Translate one or more opencli/autocli YAML adapters into glance's
    /// schema and write the result into ~/.glance/chrome-adapters/.
    ///
    /// Browser DOM-scrape adapters with one `evaluate:` step translate
    /// cleanly. `strategy: public` (HTTP-only), `intercept`, `auth: TOKEN`,
    /// and pipelines that need the upstream runtime (`fetch:` / `collect:`)
    /// are skipped with a one-line reason.
    Import {
        /// Path to a single YAML file, or a directory to scan recursively.
        path: std::path::PathBuf,
        /// Print what would be imported without writing any files.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Overwrite existing adapters of the same name.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Install {
            client,
            remove,
            no_agents_md,
        } => glance::install::install_with_opts(&client, remove, !no_agents_md),
        Cmd::Doctor => doctor::run(),
        Cmd::Stats { days, json } => stats::run(days, json),
        Cmd::Chrome { action } => match action {
            ChromeCmd::Install => chrome_setup::install(),
            ChromeCmd::Bind { extension_id } => chrome_setup::bind(&extension_id),
            ChromeCmd::Status => chrome_setup::status(),
            ChromeCmd::Import { path, dry_run, force } => {
                chrome_setup::import(&path, dry_run, force)
            }
        },
    }
}

mod doctor {
    use std::time::{Duration, Instant};

    use anyhow::Result;
    use serde_json::json;

    use glance::config::{self, Config};
    use glance::install::{
        claude as claude_install, codex as codex_install, cursor as cursor_install,
    };

    pub fn run() -> Result<()> {
        println!("glance doctor — checking environment\n");

        // 1. Config load
        let cfg = match config::load_or_default() {
            Ok(c) => {
                println!("\u{2713} config loaded");
                c
            }
            Err(e) => {
                println!("\u{2717} config load failed: {:#}", e);
                Config::default()
            }
        };

        // 2. Backend api_key non-empty
        let key_present = !cfg.backend.api_key.trim().is_empty();
        if key_present {
            let masked = mask(&cfg.backend.api_key);
            println!("\u{2713} backend.api_key set ({})", masked);
        } else {
            println!(
                "\u{2717} backend.api_key empty — set GLANCE_API_KEY or add to {}",
                config::user_config_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "~/.glance/config.toml".to_string())
            );
        }

        // 3. Reachability — fire and forget on a small tokio runtime so we don't
        //    drag #[tokio::main] into this binary's hot path.
        if key_present {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio rt");
            let cfg_clone = cfg.clone();
            let res = rt.block_on(async move { ping_backend(&cfg_clone).await });
            match res {
                Ok(latency_ms) => println!(
                    "\u{2713} backend reachable (model={}, latency={}ms)",
                    cfg.backend.model, latency_ms
                ),
                Err(e) => println!("\u{2717} backend ping failed: {:#}", e),
            }
        } else {
            println!("— skipping backend ping (no api_key)");
        }

        println!();
        println!("clients:");
        check_client(
            "codex CLI",
            codex_install::config_path().ok(),
            codex_install::is_registered().unwrap_or(false),
            "glance install --client codex",
        );
        check_client(
            "Claude Code",
            claude_install::config_path().ok(),
            claude_install::is_registered().unwrap_or(false),
            "glance install --client claude",
        );
        check_client(
            "Cursor",
            cursor_install::config_path().ok(),
            cursor_install::is_registered().unwrap_or(false),
            "glance install --client cursor",
        );

        println!();
        println!("obsidian:");
        let vault_setting = cfg.obsidian.vault.trim().to_string();
        if vault_setting.is_empty() {
            println!("— vault not set in [obsidian].vault (will fall back at runtime)");
        } else {
            let p = std::path::PathBuf::from(&vault_setting);
            if p.is_dir() {
                println!("\u{2713} vault {}", p.display());
            } else {
                println!("\u{2717} vault path missing: {}", p.display());
            }
        }

        Ok(())
    }

    async fn ping_backend(cfg: &Config) -> Result<u128> {
        let url = format!(
            "{}/chat/completions",
            cfg.backend.base_url.trim_end_matches('/')
        );
        let body = json!({
            "model": cfg.backend.model,
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 1,
        });
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.backend.timeout_secs.into()))
            .build()?;
        let start = Instant::now();
        let resp = http
            .post(&url)
            .bearer_auth(&cfg.backend.api_key)
            .json(&body)
            .send()
            .await?;
        let elapsed = start.elapsed().as_millis();
        let status = resp.status();
        if !status.is_success() {
            let snippet: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            anyhow::bail!("HTTP {} from {}: {}", status, url, snippet);
        }
        Ok(elapsed)
    }

    fn check_client(label: &str, path: Option<std::path::PathBuf>, registered: bool, fix: &str) {
        let path_str = path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "?".to_string());
        let exists = path.as_ref().map(|p| p.exists()).unwrap_or(false);
        let status = if !exists {
            "— file not found".to_string()
        } else if registered {
            "\u{2713} registered".to_string()
        } else {
            format!("\u{2717} not registered  → run `{}`", fix)
        };
        println!("  {:<12} {:<40} {}", label, path_str, status);
    }

    fn mask(key: &str) -> String {
        let trimmed = key.trim();
        if trimmed.len() <= 8 {
            return "***".to_string();
        }
        let head: String = trimmed.chars().take(4).collect();
        let tail: String = trimmed
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("{}…{}", head, tail)
    }
}

mod stats {
    //! `glance stats` — read `~/.glance/events.jsonl`, aggregate per tool,
    //! print a savings table.
    //!
    //! Algorithm (re-implemented, not lifted): scan the JSONL, drop events
    //! older than the cutoff, fold into per-tool buckets, emit either a
    //! pretty-printed table or one JSON blob.

    use std::collections::BTreeMap;

    use anyhow::Result;
    use chrono::{DateTime, Duration, Utc};
    use serde::Serialize;

    use glance::events::{self, ToolEvent};

    #[derive(Default, Debug, Serialize)]
    struct Bucket {
        tool: String,
        calls: u64,
        tokens: u64,
        savings_tokens: u64,
        wall_ms: u64,
        bytes_in: u64,
        bytes_out: u64,
        errors: u64,
    }

    #[derive(Debug, Serialize)]
    struct StatsReport {
        days: i64,
        total_calls: u64,
        total_tokens: u64,
        total_savings_tokens: u64,
        total_wall_ms: u64,
        per_tool: Vec<Bucket>,
    }

    pub fn run(days: i64, as_json: bool) -> Result<()> {
        let path = events::events_path()?;
        if !path.exists() {
            eprintln!(
                "no events file at {} (set events_enabled = true in ~/.glance/config.toml)",
                path.display()
            );
            return Ok(());
        }
        let raw = std::fs::read_to_string(&path)?;
        let cutoff = Utc::now() - Duration::days(days.max(0));

        let mut buckets: BTreeMap<String, Bucket> = BTreeMap::new();
        let mut total_calls = 0u64;
        let mut total_tokens = 0u64;
        let mut total_savings = 0u64;
        let mut total_wall = 0u64;

        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let ev: ToolEvent = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue, // tolerate older / partial records
            };
            // Filter by date.
            let Ok(ts) = DateTime::parse_from_rfc3339(&ev.ts) else {
                continue;
            };
            if ts.with_timezone(&Utc) < cutoff {
                continue;
            }
            let b = buckets.entry(ev.tool.clone()).or_default();
            b.tool = ev.tool.clone();
            b.calls += 1;
            b.tokens += ev.tokens as u64;
            b.savings_tokens += ev.estimated_caller_savings_tokens;
            b.wall_ms += ev.duration_ms;
            b.bytes_in += ev.bytes_in;
            b.bytes_out += ev.bytes_out;
            if !ev.ok {
                b.errors += 1;
            }
            total_calls += 1;
            total_tokens += ev.tokens as u64;
            total_savings += ev.estimated_caller_savings_tokens;
            total_wall += ev.duration_ms;
        }

        let mut per_tool: Vec<Bucket> = buckets.into_values().collect();
        per_tool.sort_by_key(|b| std::cmp::Reverse(b.calls));

        if as_json {
            let report = StatsReport {
                days,
                total_calls,
                total_tokens,
                total_savings_tokens: total_savings,
                total_wall_ms: total_wall,
                per_tool,
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
            return Ok(());
        }

        println!("glance stats — last {} day(s) — {}", days, path.display());
        println!();
        println!(
            "{:<22} {:>8} {:>10} {:>14} {:>10} {:>8}",
            "tool", "calls", "tokens", "saved (tok)", "wall (s)", "errors"
        );
        println!("{}", "-".repeat(78));
        if per_tool.is_empty() {
            println!("(no events in window)");
        } else {
            for b in &per_tool {
                println!(
                    "{:<22} {:>8} {:>10} {:>14} {:>10.1} {:>8}",
                    truncate(&b.tool, 22),
                    b.calls,
                    b.tokens,
                    b.savings_tokens,
                    b.wall_ms as f64 / 1000.0,
                    b.errors,
                );
            }
        }
        println!("{}", "-".repeat(78));
        println!(
            "{:<22} {:>8} {:>10} {:>14} {:>10.1}",
            "TOTAL",
            total_calls,
            total_tokens,
            total_savings,
            total_wall as f64 / 1000.0,
        );
        println!();
        println!(
            "this week glance saved you ~{} tokens of caller work.",
            total_savings
        );
        Ok(())
    }

    fn truncate(s: &str, n: usize) -> String {
        if s.chars().count() <= n {
            s.to_string()
        } else {
            let head: String = s.chars().take(n.saturating_sub(1)).collect();
            format!("{}…", head)
        }
    }
}

mod chrome_setup {
    //! Thin CLI wrapper around `glance::install::chrome` so the same logic
    //! also works from the Tauri GUI.

    use anyhow::Result;

    use glance::install::chrome as bridge;

    pub fn install() -> Result<()> {
        let r = bridge::install()?;
        println!("\u{2713} extension files at  {}", r.ext_dir.display());
        println!("\u{2713} native host binary  {}", r.host_binary.display());
        println!("\u{2713} host artifacts dir  {}", r.host_dir.display());
        println!(
            "\u{2713} bound extension id  {} (from baked manifest key)",
            r.extension_id
        );
        println!();
        println!("Last manual step (Chrome won't let us load unpacked extensions for you):");
        println!("  1. Open chrome://extensions");
        println!("  2. Enable Developer mode (top-right)");
        println!("  3. Click \"Load unpacked\" → pick:");
        println!("       {}", r.ext_dir.display());
        println!();
        println!("Then turn on `chrome` in the Glance app's Tools tab (or set");
        println!("`tools.chrome = true` in ~/.glance/config.toml) and restart your");
        println!("MCP client (Claude Code / Codex / Cursor) so it re-reads tools/list.");
        let _ = std::process::Command::new("open")
            .arg("-a")
            .arg("Google Chrome")
            .arg("chrome://extensions")
            .spawn();
        Ok(())
    }

    pub fn bind(extension_id: &str) -> Result<()> {
        let mp = bridge::bind(extension_id)?;
        println!("\u{2713} bound extension {} to native host", extension_id);
        println!("  manifest: {}", mp.display());
        println!();
        println!("Reload the extension in chrome://extensions for the change to apply.");
        Ok(())
    }

    pub fn status() -> Result<()> {
        let s = bridge::status()?;
        let tick = |b: bool| if b { "\u{2713}" } else { "\u{2717}" };
        println!("Glance Chrome bridge");
        println!("  extension dir  : {} {}", s.ext_dir.display(), tick(s.ext_dir_exists));
        println!("  host dir       : {} {}", s.host_dir.display(), tick(s.host_dir_exists));
        println!("  host manifest  : {} {}", s.manifest_path.display(), tick(s.manifest_present));
        if let Some(id) = &s.bound_extension_id {
            let match_marker = if id == &s.expected_extension_id { "(matches baked id)" } else { "(custom id)" };
            println!("    bound to     : chrome-extension://{}/  {}", id, match_marker);
        }
        println!("  unix socket    : {} {}", s.socket_path.display(), tick(s.socket_present));
        match s.heartbeat_age_secs {
            Some(age) if s.bridge_connected => println!(
                "  bridge live    : \u{2713} ({}s ago, host pid={})",
                age,
                s.heartbeat_pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
            ),
            Some(age) => println!("  bridge live    : \u{2717} stale heartbeat ({}s ago)", age),
            None => println!("  bridge live    : \u{2717} no heartbeat (extension not loaded?)"),
        }
        Ok(())
    }

    /// Translate opencli/autocli YAMLs in `path` (file or directory) and
    /// write the results to `~/.glance/chrome-adapters/`. Returns Ok even if
    /// some adapters are skipped — only fails if the path itself can't be
    /// read or the destination directory can't be created.
    pub fn import(path: &std::path::Path, dry_run: bool, force: bool) -> Result<()> {
        use anyhow::anyhow;
        use glance::install::chrome_adapter_import::{translate_opencli_yaml, TranslationError};
        use glance::install::chrome_adapters::{adapters_dir, save};

        if !path.exists() {
            return Err(anyhow!("path not found: {}", path.display()));
        }

        let mut yaml_files: Vec<std::path::PathBuf> = Vec::new();
        if path.is_file() {
            yaml_files.push(path.to_path_buf());
        } else if path.is_dir() {
            for entry in walkdir::WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
                let p = entry.path();
                if p.is_file()
                    && p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e == "yaml" || e == "yml")
                        .unwrap_or(false)
                {
                    yaml_files.push(p.to_path_buf());
                }
            }
            yaml_files.sort();
        }

        let total = yaml_files.len();
        if total == 0 {
            println!("(no YAML files found at {})", path.display());
            return Ok(());
        }

        let dest_dir = adapters_dir()?;
        let mut existing: std::collections::HashSet<String> = std::collections::HashSet::new();
        if dest_dir.exists() {
            for entry in std::fs::read_dir(&dest_dir)?.flatten() {
                if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                    existing.insert(stem.to_string());
                }
            }
        }

        if !dry_run {
            std::fs::create_dir_all(&dest_dir)?;
        }

        let mut imported: u32 = 0;
        let mut skipped: u32 = 0;
        for yaml_path in &yaml_files {
            let body = match std::fs::read_to_string(yaml_path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[skip] {}: read failed: {}", yaml_path.display(), e);
                    skipped += 1;
                    continue;
                }
            };
            match translate_opencli_yaml(&body) {
                Ok(adapter) => {
                    if existing.contains(&adapter.name) && !force {
                        eprintln!(
                            "[skip] {}: adapter `{}` already exists (pass --force to overwrite)",
                            yaml_path.display(),
                            adapter.name
                        );
                        skipped += 1;
                        continue;
                    }
                    if dry_run {
                        println!(
                            "[dry-run] would import `{}` ← {}",
                            adapter.name,
                            yaml_path.display()
                        );
                        imported += 1;
                        continue;
                    }
                    match save(&adapter) {
                        Ok(written) => {
                            println!(
                                "[ok] {} → {}",
                                yaml_path.display(),
                                written.display()
                            );
                            imported += 1;
                        }
                        Err(e) => {
                            eprintln!("[skip] {}: save failed: {}", yaml_path.display(), e);
                            skipped += 1;
                        }
                    }
                }
                Err(TranslationError::Unsupported(reason)) => {
                    eprintln!("[skip] {}: {}", yaml_path.display(), reason);
                    skipped += 1;
                }
                Err(e) => {
                    eprintln!("[skip] {}: {}", yaml_path.display(), e);
                    skipped += 1;
                }
            }
        }

        println!();
        println!("imported {} / skipped {} / total {}", imported, skipped, total);
        if !dry_run && imported > 0 {
            println!("written to {}", dest_dir.display());
            println!("verify with: glance-mcp ... → tool `chrome` action `list_adapters`");
        }
        Ok(())
    }
}

