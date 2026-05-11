//! Schema translator for opencli/autocli YAML adapters → glance's
//! `chrome-adapters/<name>.yaml` schema.
//!
//! ## Why a translator (not a port)
//!
//! Upstream opencli (TS) and autocli (Rust) ship 100+ "site recipes" under
//! Apache-2.0. Their schema is significantly richer than ours — auth
//! strategies, network interception, declarative pipelines, JSON-path
//! extraction, even cross-tab orchestration. Implementing that runtime is
//! weeks of work; we instead **translate** the subset of adapters that fit
//! into glance's existing one-shot `evaluate` model.
//!
//! ## What's in scope
//!
//! Browser-mode DOM scraping with **one** `- evaluate: |` step in the
//! pipeline. `${{ args.x }}` placeholders are inlined into a JS preamble so
//! the same scrape runs untouched. `strategy: cookie` is fine — that just
//! means the user must be logged into the site already, which is what the
//! glance Chrome bridge gives us anyway.
//!
//! ## What's skipped (with a clear reason)
//!
//! - `strategy: public` — HTTP fetches, no browser needed; tell the caller
//!   to use `web_fetch` or hit the API directly.
//! - `strategy: intercept` / `auth: INTERCEPT` — needs network capture.
//! - `auth: TOKEN` — needs token tracing across requests.
//! - Pipelines that have a `collect:` / `intercept:` / `fetch:` step.
//! - Pipelines with multiple `evaluate:` steps (cross-step state).
//! - Pipelines that try to do JSON-path extraction on a captured response.
//!
//! In all those cases we return [`TranslationError::Unsupported`] with a
//! one-liner reason. Callers (e.g. `glance chrome import`) print that to
//! stderr and skip — they don't write a half-working YAML.

use std::path::Path;

use serde_yaml::Value as Yaml;
use thiserror::Error;

use super::chrome_adapters::{Adapter, AdapterArg};

#[derive(Debug, Error)]
pub enum TranslationError {
    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("unsupported: {0}")]
    Unsupported(String),
}

/// Translate one opencli/autocli YAML document into a glance [`Adapter`].
pub fn translate_opencli_yaml(yaml_text: &str) -> Result<Adapter, TranslationError> {
    let root: Yaml = serde_yaml::from_str(yaml_text)?;
    let map = root
        .as_mapping()
        .ok_or(TranslationError::MissingField("root must be a mapping"))?;

    // ---- name = `<site>_<command>` ------------------------------------------
    let site = get_str(map, "site").ok_or(TranslationError::MissingField("site"))?;
    let command = get_str(map, "name")
        .or_else(|| get_str(map, "command"))
        .ok_or(TranslationError::MissingField("name"))?;
    let name = sanitize_name(&format!("{}_{}", site, command));

    let description = get_str(map, "description").map(|s| s.to_string());

    // ---- strategy / auth gates ----------------------------------------------
    let strategy = get_str(map, "strategy").unwrap_or("").to_ascii_lowercase();
    let auth = get_str(map, "auth").unwrap_or("").to_ascii_uppercase();
    if strategy == "public" {
        return Err(TranslationError::Unsupported(
            "strategy=public — use glance.web_fetch or hit the API directly, no browser needed"
                .into(),
        ));
    }
    if strategy == "intercept" || auth == "INTERCEPT" {
        return Err(TranslationError::Unsupported(
            "strategy=intercept needs network capture; not supported by glance.chrome".into(),
        ));
    }
    if auth == "TOKEN" {
        return Err(TranslationError::Unsupported(
            "auth=TOKEN needs token tracing across requests; not supported".into(),
        ));
    }

    // ---- pipeline -----------------------------------------------------------
    // We accept exactly one `evaluate:` step (browser DOM scrape). Any of:
    // intercept / collect / fetch / multiple-evaluate → skip.
    let pipeline = map
        .get(&Yaml::from("pipeline"))
        .and_then(|v| v.as_sequence())
        .ok_or(TranslationError::MissingField("pipeline"))?;

    // Allow optional leading `navigate:` step — we don't drive navigation
    // ourselves but we DO surface it as `match_url` and a hint in the
    // description, so the LLM knows where the page should already be.
    let mut nav_url: Option<String> = None;
    let mut evaluate_body: Option<String> = None;
    let mut limit_expr: Option<String> = None;

    for step in pipeline {
        let step_map = match step.as_mapping() {
            Some(m) => m,
            None => continue,
        };
        // Check forbidden steps first.
        for forbidden in ["intercept", "collect", "fetch"] {
            if step_map.contains_key(&Yaml::from(forbidden)) {
                return Err(TranslationError::Unsupported(format!(
                    "pipeline step `{}:` requires the opencli runtime; not supported",
                    forbidden
                )));
            }
        }
        if let Some(nav) = step_map.get(&Yaml::from("navigate")) {
            // `navigate: https://...` (string) OR `navigate: { url: ..., settleMs: ... }`
            let url = if let Some(s) = nav.as_str() {
                Some(s.to_string())
            } else if let Some(m) = nav.as_mapping() {
                m.get(&Yaml::from("url")).and_then(|v| v.as_str()).map(|s| s.to_string())
            } else {
                None
            };
            if let Some(u) = url {
                nav_url = Some(u);
            }
            continue;
        }
        if let Some(ev) = step_map.get(&Yaml::from("evaluate")) {
            if evaluate_body.is_some() {
                return Err(TranslationError::Unsupported(
                    "multiple `evaluate:` steps not supported (we run a single shot)".into(),
                ));
            }
            let body = ev.as_str().ok_or_else(|| {
                TranslationError::Unsupported("`evaluate:` value must be a JS string".into())
            })?;
            evaluate_body = Some(body.to_string());
            continue;
        }
        if let Some(lim) = step_map.get(&Yaml::from("limit")) {
            // Honor `- limit: ${{ args.limit }}` by slicing the result tail.
            limit_expr = lim.as_str().map(|s| s.to_string()).or_else(|| {
                lim.as_i64().map(|n| n.to_string())
            });
            continue;
        }
        // Other pipeline verbs (map, filter, scroll, wait) we either fold
        // into the evaluate body upstream-supplied, or refuse.
        let known_passthrough = ["map", "filter", "scroll", "wait", "delay", "click", "navigate"];
        let only_passthrough = step_map
            .iter()
            .all(|(k, _)| k.as_str().map(|s| known_passthrough.contains(&s)).unwrap_or(false));
        if !only_passthrough {
            // Unknown verb — be conservative.
            let key_names: Vec<String> = step_map
                .iter()
                .filter_map(|(k, _)| k.as_str().map(|s| s.to_string()))
                .collect();
            return Err(TranslationError::Unsupported(format!(
                "unsupported pipeline step(s): {}",
                key_names.join(", ")
            )));
        }
        // For `map:` / `filter:` we trust the upstream `evaluate:` to have
        // already shaped the rows; the post-evaluate `map` is just a column
        // rename on a Rust runtime we don't have. So we skip silently.
    }

    let raw_eval = evaluate_body.ok_or_else(|| {
        TranslationError::Unsupported(
            "no `- evaluate:` step found — only browser DOM scrapes are translatable".into(),
        )
    })?;

    // ---- args ---------------------------------------------------------------
    let args = parse_args(map.get(&Yaml::from("args")));

    // ---- match_url derivation -----------------------------------------------
    // Prefer an explicit `endpoint:` if present (opencli), otherwise derive
    // from `domain:` (autocli). Both get wrapped as a loose-anchored regex.
    let match_url = if let Some(ep) = get_str(map, "endpoint") {
        Some(escape_url_to_regex(ep))
    } else if let Some(u) = &nav_url {
        Some(escape_url_to_regex(u))
    } else if let Some(domain) = get_str(map, "domain") {
        if domain == "localhost" {
            None
        } else {
            Some(format!("^https?://{}", regex::escape(domain)))
        }
    } else {
        None
    };

    // ---- build evaluate body ------------------------------------------------
    // Inline `${{ args.x }}` placeholders. Upstream uses both `${{ args.foo }}`
    // (Liquid-ish) inside JS string literals, and `${ args.foo }` template
    // strings inside `..\`...\`..` JS. We rewrite the autocli-style ones into
    // the corresponding glance reference (`args.foo`) — the ones that already
    // sit inside backtick templates resolve naturally because `args` is in
    // scope.
    let inlined = inline_args_placeholders(&raw_eval);

    // Wrap so the final value is always a `{ rows: [...] }` shape. If the
    // adapter already returned an array, use it as `rows`; if it returned an
    // object, pass through; otherwise just stringify.
    let mut evaluate = format!(
        "(async () => {{\n  const __r = await ({inner});\n  if (Array.isArray(__r)) return {{ rows: __r }};\n  return __r;\n}})()",
        inner = inlined,
    );

    if let Some(lim_raw) = limit_expr {
        let lim_inlined = inline_args_placeholders(&lim_raw);
        evaluate = format!(
            "(async () => {{\n  const __out = await ({wrapped});\n  const __lim = ({lim});\n  if (__out && Array.isArray(__out.rows) && Number.isFinite(Number(__lim))) {{\n    __out.rows = __out.rows.slice(0, Number(__lim));\n  }}\n  return __out;\n}})()",
            wrapped = evaluate,
            lim = lim_inlined,
        );
    }

    // ---- assemble -----------------------------------------------------------
    Ok(Adapter {
        name,
        description,
        match_url,
        args,
        evaluate,
        await_promise: true,
        world: None,
        source_path: None,
    })
}

/// Convenience over [`translate_opencli_yaml`] for a file on disk.
pub fn translate_file(path: &Path) -> Result<Adapter, TranslationError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        TranslationError::Unsupported(format!("read {}: {}", path.display(), e))
    })?;
    translate_opencli_yaml(&text)
}

// ---- helpers ----------------------------------------------------------------

fn get_str<'a>(map: &'a serde_yaml::Mapping, key: &str) -> Option<&'a str> {
    map.get(&Yaml::from(key)).and_then(|v| v.as_str())
}

fn sanitize_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>()
        .to_ascii_lowercase()
}

fn parse_args(v: Option<&Yaml>) -> Vec<AdapterArg> {
    let Some(args_map) = v.and_then(|x| x.as_mapping()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (k, v) in args_map.iter() {
        let Some(name) = k.as_str() else { continue };
        let mut required = false;
        let mut description: Option<String> = None;
        let mut default: Option<serde_json::Value> = None;
        if let Some(spec) = v.as_mapping() {
            required = spec
                .get(&Yaml::from("required"))
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            // autocli: `positional: true` means the user MUST pass it.
            if spec
                .get(&Yaml::from("positional"))
                .and_then(|x| x.as_bool())
                .unwrap_or(false)
            {
                required = true;
            }
            description = spec.get(&Yaml::from("description"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            if let Some(d) = spec.get(&Yaml::from("default")) {
                if let Ok(jv) = serde_json::to_value(d) {
                    default = Some(jv);
                }
            }
        }
        out.push(AdapterArg { name: name.to_string(), description, required, default });
    }
    out
}

/// Replace `${{ args.foo }}` / `${{ args.foo | default(N) }}` patterns with a
/// JS expression that reads from the in-scope `args` object. We're
/// deliberately permissive — anything we don't recognize is left as-is so the
/// resulting JS at least parses.
fn inline_args_placeholders(src: &str) -> String {
    // {{ ... }} (with surrounding $) → ( ... ) where ... is the inner expr,
    // with `args.x` left alone (it's already in scope) and Liquid filters
    // dropped.
    let re = regex::Regex::new(r"\$\{\{\s*([^}]+?)\s*\}\}").expect("static regex");
    re.replace_all(src, |caps: &regex::Captures| {
        let inner = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        // Drop a trailing `| filter(...)` Liquid filter chain; we can't run
        // those in plain JS.
        let head = inner.split('|').next().unwrap_or(inner).trim();
        // Wrap so the substitution remains a single expression even when
        // surrounded by string concatenation.
        format!("({})", head)
    })
    .into_owned()
}

/// Turn a literal URL into a loose-anchored regex (escaped, anchored at the
/// start, allowing query strings / paths to extend past).
fn escape_url_to_regex(url: &str) -> String {
    // Strip query string before escaping — a recipe page rarely needs to
    // match the exact query.
    let trimmed = url.split('?').next().unwrap_or(url).trim_end_matches('/');
    // Allow http(s) interchangeably.
    let stripped = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    format!("^https?://{}", regex::escape(stripped))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = r#"
site: hackernews
name: top
description: Hacker News top stories (DOM)
domain: news.ycombinator.com
strategy: cookie
browser: true

args:
  limit:
    type: int
    default: 20
    description: Number of stories

pipeline:
  - navigate: https://news.ycombinator.com/news
  - evaluate: |
      (() => {
        return Array.from(document.querySelectorAll('.athing'))
          .slice(0, ${{ args.limit }})
          .map((el, i) => ({ rank: i + 1, title: el.querySelector('.titleline a')?.textContent }));
      })()
"#;

    #[test]
    fn translates_simple_dom_scrape() {
        let a = translate_opencli_yaml(SIMPLE).expect("translate");
        assert_eq!(a.name, "hackernews_top");
        assert_eq!(a.match_url.as_deref(), Some("^https?://news\\.ycombinator\\.com/news"));
        assert!(a.evaluate.contains("document.querySelectorAll"));
        assert!(a.evaluate.contains("(args.limit)"));
        assert!(a.args.iter().any(|x| x.name == "limit"));
    }

    #[test]
    fn skips_public_strategy() {
        let yaml = r#"
site: x
name: y
strategy: public
pipeline:
  - fetch: { url: https://example.com }
"#;
        let err = translate_opencli_yaml(yaml).unwrap_err();
        assert!(matches!(err, TranslationError::Unsupported(_)));
        assert!(format!("{}", err).contains("public"));
    }

    #[test]
    fn skips_intercept_strategy() {
        let yaml = r#"
site: x
name: y
strategy: intercept
pipeline:
  - intercept: { pattern: foo }
"#;
        let err = translate_opencli_yaml(yaml).unwrap_err();
        assert!(format!("{}", err).contains("intercept"));
    }

    #[test]
    fn skips_pipeline_with_collect() {
        let yaml = r#"
site: x
name: y
strategy: cookie
pipeline:
  - evaluate: "(() => 1)()"
  - collect: { parse: "(r) => r" }
"#;
        let err = translate_opencli_yaml(yaml).unwrap_err();
        assert!(format!("{}", err).contains("collect"));
    }

    #[test]
    fn skips_multiple_evaluate() {
        let yaml = r#"
site: x
name: y
strategy: cookie
pipeline:
  - evaluate: "(() => 1)()"
  - evaluate: "(() => 2)()"
"#;
        let err = translate_opencli_yaml(yaml).unwrap_err();
        assert!(format!("{}", err).contains("multiple"));
    }

    #[test]
    fn applies_limit_step() {
        let yaml = r#"
site: x
name: y
strategy: cookie
args:
  limit:
    type: int
    default: 5
pipeline:
  - evaluate: |
      (() => [1,2,3,4,5,6,7,8,9,10])()
  - limit: ${{ args.limit }}
"#;
        let a = translate_opencli_yaml(yaml).expect("translate");
        assert!(a.evaluate.contains("__out.rows.slice"));
    }
}
