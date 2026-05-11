//! End-to-end smoke for the stdio MCP client + aggregator.
//!
//! Locates the `mock_mcp_server` example binary built by cargo, points an
//! `UpstreamMcp::Stdio` spec at it, asks the aggregator to:
//!
//! 1. Spawn the subprocess and complete the `initialize` handshake.
//! 2. List its tools (we expect a single namespaced tool `mock__echo`).
//! 3. Call that tool through `Aggregator::call_tool` and verify the reply.
//!
//! The aggregator drops its handle at the end of the test, which kills the
//! subprocess (kill_on_drop=true). Test passes if the response text matches.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use glance::config::UpstreamMcp;
use glance::mcp::protocol::ToolContentBlock;
use glance::mcp_aggregator::Aggregator;

/// Build the mock binary on demand and return its path. We need this because
/// `cargo test` doesn't auto-build examples.
fn build_mock_binary() -> PathBuf {
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("examples");
    let exe = target_dir.join("mock_mcp_server");
    if !exe.exists() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "--example", "mock_mcp_server"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("cargo build mock_mcp_server");
        assert!(status.success(), "failed to build mock_mcp_server example");
    }
    exe
}

#[tokio::test]
async fn aggregator_stdio_end_to_end() {
    let exe = build_mock_binary();
    assert!(exe.exists(), "mock binary missing at {}", exe.display());

    let spec = UpstreamMcp::Stdio {
        name: "mock".into(),
        command: exe.to_string_lossy().into_owned(),
        args: Vec::new(),
        env: HashMap::new(),
        enabled: true,
        clients: Vec::new(),
        prelude_call: None,
    };

    let agg = Aggregator::start(&[spec], "").await;
    let tools = agg.list_tools().await;
    assert_eq!(
        tools.len(),
        1,
        "expected exactly one namespaced tool, got: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    assert_eq!(tools[0].name, "mock__echo");

    let result = agg
        .call_tool(
            "mock__echo",
            serde_json::json!({"message": "hello aggregator"}),
        )
        .await
        .expect("call_tool")
        .expect("aggregator should claim this tool");
    let text = result
        .content
        .iter()
        .map(|b| match b {
            ToolContentBlock::Text { text } => text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(text, "echo: hello aggregator");
    assert!(!result.is_error.unwrap_or(false));

    // Sanity: an unknown tool not under our namespace should return None.
    let none_result = agg
        .call_tool("research", serde_json::json!({}))
        .await
        .expect("call_tool");
    assert!(
        none_result.is_none(),
        "non-namespaced tool should fall through, got Some"
    );
}

/// `MOCK_DIE_AFTER=1` makes the mock exit right after replying to its first
/// `tools/call`. The aggregator should then notice on the next call, respawn
/// the subprocess transparently, and return a successful result. Drives the
/// happy path of the self-heal loop.
#[tokio::test]
async fn aggregator_stdio_respawns_after_child_death() {
    let exe = build_mock_binary();
    let mut env = HashMap::new();
    env.insert("MOCK_DIE_AFTER".to_string(), "1".to_string());

    let spec = UpstreamMcp::Stdio {
        name: "mock".into(),
        command: exe.to_string_lossy().into_owned(),
        args: Vec::new(),
        env,
        enabled: true,
        clients: Vec::new(),
        prelude_call: None,
    };

    let agg = Aggregator::start(&[spec], "").await;

    // First call goes to the original child; child exits right after sending
    // the response.
    let r1 = agg
        .call_tool("mock__echo", serde_json::json!({"message": "first"}))
        .await
        .expect("first call_tool")
        .expect("aggregator should claim mock__echo");
    let t1: String = r1
        .content
        .iter()
        .map(|b| match b {
            ToolContentBlock::Text { text } => text.clone(),
        })
        .collect();
    assert_eq!(t1, "echo: first");

    // Give the child a moment to actually exit so try_wait observes it.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // Second call should transparently respawn the subprocess and succeed.
    let r2 = agg
        .call_tool("mock__echo", serde_json::json!({"message": "second"}))
        .await
        .expect("second call_tool after respawn")
        .expect("aggregator should claim mock__echo");
    assert!(
        !r2.is_error.unwrap_or(false),
        "respawn path returned error: {:?}",
        r2.content
    );
    let t2: String = r2
        .content
        .iter()
        .map(|b| match b {
            ToolContentBlock::Text { text } => text.clone(),
        })
        .collect();
    assert_eq!(
        t2, "echo: second",
        "respawned upstream did not produce the expected echo"
    );
}

/// Drives the "respawn budget exhausted" path. The mock dies after every
/// `tools/call`, *and* a marker file makes every subsequent `initialize`
/// answer with an error. So:
///
/// 1. First call succeeds (initial healthy child).
/// 2. Child exits + drops marker.
/// 3. Calls 2, 3, 4 each trigger a respawn attempt that fails at handshake;
///    after 3 consecutive failures the aggregator promotes the upstream to
///    `Failed` and short-circuits further calls until the cooldown elapses.
#[tokio::test]
async fn aggregator_stdio_gives_up_after_three_failures() {
    let exe = build_mock_binary();
    let marker = std::env::temp_dir().join(format!(
        "glance-mock-fail-init-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&marker);

    let mut env = HashMap::new();
    env.insert("MOCK_DIE_AFTER".to_string(), "1".to_string());
    env.insert(
        "MOCK_FAIL_INIT_IF_FILE".to_string(),
        marker.to_string_lossy().into_owned(),
    );

    let spec = UpstreamMcp::Stdio {
        name: "flaky".into(),
        command: exe.to_string_lossy().into_owned(),
        args: Vec::new(),
        env,
        enabled: true,
        clients: Vec::new(),
        prelude_call: None,
    };
    let agg = Aggregator::start(&[spec], "").await;

    // Call 1: healthy. Child writes the response, drops the marker, exits.
    let r1 = agg
        .call_tool("flaky__echo", serde_json::json!({"message": "ok"}))
        .await
        .expect("call 1")
        .expect("aggregator should claim flaky__echo");
    assert!(
        !r1.is_error.unwrap_or(false),
        "first call should succeed, got {:?}",
        r1.content
    );

    // Wait for the mock to actually exit + drop the marker. Poll briefly
    // instead of guessing a sleep duration, to keep the test deterministic.
    for _ in 0..50 {
        if marker.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        marker.exists(),
        "mock should have written its respawn-poison marker at {}",
        marker.display()
    );

    // Calls 2..=4: respawn keeps failing, bounded by the 3-strike budget.
    let mut last_error_message = String::new();
    for i in 2..=4 {
        let r = agg
            .call_tool(
                "flaky__echo",
                serde_json::json!({"message": format!("retry-{}", i)}),
            )
            .await
            .expect("call_tool")
            .expect("aggregator should claim flaky__echo");
        assert!(
            r.is_error.unwrap_or(false),
            "call #{} expected error (respawn budget burning), got {:?}",
            i,
            r.content
        );
        if let Some(glance::mcp::protocol::ToolContentBlock::Text { text }) = r.content.first() {
            last_error_message = text.clone();
        }
    }

    // After the budget is spent, the upstream is sticky-failed and the
    // error message switches to the "in failed state" branch.
    let r5 = agg
        .call_tool("flaky__echo", serde_json::json!({"message": "after"}))
        .await
        .expect("call_tool")
        .expect("aggregator should claim flaky__echo");
    assert!(r5.is_error.unwrap_or(false));
    let txt5 = match r5.content.first() {
        Some(glance::mcp::protocol::ToolContentBlock::Text { text }) => text.clone(),
        _ => String::new(),
    };
    assert!(
        txt5.contains("in failed state") || txt5.contains("respawn"),
        "post-budget call should report failed state, got: {} (prev: {})",
        txt5,
        last_error_message
    );

    let _ = std::fs::remove_file(&marker);
}
