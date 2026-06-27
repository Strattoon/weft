// Dashboard server: chat -> derive_spec -> author_until_green -> axum HTTP + graph view.
// Requires the `dashboard` feature to run; the stub below keeps `cargo build` green
// without it.

#[cfg(not(feature = "dashboard"))]
fn main() {
    eprintln!(
        "Rebuild with --features dashboard to run the dashboard:\n\
         \n  cargo run -p weft-author --features dashboard --bin weft-author-dashboard\n"
    );
    std::process::exit(1);
}

// ─── Live binary (feature = "dashboard") ────────────────────────────────────

#[cfg(feature = "dashboard")]
mod live {
    use axum::{
        extract::Query,
        http::{header, StatusCode},
        response::{
            sse::{Event, KeepAlive, Sse},
            IntoResponse,
        },
        routing::{get, post},
        Json, Router,
    };
    use futures::stream::{self, Stream};
    use serde::{Deserialize, Serialize};
    use std::convert::Infallible;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;
    use weft_author::authoring::{author_until_green, AuthorStatus};
    use weft_author::catalog_index::NodeIndex;
    use weft_author::cli_provider::{resolve_backend, resolve_backend_with_usage, RuntimeMode};
    use weft_author::intent::derive_spec;
    use weft_author::providers::{BlockingAuthor, CallUsage, UsageLog};
    use weft_compiler::build::build_project_catalog;
    use weft_compiler::validate_file;
    use weft_core::node::Severity;

    // ── Embedded HTML ────────────────────────────────────────────────────────
    const DASHBOARD_HTML: &str = include_str!("../dashboard.html");

    // ── Persistent working project ───────────────────────────────────────────
    /// Returns the path to the persistent working project, creating it if needed.
    fn ensure_working_project() -> Result<PathBuf, String> {
        let home = std::env::var("HOME").map_err(|_| "HOME env var not set".to_string())?;
        let base = PathBuf::from(home).join(".weft-node-dashboard");
        let project = base.join("project");

        if !project.exists() {
            // Create the parent directory
            std::fs::create_dir_all(&base)
                .map_err(|e| format!("create_dir_all {}: {e}", base.display()))?;

            // Shell `weft new project` in the base dir to scaffold a real catalog-backed project
            let status = std::process::Command::new("weft")
                .arg("new")
                .arg("project")
                .current_dir(&base)
                .status()
                .map_err(|e| format!("failed to run `weft new project`: {e}"))?;

            if !status.success() {
                return Err(format!(
                    "`weft new project` failed with exit code {:?}",
                    status.code()
                ));
            }
        }

        Ok(project)
    }

    // ── API types ─────────────────────────────────────────────────────────────

    /// The promoted node generator: gpt-oss-120b. The bare id auto-routes to
    /// Cerebras in `OpenRouterAuthor::from_env`, so this is the fast path. Used
    /// whenever a request omits `model` or sends an empty string.
    pub(crate) const DEFAULT_NODE_GENERATOR: &str = "openai/gpt-oss-120b";

    fn default_model() -> String {
        DEFAULT_NODE_GENERATOR.to_string()
    }

    /// Fall back to the default node generator when the caller sends an empty model.
    fn resolve_model(model: String) -> String {
        if model.trim().is_empty() {
            default_model()
        } else {
            model
        }
    }

    /// Generator backend: explicit `generator` field wins; otherwise PRESERVE the
    /// legacy `model` field's behavior by wrapping it as an `openrouter:` spec.
    /// This keeps old clients that send only `model` controlling generation —
    /// `resolve_model` already maps empty → DEFAULT_NODE_GENERATOR, so an absent
    /// `model` still lands on the default. Backward-compatible.
    fn resolve_generator(generator: Option<String>, legacy_model: String) -> String {
        generator
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("openrouter:{}", resolve_model(legacy_model)))
    }

    /// Planner backend: explicit `planner` field, else same as the generator
    /// (backward compatible — planner defaults to whatever drives generation).
    fn resolve_planner(planner: Option<String>, generator_spec: &str) -> String {
        planner
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| generator_spec.to_string())
    }

    #[derive(Deserialize)]
    struct RunRequest {
        chat: String,
        #[serde(default = "default_model")]
        model: String,
        #[serde(default)]
        planner: Option<String>,
        #[serde(default)]
        generator: Option<String>,
        max_rounds: u32,
    }

    #[derive(Deserialize)]
    struct RunStreamQuery {
        chat: String,
        #[serde(default = "default_model")]
        model: String,
        #[serde(default)]
        planner: Option<String>,
        #[serde(default)]
        generator: Option<String>,
        max_rounds: u32,
    }

    #[derive(Serialize)]
    struct DiagItem {
        line: usize,
        column: usize,
        code: String,
        message: String,
    }

    // SSE event payloads
    #[derive(Serialize)]
    struct SpecPayload {
        markdown: String,
    }

    #[derive(Serialize)]
    struct RoundPayload {
        round: u32,
        source: String,
        errors: Vec<DiagItem>,
        graph: Option<serde_json::Value>,
    }

    #[derive(Serialize)]
    struct AuthoredPayload {
        status: String,
        rounds: u32,
        final_weft: String,
        graph: Option<serde_json::Value>,
    }

    #[derive(Serialize)]
    struct RunPayload {
        #[serde(skip_serializing_if = "Option::is_none")]
        color: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        daemon_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    }

    // Legacy POST /api/run types
    #[derive(Serialize)]
    struct RoundDetail {
        round: u32,
        source: String,
        errors: Vec<DiagItem>,
    }

    #[derive(Serialize)]
    struct ProvenancePayload {
        planner_backend: String,
        generator_backend: String,
    }

    /// Token + cost rollup for one backend over a run (summed across all its
    /// model calls). Empty for CLI/subscription backends (no per-token cost).
    #[derive(Serialize, Default)]
    struct UsageSummary {
        calls: usize,
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
        /// Summed USD cost across calls (only counts calls that reported a cost).
        cost_usd: f64,
        per_call: Vec<CallUsage>,
    }

    impl UsageSummary {
        fn from_log(log: &UsageLog) -> Self {
            let calls = log.lock().unwrap().clone();
            let mut s = UsageSummary {
                calls: calls.len(),
                per_call: calls.clone(),
                ..Default::default()
            };
            for c in &calls {
                s.prompt_tokens += c.prompt_tokens;
                s.completion_tokens += c.completion_tokens;
                s.total_tokens += c.total_tokens;
                s.cost_usd += c.cost.unwrap_or(0.0);
            }
            s
        }
    }

    #[derive(Serialize)]
    struct RunResponse {
        model: String,
        planner_backend: String,
        generator_backend: String,
        planner_usage: UsageSummary,
        generator_usage: UsageSummary,
        spec_markdown: String,
        status: String,
        rounds: u32,
        rounds_detail: Vec<RoundDetail>,
        final_weft: String,
        graph: Option<serde_json::Value>,
        error: Option<String>,
    }

    // ── Handlers ──────────────────────────────────────────────────────────────

    async fn index() -> impl IntoResponse {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            DASHBOARD_HTML,
        )
    }

    /// GET /api/run-stream?chat=...&model=...&max_rounds=...
    async fn api_run_stream(
        Query(q): Query<RunStreamQuery>,
    ) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
        // mpsc channel: harness -> SSE forwarder
        let (tx, rx) = mpsc::channel::<Event>(64);

        tokio::task::spawn_blocking(move || {
            run_harness_streaming(q, tx);
        });

        let output_stream = stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|ev| (Ok(ev), rx))
        });

        Sse::new(output_stream).keep_alive(KeepAlive::default())
    }

    /// POST /api/run — kept for backward compatibility
    async fn api_run(Json(req): Json<RunRequest>) -> impl IntoResponse {
        let result = tokio::task::spawn_blocking(move || run_harness_blocking(req))
            .await
            .unwrap_or_else(|e| {
                Ok(RunResponse {
                    model: String::new(),
                    planner_backend: String::new(),
                    generator_backend: String::new(),
                    planner_usage: UsageSummary::default(),
                    generator_usage: UsageSummary::default(),
                    spec_markdown: String::new(),
                    status: "error".into(),
                    rounds: 0,
                    rounds_detail: vec![],
                    final_weft: String::new(),
                    graph: None,
                    error: Some(format!("spawn_blocking panicked: {e}")),
                })
            });

        let resp = match result {
            Ok(r) => r,
            Err(e) => RunResponse {
                model: String::new(),
                planner_backend: String::new(),
                generator_backend: String::new(),
                planner_usage: UsageSummary::default(),
                generator_usage: UsageSummary::default(),
                spec_markdown: String::new(),
                status: "error".into(),
                rounds: 0,
                rounds_detail: vec![],
                final_weft: String::new(),
                graph: None,
                error: Some(e),
            },
        };

        (StatusCode::OK, axum::Json(resp))
    }

    // ── Streaming harness (runs inside spawn_blocking) ───────────────────────

    fn send_event(tx: &mpsc::Sender<Event>, event_name: &str, payload: impl Serialize) {
        let data = serde_json::to_string(&payload).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
        let ev = Event::default().event(event_name).data(data);
        // Best-effort: drop if channel is full / closed
        let _ = tx.blocking_send(ev);
    }

    fn run_harness_streaming(q: RunStreamQuery, tx: mpsc::Sender<Event>) {
        // Use the persistent working project
        let project_root = match ensure_working_project() {
            Ok(p) => p,
            Err(e) => {
                send_event(
                    &tx,
                    "authored",
                    AuthoredPayload {
                        status: "error".into(),
                        rounds: 0,
                        final_weft: String::new(),
                        graph: None,
                    },
                );
                // Surface the error via a "done" and return
                let _ = tx.blocking_send(
                    Event::default()
                        .event("done")
                        .data(format!("{{\"error\":\"{e}\"}}"))
                );
                return;
            }
        };

        // 1. Build catalog + node index from the persistent project
        let catalog = match build_project_catalog(&project_root) {
            Ok(c) => c,
            Err(e) => {
                send_event(
                    &tx,
                    "authored",
                    AuthoredPayload {
                        status: "error".into(),
                        rounds: 0,
                        final_weft: format!("build_project_catalog: {e}"),
                        graph: None,
                    },
                );
                let _ = tx.blocking_send(Event::default().event("done").data("{}"));
                return;
            }
        };
        let index = NodeIndex::build(&catalog);

        // 2. Build planner + generator backends
        let generator_spec = resolve_generator(q.generator.clone(), q.model.clone());
        let planner_spec = resolve_planner(q.planner.clone(), &generator_spec);

        let planner_inner = match resolve_backend(&planner_spec, RuntimeMode::LocalDev) {
            Ok(a) => a,
            Err(e) => {
                send_event(
                    &tx,
                    "authored",
                    AuthoredPayload {
                        status: "error".into(),
                        rounds: 0,
                        final_weft: format!("planner backend: {e}"),
                        graph: None,
                    },
                );
                let _ = tx.blocking_send(Event::default().event("done").data("{}"));
                return;
            }
        };
        let planner = match BlockingAuthor::new(planner_inner) {
            Ok(a) => a,
            Err(e) => {
                send_event(
                    &tx,
                    "authored",
                    AuthoredPayload {
                        status: "error".into(),
                        rounds: 0,
                        final_weft: format!("BlockingAuthor(planner): {e}"),
                        graph: None,
                    },
                );
                let _ = tx.blocking_send(Event::default().event("done").data("{}"));
                return;
            }
        };
        let generator_inner = match resolve_backend(&generator_spec, RuntimeMode::LocalDev) {
            Ok(a) => a,
            Err(e) => {
                send_event(
                    &tx,
                    "authored",
                    AuthoredPayload {
                        status: "error".into(),
                        rounds: 0,
                        final_weft: format!("generator backend: {e}"),
                        graph: None,
                    },
                );
                let _ = tx.blocking_send(Event::default().event("done").data("{}"));
                return;
            }
        };
        let generator = match BlockingAuthor::new(generator_inner) {
            Ok(a) => a,
            Err(e) => {
                send_event(
                    &tx,
                    "authored",
                    AuthoredPayload {
                        status: "error".into(),
                        rounds: 0,
                        final_weft: format!("BlockingAuthor(generator): {e}"),
                        graph: None,
                    },
                );
                let _ = tx.blocking_send(Event::default().event("done").data("{}"));
                return;
            }
        };

        // 3. Emit provenance event now that backends are resolved
        send_event(&tx, "provenance", ProvenancePayload {
            planner_backend: planner_spec.clone(),
            generator_backend: generator_spec.clone(),
        });

        // 4. derive_spec — emit `spec` event (non-blocking: no approval wait)
        let spec = match derive_spec(&planner, &index, &q.chat) {
            Ok(s) => s,
            Err(e) => {
                send_event(
                    &tx,
                    "authored",
                    AuthoredPayload {
                        status: "error".into(),
                        rounds: 0,
                        final_weft: format!("derive_spec: {e}"),
                        graph: None,
                    },
                );
                let _ = tx.blocking_send(Event::default().event("done").data("{}"));
                return;
            }
        };

        send_event(
            &tx,
            "spec",
            SpecPayload {
                markdown: spec.to_markdown(),
            },
        );

        let spec_markdown = spec.to_markdown();
        let main_weft = project_root.join("main.weft");
        let round_counter = std::cell::Cell::new(0u32);
        let tx_ref = &tx;
        let project_ref = &project_root;

        // 5. Authoring loop with per-round events
        let validate = |src: &str| -> Result<Vec<weft_core::node::Diagnostic>, String> {
            let n = round_counter.get() + 1;
            round_counter.set(n);

            std::fs::write(&main_weft, src).map_err(|e| format!("write main.weft: {e}"))?;

            let diags = validate_file(project_ref, &main_weft)?;

            let errors: Vec<DiagItem> = diags
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .map(|d| DiagItem {
                    line: d.line,
                    column: d.column,
                    code: d.code.clone().unwrap_or_default(),
                    message: d.message.clone(),
                })
                .collect();

            // Parse graph for this round's source
            let graph = parse_weft_graph(project_ref, src);

            send_event(
                tx_ref,
                "round",
                RoundPayload {
                    round: n,
                    source: src.to_owned(),
                    errors,
                    graph,
                },
            );

            Ok(diags)
        };

        let outcome = author_until_green(
            &generator,
            &catalog,
            &spec.selected_nodes,
            &spec_markdown,
            &validate,
            q.max_rounds,
        );

        let status_str = match outcome.status {
            AuthorStatus::Green => "green",
            AuthorStatus::ExhaustedRed => "exhausted_red",
            AuthorStatus::Error => "error",
        }
        .to_string();

        // Overwrite main.weft with the final result
        let _ = std::fs::write(&main_weft, &outcome.weft);

        let final_graph = parse_weft_graph(&project_root, &outcome.weft);

        send_event(
            &tx,
            "authored",
            AuthoredPayload {
                status: status_str.clone(),
                rounds: outcome.rounds,
                final_weft: outcome.weft.clone(),
                graph: final_graph,
            },
        );

        // 6. Attempt execution only if green
        if status_str == "green" {
            match run_weft_detached(&project_root) {
                Ok(color) => {
                    send_event(
                        &tx,
                        "run",
                        RunPayload {
                            color: Some(color),
                            daemon_url: Some("http://127.0.0.1:9999".to_string()),
                            error: None,
                        },
                    );
                }
                Err(e) => {
                    send_event(
                        &tx,
                        "run",
                        RunPayload {
                            color: None,
                            daemon_url: None,
                            error: Some(e),
                        },
                    );
                }
            }
        }

        // Signal stream end
        let _ = tx.blocking_send(Event::default().event("done").data("{}"));
    }

    // ── Blocking harness for POST /api/run (legacy) ──────────────────────────

    fn run_harness_blocking(req: RunRequest) -> Result<RunResponse, String> {
        let model = resolve_model(req.model.clone());

        let project_root = ensure_working_project()?;

        let catalog =
            build_project_catalog(&project_root).map_err(|e| format!("build_project_catalog: {e}"))?;
        let index = NodeIndex::build(&catalog);

        let generator_spec = resolve_generator(req.generator.clone(), req.model.clone());
        let planner_spec = resolve_planner(req.planner.clone(), &generator_spec);

        // Usage logs so we can report per-backend token spend after the run.
        // Only `openrouter:` backends populate these; CLI backends record nothing.
        let planner_log: UsageLog = Arc::new(Mutex::new(Vec::new()));
        let generator_log: UsageLog = Arc::new(Mutex::new(Vec::new()));

        let planner_inner =
            resolve_backend_with_usage(&planner_spec, RuntimeMode::LocalDev, Some(planner_log.clone()))
                .map_err(|e| format!("planner backend: {e}"))?;
        let planner = BlockingAuthor::new(planner_inner)
            .map_err(|e| format!("BlockingAuthor(planner): {e}"))?;
        let generator_inner = resolve_backend_with_usage(
            &generator_spec,
            RuntimeMode::LocalDev,
            Some(generator_log.clone()),
        )
        .map_err(|e| format!("generator backend: {e}"))?;
        let generator = BlockingAuthor::new(generator_inner)
            .map_err(|e| format!("BlockingAuthor(generator): {e}"))?;

        let spec = derive_spec(&planner, &index, &req.chat).map_err(|e| format!("derive_spec: {e}"))?;
        let spec_markdown = spec.to_markdown();

        let main_weft = project_root.join("main.weft");
        let rounds_detail: std::sync::Mutex<Vec<RoundDetail>> = std::sync::Mutex::new(Vec::new());
        let round_counter = std::cell::Cell::new(0u32);

        let validate = |src: &str| -> Result<Vec<weft_core::node::Diagnostic>, String> {
            let n = round_counter.get() + 1;
            round_counter.set(n);

            std::fs::write(&main_weft, src).map_err(|e| format!("write main.weft: {e}"))?;
            let diags = validate_file(&project_root, &main_weft)?;

            let errors: Vec<DiagItem> = diags
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .map(|d| DiagItem {
                    line: d.line,
                    column: d.column,
                    code: d.code.clone().unwrap_or_default(),
                    message: d.message.clone(),
                })
                .collect();

            rounds_detail.lock().unwrap().push(RoundDetail {
                round: n,
                source: src.to_owned(),
                errors,
            });

            Ok(diags)
        };

        let outcome = author_until_green(
            &generator,
            &catalog,
            &spec.selected_nodes,
            &spec_markdown,
            &validate,
            req.max_rounds,
        );

        let status = match outcome.status {
            AuthorStatus::Green => "green",
            AuthorStatus::ExhaustedRed => "exhausted_red",
            AuthorStatus::Error => "error",
        };

        let _ = std::fs::write(&main_weft, &outcome.weft);
        let graph = parse_weft_graph(&project_root, &outcome.weft);
        let rd = rounds_detail.into_inner().unwrap();

        Ok(RunResponse {
            model,
            planner_backend: planner_spec,
            generator_backend: generator_spec,
            planner_usage: UsageSummary::from_log(&planner_log),
            generator_usage: UsageSummary::from_log(&generator_log),
            spec_markdown,
            status: status.to_owned(),
            rounds: outcome.rounds,
            rounds_detail: rd,
            final_weft: outcome.weft,
            graph,
            error: None,
        })
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Run `weft parse` with the source written to project/main.weft, CWD = project root.
    /// Returns the parsed JSON value, or None on any failure.
    fn parse_weft_graph(project_root: &Path, weft_source: &str) -> Option<serde_json::Value> {
        if weft_source.is_empty() {
            return None;
        }
        // Write source to main.weft so the @file/@include base + catalog resolve
        // from the project. NOTE: `weft parse` reads the graph from STDIN; `--file`
        // only sets the include base. So we must also pipe the source to stdin —
        // without it parse reads empty stdin and returns an empty project.
        let main_weft = project_root.join("main.weft");
        std::fs::write(&main_weft, weft_source).ok()?;

        use std::io::Write;
        let mut child = std::process::Command::new("weft")
            .arg("parse")
            .arg("--file")
            .arg(&main_weft)
            .current_dir(project_root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        // Feed the source on stdin, then drop the handle to send EOF.
        child.stdin.take()?.write_all(weft_source.as_bytes()).ok()?;
        let output = child.wait_with_output().ok()?;

        if output.stdout.is_empty() {
            return None;
        }
        serde_json::from_slice(&output.stdout).ok()
    }

    /// Extract the execution color from `weft run --json` JSONL stdout.
    ///
    /// `weft run --json` emits one JSON object per line (JSONL), one per phase.
    /// The color lives in the `dispatcher_call_done` phase line:
    ///   `{"verb":"run","phase":"dispatcher_call_done","detail":{"color":"<uuid>",...}}`
    ///
    /// Strategy: scan all parseable lines; prefer a line with `detail.color` that is
    /// a string (the `dispatcher_call_done` line). Returns `None` if no color found.
    pub(crate) fn extract_run_color(stdout: &str) -> Option<String> {
        let mut found: Option<String> = None;
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(color) = v
                    .get("detail")
                    .and_then(|d| d.get("color"))
                    .and_then(|c| c.as_str())
                {
                    // Return immediately on a dedicated dispatcher_call_done hit
                    if v.get("phase").and_then(|p| p.as_str()) == Some("dispatcher_call_done") {
                        return Some(color.to_string());
                    }
                    // Keep as fallback if we find a color anywhere else
                    if found.is_none() {
                        found = Some(color.to_string());
                    }
                }
            }
        }
        found
    }

    /// Run `weft run --json --detach` in the project dir and parse the `color` UUID.
    fn run_weft_detached(project_root: &Path) -> Result<String, String> {
        let output = std::process::Command::new("weft")
            .arg("run")
            .arg("--json")
            .arg("--detach")
            // Each benchmark run is independent: cancel any executions still
            // running on the prior worker image so a rebuild isn't blocked.
            .arg("--running-policy")
            .arg("cancel")
            .current_dir(project_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| format!("failed to spawn `weft run`: {e}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Try to extract color from JSONL output first.
        // If a color is found the run started successfully regardless of exit code.
        if let Some(color) = extract_run_color(&stdout) {
            return Ok(color);
        }

        // No color found — surface a meaningful error.
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: String = stderr.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            return Err(format!(
                "weft run failed (exit {:?}): {tail}",
                output.status.code()
            ));
        }

        // Process exited zero but no color in output — report last parsed phase or raw tail.
        let last_phase: Option<String> = stdout
            .lines()
            .filter_map(|l| {
                serde_json::from_str::<serde_json::Value>(l.trim()).ok()
            })
            .filter_map(|v| v.get("phase").and_then(|p| p.as_str()).map(|s| s.to_string()))
            .last();

        let detail = if let Some(phase) = last_phase {
            format!("last phase: {phase}")
        } else {
            let raw: String = stdout.chars().take(200).collect();
            format!("raw output: {raw}")
        };

        Err(format!("weft run produced no color — {detail}"))
    }

    #[cfg(test)]
    mod tests {
        use super::extract_run_color;

        const SAMPLE_JSONL: &str = r#"{"ts_unix":1,"verb":"run","phase":"build_skip","detail":{"image":"abc"}}
{"ts_unix":2,"verb":"run","phase":"image_push_start","detail":{}}
{"ts_unix":3,"verb":"run","phase":"dispatcher_call_done","detail":{"color":"ebbca52b-2d9f-4530-8236-b16f417e3ae3","project_id":"p1"}}
{"ts_unix":4,"verb":"run","phase":"complete","detail":{"summary":"started ebbca52b-2d9f-4530-8236-b16f417e3ae3"}}"#;

        #[test]
        fn extract_run_color_finds_color_in_jsonl() {
            let color = extract_run_color(SAMPLE_JSONL);
            assert_eq!(
                color.as_deref(),
                Some("ebbca52b-2d9f-4530-8236-b16f417e3ae3")
            );
        }

        #[test]
        fn extract_run_color_returns_none_for_empty() {
            assert_eq!(extract_run_color(""), None);
        }

        #[test]
        fn extract_run_color_returns_none_for_colorless_jsonl() {
            let no_color = r#"{"ts_unix":1,"verb":"run","phase":"build_skip","detail":{"image":"abc"}}
{"ts_unix":4,"verb":"run","phase":"complete","detail":{"summary":"done"}}"#;
            assert_eq!(extract_run_color(no_color), None);
        }

        #[test]
        fn extract_run_color_ignores_non_json_lines() {
            let mixed = "not json at all\n{\"ts_unix\":1,\"verb\":\"run\",\"phase\":\"dispatcher_call_done\",\"detail\":{\"color\":\"aabbccdd-0000-0000-0000-000000000000\"}}";
            assert_eq!(
                extract_run_color(mixed).as_deref(),
                Some("aabbccdd-0000-0000-0000-000000000000")
            );
        }
    }

    // ── Server entry point ────────────────────────────────────────────────────

    pub async fn run() {
        let app = Router::new()
            .route("/", get(index))
            .route("/api/run-stream", get(api_run_stream))
            .route("/api/run", post(api_run));

        let addr = "127.0.0.1:7878";
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .expect("failed to bind 127.0.0.1:7878");

        println!("Weft dashboard running at: http://{addr}");
        println!("  GET  /api/run-stream?chat=...&model=...&max_rounds=...  (SSE stream)");
        println!("  POST /api/run  (blocking, legacy)");
        println!("Set OPENROUTER_API_KEY (or WORKDAY_OPENROUTER_API_KEY) before sending a run.");

        axum::serve(listener, app).await.expect("server error");
    }
}

#[cfg(feature = "dashboard")]
#[tokio::main]
async fn main() {
    live::run().await;
}
