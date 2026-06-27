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
    use tokio::sync::mpsc;
    use weft_author::authoring::{author_until_green, AuthorStatus};
    use weft_author::catalog_index::NodeIndex;
    use weft_author::intent::derive_spec;
    use weft_author::providers::{BlockingAuthor, OpenRouterAuthor};
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

    #[derive(Deserialize)]
    struct RunRequest {
        chat: String,
        model: String,
        max_rounds: u32,
    }

    #[derive(Deserialize)]
    struct RunStreamQuery {
        chat: String,
        model: String,
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
    struct RunResponse {
        model: String,
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

        // 2. Build author
        let inner = match OpenRouterAuthor::from_env(&q.model) {
            Ok(a) => a,
            Err(e) => {
                send_event(
                    &tx,
                    "authored",
                    AuthoredPayload {
                        status: "error".into(),
                        rounds: 0,
                        final_weft: format!("OpenRouterAuthor: {e}"),
                        graph: None,
                    },
                );
                let _ = tx.blocking_send(Event::default().event("done").data("{}"));
                return;
            }
        };
        let author = match BlockingAuthor::new(inner) {
            Ok(a) => a,
            Err(e) => {
                send_event(
                    &tx,
                    "authored",
                    AuthoredPayload {
                        status: "error".into(),
                        rounds: 0,
                        final_weft: format!("BlockingAuthor: {e}"),
                        graph: None,
                    },
                );
                let _ = tx.blocking_send(Event::default().event("done").data("{}"));
                return;
            }
        };

        // 3. derive_spec — emit `spec` event
        let spec = match derive_spec(&author, &index, &q.chat) {
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

        // 4. Authoring loop with per-round events
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
            &author,
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

        // 5. Attempt execution only if green
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
        let model = req.model.clone();

        let project_root = ensure_working_project()?;

        let catalog =
            build_project_catalog(&project_root).map_err(|e| format!("build_project_catalog: {e}"))?;
        let index = NodeIndex::build(&catalog);

        let inner =
            OpenRouterAuthor::from_env(&req.model).map_err(|e| format!("OpenRouterAuthor: {e}"))?;
        let author = BlockingAuthor::new(inner).map_err(|e| format!("BlockingAuthor: {e}"))?;

        let spec = derive_spec(&author, &index, &req.chat).map_err(|e| format!("derive_spec: {e}"))?;
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
            &author,
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
        // Write source to main.weft first (the project context is needed for catalog resolution)
        let main_weft = project_root.join("main.weft");
        std::fs::write(&main_weft, weft_source).ok()?;

        let output = std::process::Command::new("weft")
            .arg("parse")
            .arg("--file")
            .arg(&main_weft)
            .current_dir(project_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;

        if output.stdout.is_empty() {
            return None;
        }
        serde_json::from_slice(&output.stdout).ok()
    }

    /// Run `weft run --json --detach` in the project dir and parse the `color` UUID.
    fn run_weft_detached(project_root: &Path) -> Result<String, String> {
        let output = std::process::Command::new("weft")
            .arg("run")
            .arg("--json")
            .arg("--detach")
            .current_dir(project_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| format!("failed to spawn `weft run`: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("weft run failed: {stderr}"));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        // Parse JSON — expect `{"color":"<uuid>", ...}`
        let v: serde_json::Value = serde_json::from_str(stdout.trim())
            .map_err(|e| format!("weft run output was not JSON: {e} — raw: {stdout}"))?;

        v["color"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| format!("weft run JSON had no `color` field: {stdout}"))
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
