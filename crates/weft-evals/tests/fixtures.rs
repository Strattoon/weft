use std::path::Path;

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
fn missing_required_candidate_is_red() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/validation_required_ports/001");
    let task: weft_evals::task::WeftEvalTask =
        serde_json::from_str(&std::fs::read_to_string(base.join("task.json")).unwrap()).unwrap();
    let candidate = base.join("candidates/missing_required.weft");
    let res = weft_evals::runner::run_task_in_process(&task, &base, &candidate);
    assert_eq!(res.verdict, weft_evals::result::Verdict::Red, "reason: {}", res.reason);
}
