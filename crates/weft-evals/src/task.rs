use serde::{Deserialize, Serialize};

/// What a CORRECT candidate's oracle output looks like. The harness verdict is
/// DERIVED, not stored: actual matches expectation ⇒ Green (candidate did the
/// task); any mismatch ⇒ Red; instrument failure ⇒ Void. "Red" means the
/// candidate failed the task, never "the oracle emitted a diagnostic."
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expectation {
    /// A correct candidate produces zero error-severity diagnostics.
    #[serde(default)]
    pub diagnostics_empty: bool,
    /// Codes a correct candidate MUST produce (negative fixture: "author a graph
    /// that correctly trips check X" — present ⇒ Green, missing ⇒ Red).
    #[serde(default)]
    pub must_contain_codes: Vec<String>,
    /// Codes a correct candidate MUST NOT produce.
    #[serde(default)]
    pub must_not_contain_codes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeftEvalTask {
    pub schema_version: String,
    pub task_id: String,
    pub family: String,
    /// Path to the frozen fixture project, relative to the task file's dir.
    pub fixture_dir: String,
    /// File within the fixture the candidate provides/overwrites.
    pub candidate_file: String,
    pub allowed_files: Vec<String>,
    #[serde(default)]
    pub forbidden_files: Vec<String>,
    pub expected: Expectation,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_task() {
        let json = r#"{
            "schema_version": "weft_eval_task_v0",
            "task_id": "validation_required_ports_001",
            "family": "validation_required_ports",
            "fixture_dir": "project",
            "candidate_file": "main.weft",
            "allowed_files": ["main.weft"],
            "forbidden_files": [],
            "expected": { "diagnostics_empty": true }
        }"#;
        let task: WeftEvalTask = serde_json::from_str(json).unwrap();
        assert_eq!(task.schema_version, "weft_eval_task_v0");
        assert!(task.expected.diagnostics_empty);
    }

    #[test]
    fn rejects_unknown_fields() {
        let json = r#"{
            "schema_version": "weft_eval_task_v0",
            "task_id": "t", "family": "f", "fixture_dir": "p",
            "candidate_file": "main.weft", "allowed_files": [],
            "expected": { "diagnostics_empty": true },
            "bogus": 1
        }"#;
        assert!(serde_json::from_str::<WeftEvalTask>(json).is_err());
    }
}
