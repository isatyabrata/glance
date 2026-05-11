//! Safety guards: high-risk path / keyword detection. Lifted from CodexSaver's
//! router but stripped down — glance's read tools don't need the same level of
//! gating since they have no side effects. Write tools (Phase 5) will use it.

use crate::config::SafetyConfig;

pub fn is_high_risk_path(path: &str, cfg: &SafetyConfig) -> bool {
    let lower = path.to_lowercase();
    cfg.deny_paths.iter().any(|kw| lower.contains(kw))
}

pub fn is_high_risk_instruction(text: &str, cfg: &SafetyConfig) -> bool {
    let lower = text.to_lowercase();
    cfg.deny_keywords.iter().any(|kw| lower.contains(kw))
}
