//! The weft compiler. Turns a project directory (`main.weft`, `nodes/`,
//! `weft.toml`) into a compiled rust binary.
//!
//! Pipeline:
//! 1. `project::load` reads the project manifest and the graph source.
//! 2. `parser::parse_weft` turns the weft source into a graph AST.
//! 3. `enrich::enrich` resolves TypeVars, dynamic ports, and form-
//!    derived ports (ported from v1 in phase A2).
//! 4. `validate::validate` checks callback isolation, entry-point
//!    detection, required-port coverage.
//! 5. `codegen::emit` produces rust source files that link the graph +
//!    every referenced node (all from the project's `nodes/`).
//! 6. `build::invoke_cargo` runs cargo to produce the binary.

pub mod project;
pub mod source_name;
pub mod weft_compiler;
pub mod cst;
pub mod edit;
pub mod file_ref;
pub mod enrich;
pub mod validate;
pub mod lower_state_machine;
pub mod codegen;
pub mod worker_image;
pub mod build;
pub mod error;

pub use error::{CompileError as ProjectError, CompileResult};
pub use weft_compiler::{compile as compile_source, CompileError as SourceError};

use uuid::Uuid;
use weft_core::{MetadataCatalog, ProjectDefinition};

// Re-export weft_core's Diagnostic/Severity so downstream callers
// keep using weft_compiler::Diagnostic without touching node impls.
pub use weft_core::node::{Diagnostic, Severity};
use weft_core::project::Span;

/// Fast-path parse for interactive editing (IDE, live preview). Runs
/// lex + parse + flatten + lenient enrich. Does NOT run validation:
/// the slow-path `validate()` does that on a longer debounce.
///
/// Unknown node types, missing catalog entries, and malformed partial
/// programs produce diagnostics but don't abort; the returned project
/// is always usable for rendering.
pub fn parse_only(
    source: &str,
    project_id: Uuid,
    base_dir: Option<&std::path::Path>,
    catalog: &dyn MetadataCatalog,
    source_name: Option<&str>,
) -> (ProjectDefinition, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();

    // Stages 1+2: lex + parse + flatten, LENIENT. A bad line becomes a
    // diagnostic; every valid node/edge around it still renders (the editor
    // must keep showing the graph mid-edit, never blank out on one typo).
    // Interface mode for @include: opaque nodes the editor navigates into.
    // `source_name` is the file's identity (e.g. `MyCleaner`); an anonymous
    // top-level group takes it as its id, so the file's root carries the same
    // id at parse, edit, and render with no sentinel to rename later.
    let (mut project, parse_errors) = weft_compiler::compile_lenient(
        source,
        project_id,
        base_dir,
        weft_compiler::IncludeMode::Interface,
        source_name,
    );
    for e in parse_errors {
        diagnostics.push(Diagnostic::at(e.span, Severity::Error, "parse", e.message));
    }

    // Stage 3: enrich (lenient). Unknown types / catalog misses become
    // empty-port placeholders, not aborts.
    if let Err(e) = enrich::enrich_with_policy(&mut project, catalog, enrich::EnrichPolicy::Lenient) {
        diagnostics.push(Diagnostic::at(Span::default(), Severity::Warning, "enrich", format!("{e}")));
    }

    // Surface unknown node types as warnings so the IDE can paint a
    // squiggly on the header line even without calling /validate.
    for node in &project.nodes {
        // Opaque `@include` interface nodes carry no catalog entry by design
        // (their ports come from the included file's Group header). Don't
        // flag them as unknown types.
        if node.include_path.is_some() {
            continue;
        }
        if catalog.lookup(&node.node_type).is_none() {
            diagnostics.push(Diagnostic::at(
                node.header_span_or_default(),
                Severity::Warning,
                "unknown-type",
                format!("unknown node type '{}'", node.node_type),
            ));
        }
    }

    // Structural validate so the IDE gets inline feedback for
    // graph-shape problems (no-output-node, unreachable-from-output,
    // duplicate ids, etc.) directly from /parse. Runtime-only rules
    // still only fire from the dedicated /validate endpoint. The source's
    // StateMachine blocks are validated too, so an SM-only file gets live
    // SM diagnostics (and is not flagged for a missing graph output).
    let sms = weft_compiler::extract_state_machines(source);
    diagnostics.extend(validate::validate_with_mode(
        &project,
        &sms,
        catalog,
        validate::ValidationMode::Structural,
    ));

    (project, diagnostics)
}

/// Strict sibling of `parse_only`: the full pipeline (lex + parse +
/// flatten, strict enrich, validate) collecting structured diagnostics
/// instead of aborting. This is the single home for the
/// error-to-`Diagnostic` mapping; every strict caller (the editor's
/// `weft validate`, and `compile_checked` below for build/hash) goes
/// through it, so the four paths can't drift.
///
/// `mode` selects how much validation runs: `Structural` (graph shape)
/// or `Runtime` (also missing-credential style rules). The editor's
/// Problems panel wants `Runtime`; the build gate wants `Structural`
/// (a project may legitimately build without every secret filled).
///
/// Never aborts: a parse failure returns an empty project plus the
/// parse diagnostics, mirroring `parse_only`, so a caller that only
/// wants diagnostics (the editor) gets them uniformly. Callers that
/// must abort on errors use `compile_checked`.
pub fn compile_strict(
    source: &str,
    project_id: Uuid,
    base_dir: Option<&std::path::Path>,
    catalog: &dyn MetadataCatalog,
    mode: validate::ValidationMode,
    source_name: Option<&str>,
) -> (ProjectDefinition, Vec<Diagnostic>) {
    let (project, mut diagnostics) =
        compile_and_enrich(source, project_id, base_dir, catalog, source_name);
    // Validate the source's StateMachine blocks alongside the graph. SM blocks
    // are not lowered into `project` here (lowering is the build/runtime step,
    // P6d); validation runs the four pure SM checks on the extracted defs and
    // merges their diagnostics, so a malformed SM is never silently accepted.
    let sms = weft_compiler::extract_state_machines(source);
    diagnostics.extend(validate::validate_with_mode(&project, &sms, catalog, mode));
    (project, diagnostics)
}

/// Compile + strict enrich + validate, aborting if any `Error`-severity
/// diagnostic fires. The shape the build path wants: a clean validated
/// `ProjectDefinition` or one loud error. Layered on `compile_strict`
/// so there is exactly one pipeline; this only adds "errors abort".
pub fn compile_checked(
    source: &str,
    project_id: Uuid,
    base_dir: Option<&std::path::Path>,
    catalog: &dyn MetadataCatalog,
    mode: validate::ValidationMode,
) -> CompileResult<ProjectDefinition> {
    // Build path: a real project with a named main group (no anonymous root), so
    // the source name is irrelevant; `None` falls back to `Untitled`, unused.
    let (project, diagnostics) = compile_strict(source, project_id, base_dir, catalog, mode, None);
    bail_on_errors(diagnostics)?;
    Ok(project)
}

/// Compile + strict enrich, no validation, returning the full
/// diagnostic list on failure. For callers that need the enriched
/// topology (infra-closure walk, hashing) but not the full validation
/// gate (which the build path owns) and want to surface structured
/// per-error info to the user.
pub fn compile_enriched_with_diagnostics(
    source: &str,
    project_id: Uuid,
    base_dir: Option<&std::path::Path>,
    catalog: &dyn MetadataCatalog,
) -> Result<ProjectDefinition, Vec<Diagnostic>> {
    let (project, diagnostics) = compile_and_enrich(source, project_id, base_dir, catalog, None);
    let any_errors = diagnostics
        .iter()
        .any(|d| matches!(d.severity, Severity::Error));
    if any_errors {
        Err(diagnostics)
    } else {
        Ok(project)
    }
}

/// The shared front half of every strict pipeline: lex + parse +
/// flatten, then strict enrich, collecting failures as `Error`
/// diagnostics rather than aborting. The single home for the
/// parse/enrich error-to-`Diagnostic` mapping. A parse failure yields
/// an empty project (mirrors `parse_only`) so the shape is uniform;
/// callers decide whether to abort (`bail_on_errors`) or surface.
fn compile_and_enrich(
    source: &str,
    project_id: Uuid,
    base_dir: Option<&std::path::Path>,
    catalog: &dyn MetadataCatalog,
    source_name: Option<&str>,
) -> (ProjectDefinition, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    let mut project = match weft_compiler::compile_with_mode(
        source,
        project_id,
        base_dir,
        weft_compiler::IncludeMode::Full,
        source_name,
    ) {
        Ok(p) => p,
        Err(errors) => {
            for e in errors {
                diagnostics.push(Diagnostic::at(e.span, Severity::Error, "parse", e.message));
            }
            return (empty_project(project_id), diagnostics);
        }
    };
    if let Err(e) = enrich::enrich(&mut project, catalog) {
        diagnostics.push(Diagnostic::at(Span::default(), Severity::Error, "enrich", format!("{e}")));
    }
    (project, diagnostics)
}

/// Render a diagnostic list as one `line:column message` per line.
/// Prefers Error-severity lines; if the compile failed with only
/// warnings/hints, renders those instead of an empty string that
/// would read as "failed for no stated reason". The single
/// human-facing diagnostic rendering: `bail_on_errors` here and the
/// CLI's TTY compile-failure path both call it so the two can't drift.
pub fn render_diagnostics(diagnostics: &[Diagnostic]) -> String {
    let render = |only_errors: bool| -> String {
        diagnostics
            .iter()
            .filter(|d| !only_errors || matches!(d.severity, Severity::Error))
            .map(|d| format!("{}:{} {}", d.line, d.column, d.message))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let errors = render(true);
    if errors.is_empty() { render(false) } else { errors }
}

/// Turn an `Error`-severity diagnostic set into a single loud
/// `CompileError`; `Ok(())` when only warnings (or nothing) remain.
/// One place so every aborting entry formats failures identically.
fn bail_on_errors(diagnostics: Vec<Diagnostic>) -> CompileResult<()> {
    if diagnostics.iter().any(|d| matches!(d.severity, Severity::Error)) {
        Err(error::CompileError::Validate(render_diagnostics(&diagnostics)))
    } else {
        Ok(())
    }
}

/// File-aware, in-process validation entry point. Reads `source_file`, builds the
/// project catalog from `project_root`, resolves the `@file`/`@include` base and
/// source identity exactly as `weft validate --file` does, runs the strict
/// parse→enrich→validate pipeline, and returns the typed diagnostics (already in
/// deterministic order — see `validate::validate_with_mode`). This is the pure
/// source→diagnostics oracle the eval/author harnesses call: no CLI, no JSON
/// printing, no subprocess. An `Err` is an instrument failure (mapped to `void`
/// by the harness), never a graph diagnostic.
pub fn validate_file(
    project_root: &std::path::Path,
    source_file: &std::path::Path,
) -> Result<Vec<Diagnostic>, String> {
    let source = std::fs::read_to_string(source_file)
        .map_err(|e| format!("read {}: {e}", source_file.display()))?;
    let catalog = build::build_project_catalog(project_root)
        .map_err(|e| format!("catalog: {e}"))?;
    let project_id = project_id_for(project_root);
    // Base for @file/@include = the source file's own directory (matches the CLI's
    // base_dir_for(Some(file), _), which prefers the file's dir over the root).
    let base = source_file.parent().filter(|p| !p.as_os_str().is_empty());
    let source_name = crate::source_name::derive_id(Some(source_file));
    let (_, diagnostics) = compile_strict(
        &source,
        project_id,
        base,
        &catalog,
        validate::ValidationMode::Runtime,
        Some(&source_name),
    );
    Ok(diagnostics)
}

/// The project id `compile_strict` wants. Validation diagnostics are independent
/// of the id value (it is the dispatcher's identity, not a validation input), so
/// a deterministic fallback is safe. Read it from `weft.toml` for fidelity with
/// the CLI (mirrors `weft-cli`'s `resolve_project_id` which calls
/// `Project::load` → `manifest.package.id`), falling back to the nil UUID.
fn project_id_for(project_root: &std::path::Path) -> Uuid {
    std::fs::read_to_string(project_root.join("weft.toml"))
        .ok()
        .and_then(|raw| raw.parse::<toml::Value>().ok())
        .and_then(|v| {
            v.get("package")
                .and_then(|p| p.get("id"))
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
        .and_then(|s| Uuid::parse_str(&s).ok())
        .unwrap_or_else(Uuid::nil)
}

fn empty_project(project_id: Uuid) -> ProjectDefinition {
    ProjectDefinition {
        id: project_id,
        nodes: Vec::new(),
        edges: Vec::new(),
        groups: Vec::new(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid `weft.toml`. Copied from the real e2e fixture shape
    /// (`crates/weft-e2e/fixtures/plain/weft.toml`): `[package]` with `name`
    /// and `id`. This is the canonical on-disk form `Project::load` parses.
    fn minimal_valid_weft_toml() -> &'static str {
        r#"[package]
name = "test_project"
id = "00000000-0000-0000-0000-000000000099"
version = "0.1.0"
"#
    }

    /// Metadata for a node with two required input ports (mirrors the real
    /// `catalog/logic/gate/metadata.json` shape). Used to prove the
    /// required-port-unmet diagnostic fires end-to-end.
    fn gate_metadata_json() -> &'static str {
        r##"{
  "type": "Gate",
  "label": "Gate",
  "description": "Forwards value when pass is true.",
  "category": "Flow",
  "tags": ["flow"],
  "icon": "GitBranch",
  "color": "#6366f1",
  "inputs": [
    { "name": "pass", "type": "Boolean", "required": true },
    { "name": "value", "type": "T", "required": true }
  ],
  "outputs": [
    { "name": "value", "type": "T", "required": false }
  ],
  "fields": [],
  "entry": [],
  "requires_infra": false
}"##
    }

    /// Write a minimal project to `root`: weft.toml + nodes/Gate/metadata.json.
    fn write_minimal_project(root: &std::path::Path) {
        std::fs::write(root.join("weft.toml"), minimal_valid_weft_toml()).unwrap();
        let gate_dir = root.join("nodes").join("Gate");
        std::fs::create_dir_all(&gate_dir).unwrap();
        std::fs::write(gate_dir.join("metadata.json"), gate_metadata_json()).unwrap();
    }

    #[test]
    fn validate_file_validates_a_real_project_in_process() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_minimal_project(root);

        // Gate has two required inputs (pass, value) — leaving them unwired
        // produces a required-port-unmet Error diagnostic. Also includes a
        // Debug output node so the graph is topologically valid (has an output).
        let src = root.join("main.weft");
        std::fs::write(&src, "gate = Gate\nout = Debug\nout.data = gate.value\n").unwrap();

        let diags = validate_file(root, &src).expect("validate_file should run, not error");
        assert!(
            diags.iter().any(|d| d.severity == Severity::Error),
            "expected at least one error diagnostic (required-port-unmet), got: {diags:?}"
        );
    }

    #[test]
    fn validate_file_errs_on_missing_source() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("weft.toml"), minimal_valid_weft_toml()).unwrap();
        let res = validate_file(root, &root.join("nope.weft"));
        assert!(
            res.is_err(),
            "missing source file is an instrument error (Err), not a diagnostic"
        );
    }
}
