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

use crate::providers::{strip_code_fences, AsyncAuthor, OpenRouterAuthor};
use async_trait::async_trait;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{timeout, Duration};

/// Read up to `cap` bytes from `r`, then keep draining (discarding) so the
/// child never blocks on a full pipe. Returns (bytes, truncated).
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R, cap: usize) -> Result<(Vec<u8>, bool)> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if buf.len() < cap {
            let take = (cap - buf.len()).min(n);
            buf.extend_from_slice(&chunk[..take]);
            if take < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    Ok((buf, truncated))
}

/// Decode capped bytes to a string, lossily for non-UTF-8 input, and append
/// explicit markers so callers (and the probe) can detect both lossy decode and
/// truncation. The markers are why the probe MUST compare raw bytes BEFORE any
/// fence-stripping (see `propose`/`repair` vs `run_raw`).
fn decode(bytes: Vec<u8>, truncated: bool) -> String {
    let mut lossy = false;
    let mut s = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            lossy = true;
            String::from_utf8_lossy(e.as_bytes()).into_owned()
        }
    };
    if lossy {
        s.push_str("\n…[non-utf8-lossy]");
    }
    if truncated {
        s.push_str("\n…[truncated]");
    }
    s
}

/// Raw result of one child run. `propose`/`repair`/`probe_backend` all build on
/// this; only `propose`/`repair` strip code fences — the probe inspects raw stdout.
pub struct RawCliOutput {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// Local-only CLI planner/generator. Prompt → child stdin; final text ← stdout.
pub struct CliAuthor {
    backend: &'static CliBackend,
}

impl CliAuthor {
    pub fn new(backend: &'static CliBackend) -> Self {
        Self { backend }
    }

    /// Spawn the backend, write `prompt` to stdin, capture capped stdout/stderr,
    /// enforce the timeout (killing AND reaping the child), and return the RAW
    /// captured output (NO fence-stripping). This is the single source of truth
    /// for both the author path and the probe path.
    pub async fn run_raw(&self, prompt: String) -> Result<RawCliOutput> {
        let b = self.backend;
        let mut child = tokio::process::Command::new(b.program)
            .args(b.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("CliAuthor({}): spawn '{}' failed: {e}", b.name, b.program))?;

        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin
            .write_all(prompt.as_bytes())
            .await
            .map_err(|e| anyhow!("CliAuthor({}): write stdin failed: {e}", b.name))?;
        drop(stdin); // close stdin so the CLI sees EOF

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let read_out = read_capped(stdout, b.max_stdout_bytes);
        let read_err = read_capped(stderr, b.max_stderr_bytes);

        let name = b.name;
        let secs = b.timeout_secs;
        let wait = async {
            match timeout(Duration::from_secs(secs), child.wait()).await {
                Ok(s) => s.map_err(|e| anyhow!("CliAuthor({name}): wait failed: {e}")),
                Err(_) => {
                    // Kill AND reap so we don't leave a zombie/orphan behind.
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    Err(anyhow!("CliAuthor({name}): timed out after {secs}s"))
                }
            }
        };

        let (status, out_res, err_res) = tokio::join!(wait, read_out, read_err);
        let status = status?;
        let (out_bytes, out_trunc) = out_res?;
        let (err_bytes, err_trunc) = err_res?;
        Ok(RawCliOutput {
            status,
            stdout: decode(out_bytes, out_trunc),
            stderr: decode(err_bytes, err_trunc),
            stdout_truncated: out_trunc,
            stderr_truncated: err_trunc,
        })
    }

    /// Author path: run, enforce exit/empty rules, then strip code fences.
    async fn run(&self, prompt: String) -> Result<String> {
        let b = self.backend;
        let raw = self.run_raw(prompt).await?;
        if !raw.status.success() {
            return Err(anyhow!(
                "CliAuthor({}): exit {:?}; stderr: {}",
                b.name,
                raw.status.code(),
                raw.stderr
            ));
        }
        let trimmed = raw.stdout.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("CliAuthor({}): returned empty output", b.name));
        }
        Ok(strip_code_fences(trimmed))
    }
}

#[async_trait]
impl AsyncAuthor for CliAuthor {
    async fn propose(&self, context: &str) -> Result<String> {
        self.run(context.to_owned()).await
    }
    async fn repair(&self, context: &str, prev: &str, diagnostics: &str) -> Result<String> {
        let msg = format!(
            "{context}\n\nPrevious attempt:\n{prev}\n\nFix these diagnostics:\n{diagnostics}\n\nReturn the corrected output only."
        );
        self.run(msg).await
    }
}

use std::path::PathBuf;

pub const PROBE_OK: &str = "WEFT_CLI_PROBE_OK";
pub const PROBE_PROMPT: &str = "Reply with exactly:\nWEFT_CLI_PROBE_OK";

/// Local probe cache file: `.weft/author-cli-probes.json` under the CWD.
pub fn probe_cache_path() -> PathBuf {
    PathBuf::from(".weft").join("author-cli-probes.json")
}

fn load_cache() -> serde_json::Map<String, serde_json::Value> {
    match std::fs::read_to_string(probe_cache_path()) {
        Ok(s) => serde_json::from_str::<serde_json::Value>(&s)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default(),
        Err(_) => serde_json::Map::new(),
    }
}

pub fn cached_probe_passed(name: &str) -> bool {
    load_cache()
        .get(name)
        .and_then(|e| e.get("passed"))
        .and_then(|p| p.as_bool())
        .unwrap_or(false)
}

/// Resolution-time usability: enabled-by-default backends are always usable;
/// others require a cached probe pass.
pub fn status_of(name: &str) -> CliBackendStatus {
    match lookup_cli(name) {
        None => CliBackendStatus::ProbeFailed,
        Some(b) if b.enabled_by_default => CliBackendStatus::EnabledByDefault,
        Some(_) if cached_probe_passed(name) => CliBackendStatus::ProbePassed,
        Some(_) => CliBackendStatus::ProbeRequired,
    }
}

fn write_cache_pass(b: &CliBackend) -> Result<()> {
    let mut cache = load_cache();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    cache.insert(
        b.name.to_string(),
        serde_json::json!({
            "passed": true,
            "program": b.program,
            "args": b.args,
            "checked_at": now,
        }),
    );
    if let Some(dir) = probe_cache_path().parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(probe_cache_path(), serde_json::to_string_pretty(&cache)?)?;
    Ok(())
}

/// Probe a backend DEFINITION directly via the RAW path. The probe contract is
/// strict: the CLI must emit ONLY the token as final text — fenced output,
/// chatter, logs, non-zero exit, or truncated streams all FAIL. We therefore use
/// `run_raw` (NOT `propose`, which strips fences and would let `\`\`\`OK\`\`\``
/// pass). On pass, record the backend in the cache.
async fn probe_backend_def(b: &'static CliBackend) -> Result<()> {
    let raw = CliAuthor::new(b).run_raw(PROBE_PROMPT.to_string()).await?;
    let passed = raw.status.success()
        && raw.stdout.trim() == PROBE_OK
        && !raw.stdout_truncated
        && !raw.stderr_truncated;
    if passed {
        write_cache_pass(b)?;
        Ok(())
    } else {
        Err(anyhow!(
            "probe for '{}' failed: expected exactly {PROBE_OK:?} as the ONLY raw stdout (exit 0, untruncated); got status {:?}, stdout {:?} (fences/chatter/logs => needs a normalizing wrapper)",
            b.name,
            raw.status.code(),
            raw.stdout.trim()
        ))
    }
}

/// Run the exact-match probe against a registry backend by name. On pass, record
/// it in the cache.
pub async fn probe_backend(name: &str) -> Result<()> {
    let b = lookup_cli(name).ok_or_else(|| anyhow!("unknown cli backend '{name}'"))?;
    probe_backend_def(b).await
}

/// Where the harness is running. CLI backends are LOCAL-ONLY and are
/// mechanically blocked in `Hosted` — even a valid `cli:` spec errors there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMode {
    LocalDev,
    Hosted,
}

/// Resolve a backend spec to a live `AsyncAuthor`. `openrouter:` works in any
/// mode; `cli:` is rejected unless `mode == LocalDev` AND the backend is
/// enabled-by-default or has a cached probe pass.
pub fn resolve_backend(
    spec: &str,
    mode: RuntimeMode,
) -> Result<Box<dyn AsyncAuthor + Send + Sync>> {
    match parse_backend_spec(spec)? {
        BackendSpec::OpenRouter(model) => {
            let a = OpenRouterAuthor::from_env(model)?;
            Ok(Box::new(a))
        }
        // Local-only gate FIRST: never resolve a CLI backend in hosted mode,
        // regardless of probe/cache state. Phrased as `!= LocalDev` (not
        // `== Hosted`) deliberately: if a third RuntimeMode is ever added, this
        // stays safe-by-default (any non-LocalDev mode blocks CLI backends).
        BackendSpec::Cli(_) if mode != RuntimeMode::LocalDev => Err(anyhow!(
            "CLI backends are local-only and disabled in hosted mode"
        )),
        BackendSpec::Cli(name) => match status_of(&name) {
            CliBackendStatus::EnabledByDefault | CliBackendStatus::ProbePassed => {
                let b = lookup_cli(&name).expect("validated by parse_backend_spec");
                Ok(Box::new(CliAuthor::new(b)))
            }
            CliBackendStatus::ProbeRequired => Err(anyhow!(
                "cli:{name} is disabled until probed — run `weft-author-probe-cli --backend cli:{name}`"
            )),
            CliBackendStatus::ProbeFailed => Err(anyhow!("cli:{name} is not usable")),
        },
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    // Share the SAME process-global CWD lock + guard as the cache tests.
    use super::{lock_cwd, ChdirGuard};

    #[test]
    fn resolve_rejects_unprobed_codex_in_local_dev() {
        let _lock = lock_cwd();
        let dir = tempfile::tempdir().unwrap();
        let _g = ChdirGuard::to(dir.path());
        let r = resolve_backend("cli:codex", RuntimeMode::LocalDev);
        assert!(r.is_err());
        assert!(r.err().unwrap().to_string().contains("disabled until probed"));
    }

    #[test]
    fn resolve_rejects_cli_in_hosted_mode() {
        // Even claude-p (enabled-by-default) is blocked in hosted mode.
        let r = resolve_backend("cli:claude-p", RuntimeMode::Hosted);
        assert!(r.is_err());
        assert!(r.err().unwrap().to_string().contains("local-only"));
    }

    #[test]
    fn resolve_rejects_unknown_scheme() {
        assert!(resolve_backend("shell:bash", RuntimeMode::LocalDev).is_err());
    }
}

#[cfg(test)]
pub(crate) static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the CWD lock, recovering from poisoning so one panicking test does not
/// cascade-fail every other CWD test.
#[cfg(test)]
pub(crate) fn lock_cwd() -> std::sync::MutexGuard<'static, ()> {
    CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Test-only CWD guard: chdir on construction, restore on drop. Hold a `lock_cwd`
/// guard for the whole scope alongside it.
#[cfg(test)]
pub(crate) struct ChdirGuard(std::path::PathBuf);
#[cfg(test)]
impl ChdirGuard {
    pub(crate) fn to(p: &std::path::Path) -> Self {
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(p).unwrap();
        ChdirGuard(prev)
    }
}
#[cfg(test)]
impl Drop for ChdirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
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

    use super::{CliAuthor, CliBackend};
    use crate::providers::AsyncAuthor;

    // A backend that echoes stdin via `cat` — deterministic, no network.
    const CAT: CliBackend = CliBackend {
        name: "test-cat", program: "cat", args: &[],
        timeout_secs: 5, max_stdout_bytes: 262_144, max_stderr_bytes: 65_536,
        enabled_by_default: false,
    };

    #[tokio::test]
    async fn cli_author_round_trips_stdin_to_stdout() {
        let a = CliAuthor::new(&CAT);
        let out = a.propose("hello plan").await.unwrap();
        assert_eq!(out, "hello plan");
    }

    #[tokio::test]
    async fn cli_author_strips_code_fences() {
        let a = CliAuthor::new(&CAT);
        let out = a.propose("```\nGoal: x\n```").await.unwrap();
        assert_eq!(out, "Goal: x");
    }

    #[tokio::test]
    async fn cli_author_errors_on_nonzero_exit() {
        const FALSE: CliBackend = CliBackend {
            name: "test-false", program: "false", args: &[],
            timeout_secs: 5, max_stdout_bytes: 1024, max_stderr_bytes: 1024,
            enabled_by_default: false,
        };
        let err = CliAuthor::new(&FALSE).propose("x").await.unwrap_err();
        assert!(err.to_string().contains("exit"), "got: {err}");
    }

    #[tokio::test]
    async fn cli_author_errors_on_empty_stdout() {
        const TRUE: CliBackend = CliBackend {
            name: "test-true", program: "true", args: &[],
            timeout_secs: 5, max_stdout_bytes: 1024, max_stderr_bytes: 1024,
            enabled_by_default: false,
        };
        let err = CliAuthor::new(&TRUE).propose("x").await.unwrap_err();
        assert!(err.to_string().contains("empty output"), "got: {err}");
    }

    #[tokio::test]
    async fn cli_author_times_out_and_kills() {
        const SLEEP: CliBackend = CliBackend {
            name: "test-sleep", program: "sleep", args: &["30"],
            timeout_secs: 1, max_stdout_bytes: 1024, max_stderr_bytes: 1024,
            enabled_by_default: false,
        };
        let err = CliAuthor::new(&SLEEP).propose("x").await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "got: {err}");
    }

    #[tokio::test]
    async fn cli_author_truncates_oversized_stdout() {
        // Emit FINITE large output then exit 0 (must NOT use `yes`, which never
        // exits and would trip the timeout error path instead of truncating).
        // `head -c` from /dev/zero | tr → deterministic, finite, larger than cap.
        const BIG: CliBackend = CliBackend {
            name: "test-big", program: "sh",
            args: &["-c", "head -c 100000 /dev/zero | tr '\\0' 'a'"],
            timeout_secs: 5, max_stdout_bytes: 64, max_stderr_bytes: 1024,
            enabled_by_default: false,
        };
        let out = CliAuthor::new(&BIG).propose("x").await.unwrap();
        assert!(out.contains("[truncated]"), "got len {}", out.len());
    }

    #[tokio::test]
    async fn cli_author_marks_non_utf8_output() {
        // Emit raw invalid UTF-8 (0xff bytes) then exit 0 → lossy decode + marker.
        const NONUTF8: CliBackend = CliBackend {
            name: "test-nonutf8", program: "sh",
            args: &["-c", "printf '\\377\\377\\377'"],
            timeout_secs: 5, max_stdout_bytes: 1024, max_stderr_bytes: 1024,
            enabled_by_default: false,
        };
        let out = CliAuthor::new(&NONUTF8).propose("x").await.unwrap();
        assert!(out.contains("[non-utf8-lossy]"), "got: {out:?}");
    }

    use super::{
        cached_probe_passed, lock_cwd, probe_backend_def, status_of, ChdirGuard,
        CliBackendStatus,
    };

    #[tokio::test]
    async fn probe_pass_writes_cache_and_flips_status() {
        let _lock = lock_cwd();
        let dir = tempfile::tempdir().unwrap();
        let _guard = ChdirGuard::to(dir.path());

        // A fake "codex"-like backend that emits EXACTLY the probe token and exits 0.
        // Reuse the real `codex` registry name (probe-gated) so we exercise the
        // gated path, but with a printf program instead of the real CLI.
        static FAKE_OK: CliBackend = CliBackend {
            name: "codex", program: "printf", args: &["WEFT_CLI_PROBE_OK"],
            timeout_secs: 5, max_stdout_bytes: 1024, max_stderr_bytes: 1024,
            enabled_by_default: false,
        };

        // Before: gated, no cache.
        assert_eq!(status_of("codex"), CliBackendStatus::ProbeRequired);
        assert!(!cached_probe_passed("codex"));

        // Probe the fake def directly → writes the cache on pass.
        probe_backend_def(&FAKE_OK).await.unwrap();

        // After: cache written and status flipped.
        assert!(cached_probe_passed("codex"));
        assert_eq!(status_of("codex"), CliBackendStatus::ProbePassed);
    }

    #[tokio::test]
    async fn probe_rejects_fenced_output() {
        let _lock = lock_cwd();
        let dir = tempfile::tempdir().unwrap();
        let _guard = ChdirGuard::to(dir.path());

        // Emits the token wrapped in a code fence → must FAIL (raw stdout != token),
        // proving the probe does NOT strip fences.
        static FAKE_FENCED: CliBackend = CliBackend {
            name: "codex", program: "printf", args: &["```\nWEFT_CLI_PROBE_OK\n```"],
            timeout_secs: 5, max_stdout_bytes: 1024, max_stderr_bytes: 1024,
            enabled_by_default: false,
        };
        assert!(probe_backend_def(&FAKE_FENCED).await.is_err());
        assert!(!cached_probe_passed("codex"));
    }

    #[test]
    fn status_enabled_for_claude_p_and_required_for_codex_without_cache() {
        let _lock = lock_cwd();
        let dir = tempfile::tempdir().unwrap();
        let _g = ChdirGuard::to(dir.path());
        assert_eq!(status_of("claude-p"), CliBackendStatus::EnabledByDefault);
        assert_eq!(status_of("codex"), CliBackendStatus::ProbeRequired);
        assert!(!cached_probe_passed("codex"));
    }
}
