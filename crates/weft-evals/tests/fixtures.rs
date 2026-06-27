use std::path::Path;

#[test]
fn clean_candidate_passes_plane_separation() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/tagged_flow_plane_separation/001");
    let task: weft_evals::task::WeftEvalTask =
        serde_json::from_str(&std::fs::read_to_string(base.join("task.json")).unwrap()).unwrap();
    let candidate = base.join("candidates/clean.weft");
    let res = weft_evals::runner::run_task_in_process(&task, &base, &candidate);
    assert_eq!(res.verdict, weft_evals::result::Verdict::Green, "reason: {}", res.reason);
}

#[test]
fn violating_candidate_fails_plane_separation() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/tagged_flow_plane_separation/001");
    let task: weft_evals::task::WeftEvalTask =
        serde_json::from_str(&std::fs::read_to_string(base.join("task.json")).unwrap()).unwrap();
    let candidate = base.join("candidates/violating.weft");
    let res = weft_evals::runner::run_task_in_process(&task, &base, &candidate);
    assert_eq!(res.verdict, weft_evals::result::Verdict::Red, "reason: {}", res.reason);

    // Pin the Red to the tagged-flow invariant specifically. Asserting only
    // `Verdict::Red` would let an unrelated compiler error keep this fixture
    // green while it silently stops proving plane separation, so require a
    // `tagged-flow-violation` diagnostic in the serialized diagnostics.
    let has_tagged_flow = res
        .diagnostics
        .as_array()
        .map(|ds| {
            ds.iter()
                .any(|d| d.get("code").and_then(|c| c.as_str()) == Some("tagged-flow-violation"))
        })
        .unwrap_or(false);
    assert!(
        has_tagged_flow,
        "expected a tagged-flow-violation diagnostic, got: {}",
        res.diagnostics
    );
}

#[test]
fn correct_candidate_is_green() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/validation_required_ports/001");
    let task: weft_evals::task::WeftEvalTask =
        serde_json::from_str(&std::fs::read_to_string(base.join("task.json")).unwrap()).unwrap();
    let candidate = base.join("candidates/correct.weft");
    let res = weft_evals::runner::run_task_in_process(&task, &base, &candidate);
    assert_eq!(res.verdict, weft_evals::result::Verdict::Green, "reason: {}", res.reason);
}

#[test]
fn workday_nodes_legal_candidate_is_green() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workday_nodes/001");
    let task: weft_evals::task::WeftEvalTask =
        serde_json::from_str(&std::fs::read_to_string(base.join("task.json")).unwrap()).unwrap();
    let candidate = base.join("candidates/legal.weft");
    let res = weft_evals::runner::run_task_in_process(&task, &base, &candidate);
    assert_eq!(res.verdict, weft_evals::result::Verdict::Green, "reason: {}", res.reason);
}

#[test]
fn missing_required_candidate_is_red() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/validation_required_ports/001");
    let task: weft_evals::task::WeftEvalTask =
        serde_json::from_str(&std::fs::read_to_string(base.join("task.json")).unwrap()).unwrap();
    let candidate = base.join("candidates/missing_required.weft");
    let res = weft_evals::runner::run_task_in_process(&task, &base, &candidate);
    assert_eq!(res.verdict, weft_evals::result::Verdict::Red, "reason: {}", res.reason);
}
