use std::path::Path;

use weft_core::node::Diagnostic;

use crate::result::{Verdict, WeftEvalResult};
use crate::score::score;
use crate::task::WeftEvalTask;

/// Run one task: assemble the fixture into a temp dir, apply the candidate over
/// `candidate_file`, run the injected IN-PROCESS validator, score the typed
/// diagnostics. Any instrument failure (unconfined/disallowed path, copy error,
/// fixture-assembly error, validator error) is `Void`, never `Red`.
///
/// `validate(project_root, source_file)` returns diagnostics or an instrument
/// error. Production wires `weft_compiler::validate_file` (see
/// `run_task_in_process`); tests inject a fake — no subprocess, no shell.
pub fn run_task(
    task: &WeftEvalTask,
    task_dir: &Path,
    candidate_path: &Path,
    validate: &dyn Fn(&Path, &Path) -> Result<Vec<Diagnostic>, String>,
) -> WeftEvalResult {
    let mut result =
        WeftEvalResult::new(&task.task_id, Verdict::Void, "uninitialized", "weft_compiler::validate_file");

    // Guard: candidate_file must be an allowed, CONFINED relative path.
    // v0 candidate artifact kind = SINGLE full-file replacement only.
    if !task.allowed_files.iter().any(|f| f == &task.candidate_file) {
        result.reason = format!("candidate_file {} not in allowed_files", task.candidate_file);
        return result;
    }
    if let Err(e) = confined_relative_path(&task.candidate_file) {
        result.reason = format!("candidate_file rejected: {e}");
        return result;
    }

    // Assemble: copy the frozen fixture into a temp workdir (fixture stays immutable).
    let work = match tempfile::tempdir() {
        Ok(w) => w,
        Err(e) => { result.reason = format!("tempdir: {e}"); return result; }
    };
    if let Err(e) = copy_dir(&task_dir.join(&task.fixture_dir), work.path()) {
        result.reason = format!("assemble fixture: {e}");
        return result;
    }

    // Apply candidate over candidate_file.
    let dest = work.path().join(&task.candidate_file);
    if let Err(e) = std::fs::copy(candidate_path, &dest) {
        result.reason = format!("apply candidate: {e}");
        return result;
    }

    // Validate IN-PROCESS. No subprocess: a bounded pass over a finite graph,
    // so there is nothing to time out. An Err is an instrument failure => Void.
    let diagnostics = match validate(work.path(), &dest) {
        Ok(d) => d,
        Err(e) => { result.reason = format!("validate error: {e}"); return result; }
    };
    // Record typed diagnostics as JSON provenance (Diagnostic: Serialize).
    result.diagnostics = serde_json::to_value(&diagnostics).unwrap_or(serde_json::Value::Null);

    let (verdict, reason) = score(&task.expected, &diagnostics);
    result.verdict = verdict;
    result.reason = reason;
    result
}

/// Convenience: run a task with the real in-process compiler validator.
pub fn run_task_in_process(task: &WeftEvalTask, task_dir: &Path, candidate_path: &Path) -> WeftEvalResult {
    // File-aware: validate_file uses the path for source identity + @file/@include base.
    let validate = |root: &Path, source_file: &Path| weft_compiler::validate_file(root, source_file);
    run_task(task, task_dir, candidate_path, &validate)
}

/// Reject anything that could escape the workdir: absolute paths, empty paths,
/// rooted paths, and any `..` component. (Symlink-escape confinement is owed
/// when directory candidates land; v0 is single-file only.)
fn confined_relative_path(p: &str) -> Result<(), String> {
    use std::path::{Component, Path};
    if p.is_empty() { return Err("empty path".into()); }
    let path = Path::new(p);
    if path.is_absolute() { return Err("absolute path".into()); }
    for c in path.components() {
        match c {
            Component::ParentDir => return Err("`..` not allowed".into()),
            Component::Prefix(_) | Component::RootDir => return Err("rooted path not allowed".into()),
            _ => {}
        }
    }
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Expectation, WeftEvalTask};
    use weft_core::node::{Diagnostic, Severity};
    use weft_core::project::Span;

    fn task() -> WeftEvalTask {
        WeftEvalTask {
            schema_version: "weft_eval_task_v0".into(),
            task_id: "t1".into(),
            family: "f".into(),
            fixture_dir: "project".into(),
            candidate_file: "main.weft".into(),
            allowed_files: vec!["main.weft".into()],
            forbidden_files: vec![],
            expected: Expectation { diagnostics_empty: true, must_contain_codes: vec![], must_not_contain_codes: vec![] },
        }
    }

    fn scratch_with_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let scratch = tempfile::tempdir().unwrap();
        let proj = scratch.path().join("project");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("main.weft"), "x = Text {}").unwrap();
        let cand = scratch.path().join("candidate.weft");
        std::fs::write(&cand, "y = Text {}").unwrap();
        (scratch, cand)
    }

    #[test]
    fn green_when_validator_reports_no_errors() {
        let (scratch, cand) = scratch_with_fixture();
        let validate = |_: &Path, _: &Path| Ok(Vec::<Diagnostic>::new());
        let res = run_task(&task(), scratch.path(), &cand, &validate);
        assert_eq!(res.verdict, Verdict::Green, "reason: {}", res.reason);
    }

    #[test]
    fn red_when_unexpected_error() {
        let (scratch, cand) = scratch_with_fixture();
        let validate = |_: &Path, _: &Path| Ok(vec![Diagnostic::at(Span::default(), Severity::Error, "E", "boom")]);
        let res = run_task(&task(), scratch.path(), &cand, &validate);
        assert_eq!(res.verdict, Verdict::Red);
    }

    #[test]
    fn void_when_validator_errors() {
        let (scratch, cand) = scratch_with_fixture();
        let validate = |_: &Path, _: &Path| Err("catalog missing".to_string());
        let res = run_task(&task(), scratch.path(), &cand, &validate);
        assert_eq!(res.verdict, Verdict::Void);
    }

    #[test]
    fn rejects_unconfined_candidate_file() {
        assert!(confined_relative_path("../escape.weft").is_err());
        assert!(confined_relative_path("/etc/passwd").is_err());
        assert!(confined_relative_path("").is_err());
        assert!(confined_relative_path("main.weft").is_ok());
        assert!(confined_relative_path("sub/main.weft").is_ok());
    }
}
