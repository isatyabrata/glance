//! `glance-mcp` — stdio MCP server entry point.
//!
//! Spawned by codex / claude / cursor. Reads JSON-RPC requests from stdin,
//! writes responses to stdout. All logging goes to stderr.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // 日志只能到 stderr —— stdout 留给 MCP JSON-RPC
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!(
        "glance-mcp v{} starting in stdio mode",
        env!("CARGO_PKG_VERSION")
    );

    // Connect every enabled upstream MCP before we start the JSON-RPC loop.
    // Failures in any one upstream are logged but don't abort startup —
    // glance's own 17 tools always work.
    if let Err(e) = glance::mcp_aggregator::rebuild_from_config().await {
        tracing::warn!("aggregator init failed: {}", e);
    } else if let Some(agg) = glance::mcp_aggregator::current().await {
        let snap = agg.status_snapshot().await;
        let connected = snap
            .iter()
            .filter(|s| matches!(s.status, glance::mcp_aggregator::UpstreamStatus::Connected))
            .count();
        tracing::info!(
            "aggregator: {} upstream(s) connected, {} total configured",
            connected,
            snap.len()
        );
    }

    glance::mcp::transport::run_stdio().await?;
    Ok(())
}
