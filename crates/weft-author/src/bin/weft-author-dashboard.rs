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
        extract::Json,
        http::{header, StatusCode},
        response::IntoResponse,
        routing::{get, post},
        Router,
    };
    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use weft_author::authoring::{author_until_green, AuthorStatus};
    use weft_author::catalog_index::NodeIndex;
    use weft_author::intent::derive_spec;
    use weft_author::providers::{BlockingAuthor, OpenRouterAuthor};
    use weft_compiler::build::build_project_catalog;
    use weft_compiler::validate_file;
    use weft_core::node::Severity;

    // ── Embedded HTML ────────────────────────────────────────────────────────
    const DASHBOARD_HTML: &str = include_str!("../dashboard.html");

    // ── Fixture project path (baked in at compile time) ──────────────────────
    const DEFAULT_PROJECT_REL: &str =
        "../../weft-evals/fixtures/validation_required_ports/001/project";

    fn fixture_project() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_PROJECT_REL)
    }

    // ── API types ─────────────────────────────────────────────────────────────

    #[derive(Deserialize)]
    struct RunRequest {
        chat: String,
        model: String,
        max_rounds: u32,
    }

    #[derive(Serialize)]
    struct RoundDetail {
        round: u32,
        source: String,
        errors: Vec<DiagItem>,
    }

    #[derive(Serialize)]
    struct DiagItem {
        line: usize,
        column: usize,
        code: String,
        message: String,
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

    async fn api_run(Json(req): Json<RunRequest>) -> impl IntoResponse {
        let result = tokio::task::spawn_blocking(move || run_harness(req))
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

    // ── Blocking harness (called inside spawn_blocking) ───────────────────────

    fn run_harness(req: RunRequest) -> Result<RunResponse, String> {
        let model = req.model.clone();

        // 1. Resolve and copy the fixture project to a temp dir
        let fixture = fixture_project();
        let fixture = fixture
            .canonicalize()
            .map_err(|e| format!("fixture project not found at {}: {e}", fixture.display()))?;

        let tmp = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
        let tmp_root = tmp.path().to_path_buf();
        copy_dir(&fixture, &tmp_root).map_err(|e| format!("copy fixture: {e}"))?;
        let tmp_main = tmp_root.join("main.weft");

        // 2. Build catalog + node index
        let catalog = build_project_catalog(&tmp_root)
            .map_err(|e| format!("build_project_catalog: {e}"))?;
        let index = NodeIndex::build(&catalog);

        // 3. Build author
        let inner = OpenRouterAuthor::from_env(&req.model)
            .map_err(|e| format!("OpenRouterAuthor: {e}"))?;
        let author = BlockingAuthor::new(inner).map_err(|e| format!("BlockingAuthor: {e}"))?;

        // 4. derive_spec
        let spec = derive_spec(&author, &index, &req.chat)
            .map_err(|e| format!("derive_spec: {e}"))?;
        let spec_markdown = spec.to_markdown();

        // 5. Gated authoring loop — collect rounds
        let rounds_detail: Mutex<Vec<RoundDetail>> = Mutex::new(Vec::new());
        let round_counter = std::cell::Cell::new(0u32);

        let validate = |src: &str| -> Result<Vec<weft_core::node::Diagnostic>, String> {
            let n = round_counter.get() + 1;
            round_counter.set(n);

            std::fs::write(&tmp_main, src).map_err(|e| format!("write main.weft: {e}"))?;
            let diags = validate_file(&tmp_root, &tmp_main)?;

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

        // 6. Run `weft parse` (stdin) on the final weft to get graph JSON
        let graph = parse_weft_graph(&tmp_root, &outcome.weft);

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

    /// Run `weft parse` with the source on stdin, CWD = project root.
    /// Returns the parsed JSON value, or None on any failure.
    fn parse_weft_graph(project_root: &Path, weft_source: &str) -> Option<serde_json::Value> {
        if weft_source.is_empty() {
            return None;
        }
        let mut child = std::process::Command::new("weft")
            .arg("parse")
            .current_dir(project_root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;

        // Write source to stdin
        use std::io::Write;
        if let Some(stdin) = child.stdin.take() {
            let mut stdin = stdin;
            let _ = stdin.write_all(weft_source.as_bytes());
            // stdin dropped here → EOF
        }

        let output = child.wait_with_output().ok()?;
        if !output.status.success() && output.stdout.is_empty() {
            return None;
        }
        serde_json::from_slice(&output.stdout).ok()
    }

    /// Recursively copy `src` dir into `dst` (dst must not exist yet).
    fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dst)
            .map_err(|e| format!("create_dir_all {}: {e}", dst.display()))?;
        for entry in std::fs::read_dir(src)
            .map_err(|e| format!("read_dir {}: {e}", src.display()))?
        {
            let entry = entry.map_err(|e| format!("readdir entry: {e}"))?;
            let ft = entry
                .file_type()
                .map_err(|e| format!("file_type: {e}"))?;
            let dest_path = dst.join(entry.file_name());
            if ft.is_dir() {
                copy_dir(&entry.path(), &dest_path)?;
            } else {
                std::fs::copy(&entry.path(), &dest_path)
                    .map_err(|e| format!("copy {} -> {}: {e}", entry.path().display(), dest_path.display()))?;
            }
        }
        Ok(())
    }

    // ── Server entry point ────────────────────────────────────────────────────

    pub async fn run() {
        let app = Router::new()
            .route("/", get(index))
            .route("/api/run", post(api_run));

        let addr = "127.0.0.1:7878";
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .expect("failed to bind 127.0.0.1:7878");

        println!("Weft dashboard running at: http://{addr}");
        println!("Set OPENROUTER_API_KEY (or WORKDAY_OPENROUTER_API_KEY) before sending a run.");

        axum::serve(listener, app)
            .await
            .expect("server error");
    }
}

#[cfg(feature = "dashboard")]
#[tokio::main]
async fn main() {
    live::run().await;
}
