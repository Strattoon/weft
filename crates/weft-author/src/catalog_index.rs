use weft_core::node::MetadataCatalog;

/// Token-overlap relevance index over node description + tags + label + type.
/// Deterministic (no model, no network): tokenize the query, score each node by
/// overlap with its searchable text, return the top-k node types. Ties break by
/// node_type (stable, deterministic order).
pub struct NodeIndex {
    entries: Vec<(String, Vec<String>)>, // (node_type, lowercased tokens)
}

impl NodeIndex {
    pub fn build(catalog: &dyn MetadataCatalog) -> Self {
        let entries = catalog
            .all()
            .into_iter()
            .map(|m| {
                let mut text = format!(
                    "{} {} {} {}",
                    m.node_type, m.label, m.description, m.category
                );
                for t in &m.tags {
                    text.push(' ');
                    text.push_str(t);
                }
                (m.node_type.clone(), tokenize(&text))
            })
            .collect();
        Self { entries }
    }

    pub fn select(&self, query: &str, k: usize) -> Vec<String> {
        let q = tokenize(query);
        let mut scored: Vec<(usize, &str)> = self
            .entries
            .iter()
            .map(|(ty, toks)| {
                let score = toks.iter().filter(|t| q.contains(*t)).count();
                (score, ty.as_str())
            })
            .collect();
        // Highest score first; ties by node_type for determinism. Drop zero-score.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        scored
            .into_iter()
            .filter(|(s, _)| *s > 0)
            .take(k)
            .map(|(_, ty)| ty.to_string())
            .collect()
    }
}

fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 2)
        .map(|t| t.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{node, FakeCat};

    #[test]
    fn selects_relevant_node_for_prompt() {
        let cat = FakeCat(vec![
            node(
                "SendEmail",
                "send an email message to a recipient",
                &["email", "notify"],
            ),
            node("Cron", "fire on a schedule", &["timer", "schedule"]),
            node("Debug", "print a value", &["debug"]),
        ]);
        let idx = NodeIndex::build(&cat);
        let picked = idx.select("email the customer when something happens", 2);
        assert!(picked.contains(&"SendEmail".to_string()));
        assert!(!picked.contains(&"Debug".to_string()));
    }

    #[test]
    fn zero_score_nodes_are_dropped() {
        let cat = FakeCat(vec![
            node("SendEmail", "send email", &["email"]),
            node("Debug", "print a value", &["debug"]),
        ]);
        let idx = NodeIndex::build(&cat);
        let picked = idx.select("totally unrelated query xyz", 10);
        assert!(picked.is_empty());
    }

    #[test]
    fn ties_broken_by_node_type_alphabetically() {
        let cat = FakeCat(vec![
            node("ZNode", "send email notify", &[]),
            node("ANode", "send email notify", &[]),
        ]);
        let idx = NodeIndex::build(&cat);
        let picked = idx.select("send email notify", 2);
        assert_eq!(picked[0], "ANode");
        assert_eq!(picked[1], "ZNode");
    }

    #[test]
    fn capped_at_k() {
        let cat = FakeCat(vec![
            node("A", "email send notify", &[]),
            node("B", "email send notify", &[]),
            node("C", "email send notify", &[]),
        ]);
        let idx = NodeIndex::build(&cat);
        let picked = idx.select("email send notify", 2);
        assert_eq!(picked.len(), 2);
    }
}
