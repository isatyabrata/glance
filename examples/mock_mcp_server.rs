//! Tiny stdio MCP server used by the aggregator integration test.
//!
//! Reads one JSON-RPC request per line from stdin, writes one response per
//! line to stdout. Implements just enough surface for an end-to-end smoke:
//!
//! - `initialize`         → {protocolVersion, serverInfo}
//! - `tools/list`         → one tool: `echo` with `{message: string}` schema
//! - `tools/call` "echo"  → text content "echo: <message>"
//! - notifications        → ignored (no response)
//!
//! Stderr is unused. Exits when stdin closes.
//!
//! Self-heal hook: if `MOCK_DIE_AFTER` is set to a positive integer N, the
//! server exits 0 right after responding to its Nth `tools/call` (handshake
//! and `tools/list` don't count). Used by the aggregator's respawn tests.
//! `MOCK_FAIL_INIT=1` makes every `initialize` return an error response, so
//! tests can starve the respawn budget without flaky races.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let die_after: Option<u64> = std::env::var("MOCK_DIE_AFTER")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &u64| *n > 0);
    let fail_init_explicit = std::env::var("MOCK_FAIL_INIT").ok().as_deref() == Some("1");
    // If MOCK_FAIL_INIT_IF_FILE points at a path, every spawn that finds the
    // file existing answers `initialize` with an error. The first spawn
    // creates the file just before exiting (when paired with MOCK_DIE_AFTER),
    // so the *second and later* spawns of the same mock starve the
    // aggregator's respawn budget deterministically.
    let fail_init_marker = std::env::var("MOCK_FAIL_INIT_IF_FILE").ok();
    let fail_init_now = fail_init_explicit
        || fail_init_marker
            .as_ref()
            .map(|p| std::path::Path::new(p).exists())
            .unwrap_or(false);
    let fail_init = fail_init_now;
    let mut tool_calls: u64 = 0;

    let mut line = String::new();
    loop {
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(n) => n,
            Err(_) => return,
        };
        if n == 0 {
            return;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = req.get("id").cloned();
        let method = req
            .get("method")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        // Notifications: no response.
        if id.is_none() {
            continue;
        }
        let id = id.unwrap();

        // When fail_init is set, slam stdin closed on the very first request
        // so the aggregator's handshake fails with a write error / channel
        // closed. (Returning an `error` JSON-RPC response would be accepted
        // as a successful handshake by the current client code.)
        if fail_init && method == "initialize" {
            std::process::exit(2);
        }

        let resp = match method.as_str() {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "serverInfo": { "name": "mock_mcp_server", "version": "0.1.0" },
                    "capabilities": { "tools": { "listChanged": false } }
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": "echo",
                        "description": "Echo back the message arg.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "message": { "type": "string" }
                            },
                            "required": ["message"]
                        }
                    }]
                }
            }),
            "tools/call" => {
                tool_calls += 1;
                let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
                let tool_name = params.get("name").and_then(|x| x.as_str()).unwrap_or("");
                if tool_name == "echo" {
                    let msg = params
                        .get("arguments")
                        .and_then(|a| a.get("message"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": format!("echo: {}", msg) }]
                        }
                    })
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("unknown tool {}", tool_name) }
                    })
                }
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("unknown method {}", method) }
            }),
        };

        let s = serde_json::to_string(&resp).unwrap();
        let _ = writeln!(out, "{}", s);
        let _ = out.flush();

        // After flushing the Nth tools/call response, self-terminate so the
        // aggregator's liveness check on the next call observes a dead child.
        if let Some(n) = die_after {
            if method == "tools/call" && tool_calls >= n {
                // Drop a marker file before exiting so that, if the test is
                // configured for the "give up" scenario, subsequent respawns
                // are forced to fail at handshake time.
                if let Some(p) = &fail_init_marker {
                    let _ = std::fs::write(p, b"1");
                }
                std::process::exit(0);
            }
        }
    }
}
