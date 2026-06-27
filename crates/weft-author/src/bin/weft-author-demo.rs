// Demo driver: chat -> derive_spec -> author_until_green with real weft-compiler gate.
// Requires the `providers` feature to run live; the stub below keeps `cargo build` green
// without it.

#[cfg(not(feature = "providers"))]
fn main() {
    eprintln!(
        "Rebuild with --features providers to run the live demo:\n\
         \n  cargo run -p weft-author --features providers --bin weft-author-demo -- --chat '...'\n"
    );
    std::process::exit(1);
}

// ─── Live binary (feature = "providers") ────────────────────────────────────

#[cfg(feature = "providers")]
mod live {
    use anyhow::{Context, Result};
    use clap::Parser;
    use std::cell::Cell;
    use std::path::{Path, PathBuf};
    use weft_author::authoring::{author_until_green, AuthorStatus};
    use weft_author::catalog_index::NodeIndex;
    use weft_author::intent::derive_spec;
    use weft_author::providers::{BlockingAuthor, OpenRouterAuthor};
    use weft_compiler::build::build_project_catalog;
    use weft_compiler::validate_file;
    use weft_core::node::{MetadataCatalog, Severity};

    /// Default fixture project, relative to the crate manifest.
    const DEFAULT_PROJECT_REL: &str =
        "../weft-evals/fixtures/validation_required_ports/001/project";

    #[derive(Parser, Debug)]
    #[command(
        name = "weft-author-demo",
        about = "Demo: chat -> derive_spec -> author_until_green -> .weft (gated by weft-compiler)"
    )]
    struct Args {
        /// Free-form user request, e.g. \"show a greeting in the debug output when a gate passes\".
        #[arg(long)]
        chat: String,

        /// Path to the Weft project (must have nodes/ + weft.toml).
        #[arg(long, default_value_os_t = default_project())]
        project: PathBuf,

        /// OpenRouter model id.
        #[arg(long, default_value = "qwen/qwen3-coder")]
        model: String,

        /// Maximum propose/repair rounds before giving up.
        #[arg(long, default_value_t = 5)]
        max_rounds: u32,
    }

    fn default_project() -> PathBuf {
        // CARGO_MANIFEST_DIR is set by cargo at build time; resolve relative to it.
        let manifest = std::env::var("CARGO_MANIFEST_DIR")
            .unwrap_or_else(|_| ".".to_string());
        PathBuf::from(manifest).join(DEFAULT_PROJECT_REL)
    }

    /// Recursively copy `src` dir into `dst` (dst must not exist yet).
    fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
        std::fs::create_dir_all(dst)
            .with_context(|| format!("create_dir_all {}", dst.display()))?;
        for entry in std::fs::read_dir(src)
            .with_context(|| format!("read_dir {}", src.display()))?
        {
            let entry = entry.with_context(|| format!("readdir entry in {}", src.display()))?;
            let ft = entry
                .file_type()
                .with_context(|| format!("file_type for {}", entry.path().display()))?;
            let dest_path = dst.join(entry.file_name());
            if ft.is_dir() {
                copy_dir(&entry.path(), &dest_path)?;
            } else {
                std::fs::copy(&entry.path(), &dest_path)
                    .with_context(|| format!("copy {} -> {}", entry.path().display(), dest_path.display()))?;
            }
        }
        Ok(())
    }

    pub fn run() -> Result<()> {
        let args = Args::parse();

        let project = args.project.canonicalize().with_context(|| {
            format!(
                "project path not found: {} — pass --project <PATH> to a Weft project directory",
                args.project.display()
            )
        })?;

        // ── Header ──────────────────────────────────────────────────────────
        println!("═══════════════════════════════════════════════════════════");
        println!("  weft-author-demo");
        println!("  chat   : {}", args.chat);
        println!("  model  : {}", args.model);
        println!("  project: {}", project.display());
        println!("  rounds : {}", args.max_rounds);
        println!("═══════════════════════════════════════════════════════════");
        println!();

        // ── 1. Build catalog + node index ─────────────────────────────────
        println!("[1/4] Building catalog from project nodes...");
        let catalog = build_project_catalog(&project)
            .map_err(|e| anyhow::anyhow!("build_project_catalog failed: {e}"))?;
        let index = NodeIndex::build(&catalog);
        println!("      Catalog ready ({} nodes).", catalog.all().len());
        println!();

        // ── 2. Build live author ──────────────────────────────────────────
        println!("[2/4] Connecting to OpenRouter (model={})...", args.model);
        let inner = OpenRouterAuthor::from_env(&args.model)?;
        let author = BlockingAuthor::new(inner)?;
        println!("      Author ready.");
        println!();

        // ── 3. Intent → spec ──────────────────────────────────────────────
        println!("[3/4] Deriving spec from chat...");
        let spec = derive_spec(&author, &index, &args.chat)
            .context("derive_spec failed — check model output")?;

        println!();
        println!("┌─ PROSE SPEC (readback) ────────────────────────────────");
        for line in spec.to_markdown().lines() {
            println!("│ {line}");
        }
        println!("└────────────────────────────────────────────────────────");
        println!();

        // ── 4. Gated authoring loop ───────────────────────────────────────
        println!("[4/4] Authoring .weft (up to {} rounds)...", args.max_rounds);

        // Copy the project once to a temp dir so we never mutate the fixture.
        let tmp = tempfile::tempdir().context("create tempdir")?;
        let tmp_root = tmp.path().to_path_buf();
        copy_dir(&project, &tmp_root)?;
        let tmp_main = tmp_root.join("main.weft");

        let round_counter = Cell::new(0u32);

        let validate = |src: &str| -> Result<Vec<weft_core::node::Diagnostic>, String> {
            let n = round_counter.get() + 1;
            round_counter.set(n);

            std::fs::write(&tmp_main, src)
                .map_err(|e| format!("write main.weft: {e}"))?;

            let diags = validate_file(&tmp_root, &tmp_main)?;

            let err_count = diags
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .count();

            println!();
            println!("--- validation round {n}: {err_count} error(s)");
            for d in diags.iter().filter(|d| d.severity == Severity::Error) {
                println!(
                    "    L{}:{} [{}] {}",
                    d.line,
                    d.column,
                    d.code.as_deref().unwrap_or(""),
                    d.message
                );
            }

            Ok(diags)
        };

        let outcome = author_until_green(
            &author,
            &catalog,
            &spec.selected_nodes,
            &spec.to_markdown(),
            &validate,
            args.max_rounds,
        );

        // ── Results ───────────────────────────────────────────────────────
        println!();
        println!("═══════════════════════════════════════════════════════════");
        let (marker, label) = match outcome.status {
            AuthorStatus::Green => ("✅", "GREEN"),
            AuthorStatus::ExhaustedRed => ("❌", "EXHAUSTED_RED"),
            AuthorStatus::Error => ("❌", "ERROR"),
        };
        println!("{marker} Status: {label}  (rounds used: {})", outcome.rounds);
        println!("═══════════════════════════════════════════════════════════");
        println!();
        println!("─── Final .weft output ─────────────────────────────────");
        println!("{}", outcome.weft);
        println!("────────────────────────────────────────────────────────");

        if outcome.status != AuthorStatus::Green {
            std::process::exit(1);
        }

        Ok(())
    }
}

#[cfg(feature = "providers")]
fn main() {
    if let Err(e) = live::run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
