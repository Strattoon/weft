//! Local-only CLI-subprocess provider for the planner backend.
//!
//! Gated behind `providers`. CLI backends resolve ONLY through the fixed
//! `registry()` below — never from request-supplied command strings — and the
//! prompt is always delivered via the child's stdin (no shell, no argv interp).

use anyhow::{anyhow, Result};

/// A known local CLI backend. Fixed at compile time; never built from input.
#[derive(Debug, Clone, Copy)]
pub struct CliBackend {
    pub name: &'static str,
    pub program: &'static str,
    pub args: &'static [&'static str],
    pub timeout_secs: u64,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    pub enabled_by_default: bool,
}

/// Usability of a CLI backend at resolution time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliBackendStatus {
    EnabledByDefault,
    ProbeRequired,
    ProbePassed,
    ProbeFailed,
}

const REGISTRY: &[CliBackend] = &[
    CliBackend {
        name: "claude-p",
        program: "claude",
        args: &["-p", "--output-format", "text"],
        timeout_secs: 120,
        max_stdout_bytes: 262_144,
        max_stderr_bytes: 65_536,
        enabled_by_default: true,
    },
    CliBackend {
        name: "codex",
        program: "codex",
        args: &["exec"],
        timeout_secs: 180,
        max_stdout_bytes: 262_144,
        max_stderr_bytes: 65_536,
        enabled_by_default: false,
    },
    CliBackend {
        name: "kimi",
        program: "kimi",
        args: &[],
        timeout_secs: 180,
        max_stdout_bytes: 262_144,
        max_stderr_bytes: 65_536,
        enabled_by_default: false,
    },
];

pub fn registry() -> &'static [CliBackend] {
    REGISTRY
}

pub fn lookup_cli(name: &str) -> Option<&'static CliBackend> {
    REGISTRY.iter().find(|b| b.name == name)
}

/// A resolved backend selector. `OpenRouter(model_id)` or `Cli(registry_name)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSpec {
    OpenRouter(String),
    Cli(String),
}

/// Parse a `openrouter:<model>` or `cli:<name>` spec string. No other schemes.
pub fn parse_backend_spec(s: &str) -> Result<BackendSpec> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("openrouter:") {
        let m = rest.trim();
        if m.is_empty() {
            return Err(anyhow!("backend spec 'openrouter:' missing a model id"));
        }
        return Ok(BackendSpec::OpenRouter(m.to_string()));
    }
    if let Some(rest) = s.strip_prefix("cli:") {
        let name = rest.trim();
        if lookup_cli(name).is_none() {
            return Err(anyhow!("unknown cli backend '{name}' (not in fixed registry)"));
        }
        return Ok(BackendSpec::Cli(name.to_string()));
    }
    Err(anyhow!(
        "backend spec '{s}' must start with 'openrouter:' or 'cli:'"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_claude_p_enabled_and_codex_kimi_gated() {
        assert!(lookup_cli("claude-p").unwrap().enabled_by_default);
        assert!(!lookup_cli("codex").unwrap().enabled_by_default);
        assert!(!lookup_cli("kimi").unwrap().enabled_by_default);
        assert!(lookup_cli("nope").is_none());
    }

    #[test]
    fn parse_openrouter_spec() {
        assert_eq!(
            parse_backend_spec("openrouter:openai/gpt-oss-120b").unwrap(),
            BackendSpec::OpenRouter("openai/gpt-oss-120b".into())
        );
    }

    #[test]
    fn parse_cli_spec_only_for_known_registry_names() {
        assert_eq!(parse_backend_spec("cli:claude-p").unwrap(), BackendSpec::Cli("claude-p".into()));
        assert!(parse_backend_spec("cli:rm-rf").is_err());
    }

    #[test]
    fn parse_rejects_unknown_scheme_and_empty_model() {
        assert!(parse_backend_spec("shell:bash").is_err());
        assert!(parse_backend_spec("openrouter:").is_err());
    }
}
