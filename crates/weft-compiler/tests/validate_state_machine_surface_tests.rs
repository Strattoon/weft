//! The invocable SM-validation surface: `weft validate` (via `compile_strict`)
//! must surface the four StateMachine diagnostics from a stdin source, while a
//! source with no StateMachine validates exactly as before. These tests drive
//! `compile_strict` directly (the single pipeline `weft validate`/`validate_file`
//! funnel through), against the real stdlib catalog.

use weft_catalog::{stdlib_root, FsCatalog};
use weft_compiler::compile_strict;
use weft_compiler::validate::ValidationMode;
use weft_compiler::{Diagnostic, Severity};

fn catalog() -> FsCatalog {
    FsCatalog::discover(&stdlib_root()).expect("stdlib catalog")
}

fn validate_source(source: &str) -> Vec<Diagnostic> {
    let (_project, diags) = compile_strict(
        source,
        uuid::Uuid::new_v4(),
        None,
        &catalog(),
        ValidationMode::Runtime,
        None,
    );
    diags
}

fn codes(diags: &[Diagnostic]) -> Vec<&str> {
    diags.iter().filter_map(|d| d.code.as_deref()).collect()
}

fn errors(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
    diags.iter().filter(|d| d.severity == Severity::Error).collect()
}

/// A valid StateMachine-only source: Open --Close--> Closed (terminal). Every
/// state reachable, the only non-terminal state has an outgoing edge, no
/// duplicate (from,event), no authority inversion. The runnable spine is the SM
/// itself, so there must be NO error (in particular, no `no-output-node`).
#[test]
fn valid_state_machine_yields_no_errors() {
    let source = r#"
StateMachine Simple {
  initial: Open
  terminal: [ Closed ]
  max_iters: 10
  transitions: [
    (Open, Close) -> Closed owner=script guard=always
  ]
}
"#;
    let diags = validate_source(source);
    assert!(
        errors(&diags).is_empty(),
        "a valid SM-only source must yield zero error diagnostics, got: {diags:?}"
    );
}

/// Two transitions sharing (from, event) are nondeterministic.
#[test]
fn malformed_sm_surfaces_nondeterministic() {
    let source = r#"
StateMachine M {
  initial: Open
  terminal: [ Closed ]
  max_iters: 10
  transitions: [
    (Open, Close) -> Closed owner=script guard=always
    (Open, Close) -> Open owner=script guard=always
  ]
}
"#;
    let diags = validate_source(source);
    assert!(
        codes(&diags).contains(&"sm-nondeterministic"),
        "expected sm-nondeterministic, got: {diags:?}"
    );
}

/// A Model-owned transition INTO a declared authority state is an inversion:
/// only Script/Human/Gateway may dispose an authority state.
#[test]
fn malformed_sm_surfaces_authority_inversion() {
    let source = r#"
StateMachine M {
  initial: A
  terminal: [ Done ]
  authority: [ Secured ]
  max_iters: 10
  transitions: [
    (A, Go) -> Secured owner=model guard=always
    (Secured, Fin) -> Done owner=script guard=always
  ]
}
"#;
    let diags = validate_source(source);
    assert!(
        codes(&diags).contains(&"sm-authority-inversion"),
        "expected sm-authority-inversion, got: {diags:?}"
    );
}

/// A state that is neither initial nor the target of any transition is
/// unreachable.
#[test]
fn malformed_sm_surfaces_unreachable_state() {
    let source = r#"
StateMachine M {
  initial: Open
  terminal: [ Closed, Ghost ]
  max_iters: 10
  transitions: [
    (Open, Close) -> Closed owner=script guard=always
  ]
}
"#;
    let diags = validate_source(source);
    assert!(
        codes(&diags).contains(&"sm-unreachable-state"),
        "expected sm-unreachable-state, got: {diags:?}"
    );
}

/// A non-terminal state with no outgoing transition is a dead end.
#[test]
fn malformed_sm_surfaces_no_terminal() {
    let source = r#"
StateMachine M {
  initial: Open
  terminal: [ Closed ]
  max_iters: 10
  transitions: [
    (Open, Go) -> Stuck owner=script guard=always
    (Open, Close) -> Closed owner=script guard=always
  ]
}
"#;
    let diags = validate_source(source);
    assert!(
        codes(&diags).contains(&"sm-no-terminal"),
        "expected sm-no-terminal, got: {diags:?}"
    );
}

/// Backward-compat guard: the `no-output-node` suppression is gated on a
/// StateMachine being present. A NON-SM source with no output node must STILL
/// be flagged exactly as before, and must never carry an `sm-*` code.
#[test]
fn non_sm_source_still_flags_missing_output() {
    let source = "x = Const\n";
    let diags = validate_source(source);
    assert!(
        codes(&diags).contains(&"no-output-node"),
        "a non-SM source with no output must still flag no-output-node, got: {diags:?}"
    );
    assert!(
        !codes(&diags).iter().any(|c| c.starts_with("sm-")),
        "a non-SM source must never carry an sm-* diagnostic, got: {diags:?}"
    );
}

/// Diagnostics are deterministic: validating the same source twice yields an
/// identical, identically-ordered list (SM diagnostics sorted in among graph
/// diagnostics by the single stable sort).
#[test]
fn sm_diagnostics_are_deterministic() {
    let source = r#"
StateMachine M {
  initial: Open
  terminal: [ Closed, Ghost ]
  max_iters: 10
  transitions: [
    (Open, Close) -> Closed owner=script guard=always
    (Open, Close) -> Open owner=script guard=always
  ]
}
"#;
    let first = validate_source(source);
    let second = validate_source(source);
    assert_eq!(first, second, "validate must be deterministic across runs");
}
