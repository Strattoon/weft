use anyhow::Result;

/// A model that authors/repairs .weft. **Sync** — this is the core + test trait
/// the deterministic loop (WA6) drives. Live async providers implement
/// `AsyncAuthor` (WA4b) and are adapted to this trait via `BlockingAuthor`, so
/// the sync loop never changes. Provider-agnostic: Claude, gpt-5.4-mini,
/// MiMo v2.5-Pro, Cerebras-120B, an 8B — all behind a provider impl. The
/// deterministic core never names a provider; only `providers`-gated impls do.
pub trait Author {
    fn propose(&self, context: &str) -> Result<String>;
    fn repair(&self, context: &str, prev: &str, diagnostics: &str) -> Result<String>;
}

/// Deterministic test double: returns scripted responses in order.
pub struct MockAuthor {
    pub scripted: std::sync::Mutex<std::collections::VecDeque<String>>,
}
impl MockAuthor {
    pub fn new(responses: Vec<String>) -> Self {
        Self { scripted: std::sync::Mutex::new(responses.into()) }
    }
    fn next(&self) -> Result<String> {
        self.scripted.lock().unwrap().pop_front().ok_or_else(|| anyhow::anyhow!("mock exhausted"))
    }
}
impl Author for MockAuthor {
    fn propose(&self, _ctx: &str) -> Result<String> { self.next() }
    fn repair(&self, _ctx: &str, _prev: &str, _diags: &str) -> Result<String> { self.next() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mock_returns_scripted_in_order() {
        let a = MockAuthor::new(vec!["v1".into(), "v2".into()]);
        assert_eq!(a.propose("c").unwrap(), "v1");
        assert_eq!(a.repair("c", "v1", "d").unwrap(), "v2");
    }
}
