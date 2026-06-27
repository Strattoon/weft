//! Live model providers for the Weft authoring harness.
//!
//! Gated behind `#[cfg(feature = "providers")]` — the deterministic core and
//! `MockAuthor` never depend on this module. Contains:
//!
//! - [`AsyncAuthor`] — the async mirror of the sync `Author` trait.
//! - [`OpenRouterAuthor`] — OpenRouter-compatible HTTP client for any model.
//! - [`BlockingAuthor`] — adapts any `AsyncAuthor` to the sync `Author` trait
//!   via `tokio::runtime::Runtime::block_on`, so the existing authoring loop
//!   (WA6) runs a live provider unchanged.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

// ── AsyncAuthor trait ────────────────────────────────────────────────────────

/// Async mirror of [`crate::author::Author`]. Live providers implement this;
/// [`BlockingAuthor`] adapts it to the sync trait.
#[async_trait]
pub trait AsyncAuthor: Send + Sync {
    /// Propose a `.weft` program from scratch given `context`.
    async fn propose(&self, context: &str) -> Result<String>;

    /// Repair a previous attempt given the original context, the prior
    /// `.weft` source, and the diagnostics to fix.
    async fn repair(&self, context: &str, prev: &str, diagnostics: &str) -> Result<String>;
}

// ── Boxed forwarding impl ────────────────────────────────────────────────────

#[async_trait]
impl AsyncAuthor for Box<dyn AsyncAuthor + Send + Sync> {
    async fn propose(&self, context: &str) -> Result<String> {
        (**self).propose(context).await
    }
    async fn repair(&self, context: &str, prev: &str, diagnostics: &str) -> Result<String> {
        (**self).repair(context, prev, diagnostics).await
    }
}

// ── OpenRouterAuthor ─────────────────────────────────────────────────────────

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

const SYSTEM_PROMPT: &str = "\
You are an expert Weft (.weft) program author. \
Weft is WeaveMindAI's orchestration language for composing agent workflows. \
When given a task context, you must reply with ONLY the .weft program source — \
no prose, no explanation, no markdown code fences. \
Your entire response must be valid .weft source code.";

/// OpenRouter-compatible HTTP author for any model id.
pub struct OpenRouterAuthor {
    model: String,
    /// Optional OpenRouter provider-routing order (e.g. `["Cerebras"]`), parsed
    /// from a `model@provider` spec. When set, the request pins these providers
    /// with fallbacks disabled, so e.g. `openai/gpt-oss-120b@Cerebras` actually
    /// runs on Cerebras (and errors rather than silently routing elsewhere).
    provider_order: Option<Vec<String>>,
    api_key: String,
    base_url: String,
    client: Client,
}

/// Split a `model@provider[,provider2]` spec into the bare model id and an
/// optional provider-routing list. A plain id (no `@`) yields `None`.
/// Provider names must match OpenRouter's provider slugs (e.g. `Cerebras`).
fn parse_model_spec(spec: &str) -> (String, Option<Vec<String>>) {
    match spec.split_once('@') {
        Some((model, providers)) => {
            let order: Vec<String> = providers
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            let order = if order.is_empty() { None } else { Some(order) };
            (model.trim().to_string(), order)
        }
        None => (spec.trim().to_string(), None),
    }
}

/// Default OpenRouter provider-routing for a bare model id (no `@provider`).
/// gpt-oss-120b is pinned to Cerebras so the benchmark measures the intended
/// fast provider rather than whatever OpenRouter would otherwise choose.
fn default_provider_order(model: &str) -> Option<Vec<String>> {
    if model.contains("gpt-oss-120b") {
        Some(vec!["Cerebras".to_string()])
    } else {
        None
    }
}

impl OpenRouterAuthor {
    /// Create from environment. Reads `OPENROUTER_API_KEY` first; falls back
    /// to `WORKDAY_OPENROUTER_API_KEY` if the first is unset or empty.
    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| {
                std::env::var("WORKDAY_OPENROUTER_API_KEY")
                    .ok()
                    .filter(|v| !v.is_empty())
            })
            .ok_or_else(|| {
                anyhow!(
                    "Neither OPENROUTER_API_KEY nor WORKDAY_OPENROUTER_API_KEY is set. \
                     Set one to use live model providers."
                )
            })?;

        let client = Client::builder()
            .build()
            .context("failed to build reqwest client")?;

        let (model, provider_order) = parse_model_spec(&model.into());
        // Default gpt-oss-120b to Cerebras (the speed target) when the caller
        // did not pin a provider explicitly via `model@provider`. OpenRouter
        // otherwise routes it to an arbitrary, often slow, provider.
        let provider_order = provider_order.or_else(|| default_provider_order(&model));

        Ok(Self {
            model,
            provider_order,
            api_key,
            base_url: DEFAULT_BASE_URL.to_owned(),
            client,
        })
    }

    /// Override the base URL (for testing against a compatible endpoint).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    async fn complete(&self, user_message: String) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);

        let body = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                Message { role: "system".into(), content: SYSTEM_PROMPT.into() },
                Message { role: "user".into(), content: user_message },
            ],
            // Pin the provider(s) when a `model@provider` spec was given, with
            // fallbacks off so the benchmark measures that provider (e.g.
            // Cerebras) rather than whatever OpenRouter would otherwise pick.
            provider: self.provider_order.clone().map(|order| ProviderRouting {
                order,
                allow_fallbacks: false,
            }),
        };

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("HTTP request to OpenRouter failed")?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "OpenRouter returned non-2xx status {}: {}",
                status,
                text
            ));
        }

        let resp: ChatResponse = response
            .json()
            .await
            .context("failed to parse OpenRouter chat completion response")?;

        let content = extract_content(&resp, &self.model)?;

        Ok(strip_code_fences(&content))
    }
}

#[async_trait]
impl AsyncAuthor for OpenRouterAuthor {
    async fn propose(&self, context: &str) -> Result<String> {
        self.complete(context.to_owned()).await
    }

    async fn repair(&self, context: &str, prev: &str, diagnostics: &str) -> Result<String> {
        let user_message = format!(
            "{context}\n\nPrevious attempt:\n{prev}\n\nFix these diagnostics:\n{diagnostics}\n\nReturn the corrected .weft only."
        );
        self.complete(user_message).await
    }
}

// ── JSON request/response types ───────────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderRouting>,
}

/// OpenRouter provider-routing block. `order` lists preferred provider slugs;
/// `allow_fallbacks: false` pins to them (errors if none can serve the model).
#[derive(Serialize)]
struct ProviderRouting {
    order: Vec<String>,
    allow_fallbacks: bool,
}

#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Option<AssistantMessage>,
}

#[derive(Deserialize)]
struct AssistantMessage {
    content: Option<String>,
}

// ── extract_content helper ────────────────────────────────────────────────────

/// Extract the text content from the first choice of a [`ChatResponse`].
///
/// Factored out of the async HTTP path so it can be tested without network I/O.
///
/// # Errors
/// - No choices: `"OpenRouter returned no choices (model={model})"`.
/// - `choices[0].message` is `None` (absent): `"OpenRouter returned choices[0].message = null …"`.
/// - `choices[0].message.content` is `None` (null): `"OpenRouter returned choices[0].message.content = null …"`.
fn extract_content(resp: &ChatResponse, model: &str) -> Result<String> {
    let choice = resp.choices.first().ok_or_else(|| {
        anyhow!("OpenRouter returned no choices (model={model})")
    })?;

    let message = choice.message.as_ref().ok_or_else(|| {
        anyhow!(
            "OpenRouter returned choices[0].message = null — \
             the model ({model}) returned no message object"
        )
    })?;

    message.content.clone().ok_or_else(|| {
        anyhow!(
            "OpenRouter returned choices[0].message.content = null — \
             the model ({model}) emitted no text content \
             (likely a reasoning/tool-use response with empty content)"
        )
    })
}

// ── BlockingAuthor ────────────────────────────────────────────────────────────

/// Adapts any [`AsyncAuthor`] to the sync [`crate::author::Author`] trait by
/// driving it on a `tokio::runtime::Runtime`. This lets the sync authoring loop
/// (WA6) use a live async provider unchanged.
pub struct BlockingAuthor<A: AsyncAuthor> {
    inner: A,
    runtime: tokio::runtime::Runtime,
}

impl<A: AsyncAuthor> BlockingAuthor<A> {
    /// Build a `BlockingAuthor` around any `AsyncAuthor`. Creates a
    /// multi-thread Tokio runtime internally.
    pub fn new(inner: A) -> Result<Self> {
        let runtime = tokio::runtime::Runtime::new()
            .context("failed to create tokio runtime for BlockingAuthor")?;
        Ok(Self { inner, runtime })
    }
}

impl<A: AsyncAuthor> crate::author::Author for BlockingAuthor<A> {
    fn propose(&self, context: &str) -> Result<String> {
        self.runtime.block_on(self.inner.propose(context))
    }

    fn repair(&self, context: &str, prev: &str, diagnostics: &str) -> Result<String> {
        self.runtime.block_on(self.inner.repair(context, prev, diagnostics))
    }
}

// ── Helper: strip markdown code fences ───────────────────────────────────────

/// Strip leading/trailing markdown code fences from model output.
///
/// Models frequently wrap code in ` ```weft ... ``` ` or ` ``` ... ``` `.
/// This function removes the opening fence line (with any optional language
/// tag) and the closing fence line, returning only the inner content.
pub fn strip_code_fences(s: &str) -> String {
    let trimmed = s.trim();
    let lines: Vec<&str> = trimmed.lines().collect();

    if lines.len() < 2 {
        return trimmed.to_owned();
    }

    let first = lines[0].trim();
    let last = lines[lines.len() - 1].trim();

    let opens_fence = first.starts_with("```");
    let closes_fence = last == "```";

    if opens_fence && closes_fence {
        // Drop first and last lines; re-join the rest.
        lines[1..lines.len() - 1].join("\n")
    } else {
        trimmed.to_owned()
    }
}

// ── Unit tests (pure functions only — no network) ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        default_provider_order, extract_content, parse_model_spec, strip_code_fences, ChatResponse,
    };

    #[test]
    fn default_provider_order_pins_gpt_oss_to_cerebras() {
        assert_eq!(
            default_provider_order("openai/gpt-oss-120b"),
            Some(vec!["Cerebras".to_string()])
        );
    }

    #[test]
    fn default_provider_order_is_none_for_other_models() {
        assert_eq!(default_provider_order("anthropic/claude-haiku-4.5"), None);
    }

    // ── parse_model_spec tests ────────────────────────────────────────────

    #[test]
    fn parse_model_spec_plain_has_no_provider() {
        assert_eq!(
            parse_model_spec("openai/gpt-4o-mini"),
            ("openai/gpt-4o-mini".to_string(), None)
        );
    }

    #[test]
    fn parse_model_spec_single_provider() {
        let (model, order) = parse_model_spec("openai/gpt-oss-120b@Cerebras");
        assert_eq!(model, "openai/gpt-oss-120b");
        assert_eq!(order, Some(vec!["Cerebras".to_string()]));
    }

    #[test]
    fn parse_model_spec_multi_provider_and_trims() {
        let (model, order) = parse_model_spec("x/y @ Cerebras, Groq");
        assert_eq!(model, "x/y");
        assert_eq!(order, Some(vec!["Cerebras".to_string(), "Groq".to_string()]));
    }

    #[test]
    fn parse_model_spec_empty_provider_is_none() {
        let (model, order) = parse_model_spec("x/y@");
        assert_eq!(model, "x/y");
        assert_eq!(order, None);
    }

    // ── extract_content tests (pure / no network) ─────────────────────────

    #[test]
    fn extract_content_ok() {
        let resp: ChatResponse = serde_json::from_str(
            r#"{"choices":[{"message":{"content":"x = Text {}"}}]}"#,
        )
        .unwrap();
        assert_eq!(extract_content(&resp, "test-model").unwrap(), "x = Text {}");
    }

    #[test]
    fn extract_content_errs_on_null_content() {
        let resp: ChatResponse = serde_json::from_str(
            r#"{"choices":[{"message":{"content":null}}]}"#,
        )
        .unwrap();
        let err = extract_content(&resp, "cerebras-120b").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("content"), "expected 'content' in error: {msg}");
        assert!(msg.contains("null"), "expected 'null' in error: {msg}");
    }

    #[test]
    fn extract_content_errs_on_no_choices() {
        let resp: ChatResponse = serde_json::from_str(r#"{"choices":[]}"#).unwrap();
        let err = extract_content(&resp, "some-model").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no choices"), "expected 'no choices' in error: {msg}");
    }

    #[test]
    fn strip_code_fences_plain_weft_tag() {
        let input = "```weft\nlet x = 1;\n```";
        assert_eq!(strip_code_fences(input), "let x = 1;");
    }

    #[test]
    fn strip_code_fences_no_tag() {
        let input = "```\nlet x = 1;\n```";
        assert_eq!(strip_code_fences(input), "let x = 1;");
    }

    #[test]
    fn strip_code_fences_multiline() {
        let input = "```weft\nfoo\nbar\nbaz\n```";
        assert_eq!(strip_code_fences(input), "foo\nbar\nbaz");
    }

    #[test]
    fn strip_code_fences_no_fences() {
        let input = "let x = 1;";
        assert_eq!(strip_code_fences(input), "let x = 1;");
    }

    #[test]
    fn strip_code_fences_preserves_inner_backticks() {
        let input = "```weft\nlet x = `hello`;\n```";
        assert_eq!(strip_code_fences(input), "let x = `hello`;");
    }

    #[test]
    fn strip_code_fences_leading_trailing_whitespace() {
        let input = "  ```weft\nlet x = 1;\n```  ";
        assert_eq!(strip_code_fences(input), "let x = 1;");
    }

    #[test]
    fn strip_code_fences_empty_body() {
        let input = "```\n```";
        assert_eq!(strip_code_fences(input), "");
    }

    #[test]
    fn strip_code_fences_single_line_no_strip() {
        // A single line that starts with ``` but has no closing fence — don't strip
        let input = "```weft";
        assert_eq!(strip_code_fences(input), "```weft");
    }
}
