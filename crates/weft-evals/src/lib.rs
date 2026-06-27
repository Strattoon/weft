//! Deterministic, offline model-authoring eval harness for Weft.
//! Grades a candidate `.weft` artifact green/red/void by calling the
//! Weft compiler's `validate()` in-process against a frozen fixture.

pub mod task;
pub mod result;
pub mod runner;
pub mod score;
pub mod report;
