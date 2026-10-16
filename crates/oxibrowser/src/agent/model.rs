//! Model providers for the agent loop (execution design M3).
//!
//! The loop is model-agnostic: anything that can turn a chat transcript
//! into assistant text drives it. `OpenAiCompatible` covers OpenAI, Anthropic
//! compat gateways, vLLM, Ollama's OpenAI endpoint, z.ai, etc. via
//! `--base-url`. Tests use [`ScriptedProvider`] (responses from a file or
//! inline vec) so loop behavior is deterministic and offline.
//!
//! Configuration (env, checked at startup):
//! - `OXIBROWSER_LLM_BASE_URL` — e.g. `https://api.openai.com/v1`
//!   (default `http://127.0.0.1:11434/v1` — a local endpoint).
//! - `OXIBROWSER_LLM_API_KEY` — sent as `Authorization: Bearer …` when set.
//! - `OXIBROWSER_LLM_MODEL` — model id (default `gpt-4o-mini`).
//! - `OXIBROWSER_LLM_SCRIPT` — JSONL file, one assistant reply per line;
//!   overrides the HTTP provider entirely (deterministic runs/tests).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// One chat message in the loop transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

/// A completion request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    /// Sampling temperature.
    pub temperature: f32,
    /// Max tokens for the reply.
    pub max_tokens: u32,
}

/// A completion result (text only — the loop speaks plain text, no tools).
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub text: String,
    /// Reported prompt+completion tokens, when the provider sends them.
    pub usage_tokens: Option<u64>,
}

/// What the loop needs from a model.
#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, String>;
}

/// OpenAI-compatible `/chat/completions` client (blocking — called from a
/// dedicated thread via [`ModelHandle`]; reqwest::blocking keeps the loop
/// code free of async plumbing).
pub struct OpenAiCompatible {
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl OpenAiCompatible {
    pub fn from_env() -> Self {
        Self {
            base_url: std::env::var("OXIBROWSER_LLM_BASE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:11434/v1".into()),
            api_key: std::env::var("OXIBROWSER_LLM_API_KEY").ok(),
            model: std::env::var("OXIBROWSER_LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into()),
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for OpenAiCompatible {
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())?;
        let mut request = client
            .post(format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .json(&json!({
                "model": self.model,
                "messages": req.messages,
                "temperature": req.temperature,
                "max_tokens": req.max_tokens,
            }));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let resp = request
            .send()
            .await
            .map_err(|e| format!("model request failed: {e}"))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("model response not JSON ({status}): {e}"))?;
        if !status.is_success() {
            return Err(format!("model error {status}: {body}"));
        }
        let text = body["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| format!("model response missing content: {body}"))?
            .to_string();
        let usage_tokens = body["usage"]["total_tokens"].as_u64();
        Ok(ChatResponse { text, usage_tokens })
    }
}

/// Deterministic provider for tests and reproducible runs: replies in
/// order, repeats the last reply when exhausted, and records every request
/// for assertions.
pub struct ScriptedProvider {
    pub replies: Vec<String>,
    pub calls: std::sync::Mutex<Vec<Vec<ChatMessage>>>,
}

impl ScriptedProvider {
    pub fn new(replies: Vec<String>) -> Self {
        Self {
            replies,
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// From a JSONL file (one assistant reply per line) — the
    /// `OXIBROWSER_LLM_SCRIPT` hook.
    pub fn from_file(path: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("script file: {e}"))?;
        let replies: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("script lines must be JSON strings: {e}"))?
            .into_iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect();
        Ok(Self::new(replies))
    }
}

#[async_trait::async_trait]
impl ModelProvider for ScriptedProvider {
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, String> {
        let n = self.calls.lock().map_err(|_| "poisoned")?.len();
        self.calls
            .lock()
            .map_err(|_| "poisoned")?
            .push(req.messages.clone());
        let reply = self
            .replies
            .get(n)
            .or_else(|| self.replies.last())
            .cloned()
            .unwrap_or_default();
        Ok(ChatResponse {
            text: reply,
            usage_tokens: None,
        })
    }
}

/// Build the provider from the environment: scripted file wins, then the
/// OpenAI-compatible HTTP endpoint.
pub fn provider_from_env() -> Result<Box<dyn ModelProvider>, String> {
    if let Ok(path) = std::env::var("OXIBROWSER_LLM_SCRIPT") {
        return Ok(Box::new(ScriptedProvider::from_file(&path)?));
    }
    Ok(Box::new(OpenAiCompatible::from_env()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokio_block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[tokio::test]
    async fn scripted_provider_replays_in_order_and_repeats_last() {
        let p = ScriptedProvider::new(vec!["one".into(), "two".into()]);
        let req = ChatRequest {
            messages: vec![ChatMessage::user("hi")],
            temperature: 0.0,
            max_tokens: 16,
        };
        assert_eq!(tokio_block(p.complete(&req)).unwrap().text, "one");
        assert_eq!(tokio_block(p.complete(&req)).unwrap().text, "two");
        assert_eq!(tokio_block(p.complete(&req)).unwrap().text, "two");
        assert_eq!(p.calls.lock().unwrap().len(), 3);
    }
}
