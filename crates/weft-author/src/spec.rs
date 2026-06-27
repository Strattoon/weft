use serde::{Deserialize, Serialize};

/// The human-confirmed prose spec — Workday's freeze artifact, for Weft. The
/// intent layer produces it; the human approves it (readback); the authoring
/// layer compiles it. Intent correctness is closed HERE, not by the compiler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuthoringSpec {
    pub goal: String,
    pub steps: Vec<String>,
    pub selected_nodes: Vec<String>,
    pub io: String,
}

impl AuthoringSpec {
    pub fn to_markdown(&self) -> String {
        let mut s = format!("**Goal:** {}\n\n**Steps:**\n", self.goal);
        for (i, step) in self.steps.iter().enumerate() { s.push_str(&format!("{}. {step}\n", i + 1)); }
        s.push_str(&format!("\n**Nodes:** {}\n\n**I/O:** {}\n", self.selected_nodes.join(", "), self.io));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spec_renders_readable_markdown_for_readback() {
        let spec = AuthoringSpec {
            goal: "email me when a form is submitted".into(),
            steps: vec!["form trigger".into(), "send email".into()],
            selected_nodes: vec!["HumanTrigger".into(), "SendEmail".into()],
            io: "in: form fields; out: email".into(),
        };
        let md = spec.to_markdown();
        assert!(md.contains("**Goal:**"));
        assert!(md.contains("1. form trigger"));
    }
}
