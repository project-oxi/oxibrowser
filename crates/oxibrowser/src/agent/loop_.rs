//! The agent loop (execution design M3): observe → model → REPL program →
//! result, with compaction, budgets, and the two consent flows.
//!
//! Protocol with the model: system prompt (Playwright deltas + rules),
//! one task message, then alternating `PROGRAM` (assistant) / `RESULT`
//! (user) turns. The model finishes by calling `task.done(result)` in a
//! program; the loop reads it from the world.
//!
//! Consent flows (review's fatal-1 split):
//! - (a) credential consent — `CONSENT_REQUIRED` errors surface the broker
//!   reason; with approval the loop appends a scoped `ConsentRecord` grant
//!   to the store the engine replays (`ConsentStore` is file-replay), then
//!   retries the same program once.
//! - (b) irreversible denial — `irreversibleActionRequiresGrant` parses the
//!   account/pattern from the message; with approval the loop marks the
//!   live context irreversible-capable (`LoginSurface::mark_irreversible` —
//!   checked at call time in this process, no context recreation needed)
//!   and retries once.

use std::sync::Arc;

use oxibrowser_cdp::account::LoginSurface;
use oxibrowser_credentials::{ConsentRecord, ConsentStore, ConsentSubject, CredentialId};
use serde_json::{Value, json};

use super::model::{ChatMessage, ChatRequest, ModelProvider};
use super::prompt;
use super::world::World;

/// Loop configuration.
#[derive(Debug, Clone)]
pub struct LoopConfig {
    pub task: String,
    /// Task id: persists the transcript under the state dir and enables
    /// `agent resume <id>` (M4). `None` = ephemeral run.
    pub task_id: Option<String>,
    pub max_steps: usize,
    pub budget_tokens: u64,
    /// Transcript chars above this trigger compaction (chars ≈ 4×tokens).
    pub compaction_chars: usize,
    /// Cap per RESULT message appended to the transcript (chars).
    pub result_cap_chars: usize,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            task: String::new(),
            task_id: None,
            max_steps: 25,
            budget_tokens: 200_000,
            compaction_chars: 80_000,
            result_cap_chars: 6_000,
        }
    }
}

/// How the loop asks a human. The terminal impl prints and reads stdin;
/// tests inject their own.
pub trait ConsentPrompt: Send + Sync {
    fn ask_credential_consent(&self, reason: &str) -> bool;
    fn ask_irreversible_grant(&self, account: &str, pattern: &str) -> bool;
}

/// Terminal consent: stderr question, `y`/`n` on stdin.
pub struct TerminalConsentPrompt;

impl ConsentPrompt for TerminalConsentPrompt {
    fn ask_credential_consent(&self, reason: &str) -> bool {
        eprintln!("┌─ credential consent requested ─────────────────────");
        eprintln!("│ {reason}");
        eprintln!("└─ approve? [y/N]");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim().eq_ignore_ascii_case("y")
    }

    fn ask_irreversible_grant(&self, account: &str, pattern: &str) -> bool {
        eprintln!("┌─ irreversible action gate ──────────────────────────");
        eprintln!("│ account {account} wants an irreversible action");
        eprintln!("│ matched pattern: {pattern:?}");
        eprintln!("│ approving allows this class of action in this context");
        eprintln!("└─ approve? [y/N]");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim().eq_ignore_ascii_case("y")
    }
}

/// Loop outcome.
#[derive(Debug)]
pub enum LoopOutcome {
    /// `task.done(result)` fired.
    Done {
        steps: usize,
        result: Value,
        model_calls: usize,
        approx_tokens: u64,
    },
    StepsExhausted {
        steps: usize,
        last_error: Option<String>,
    },
    BudgetExhausted {
        steps: usize,
    },
    ModelError {
        steps: usize,
        error: String,
    },
}

/// Consent plumbing the loop may use (all optional — plain runs have none).
#[derive(Clone, Default)]
pub struct ConsentHandles {
    pub store: Option<Arc<ConsentStore>>,
    pub surface: Option<Arc<LoginSurface>>,
    pub context_id: Option<String>,
}

/// Run the loop to a terminal outcome.
pub async fn run(
    cfg: &LoopConfig,
    provider: &dyn ModelProvider,
    world: &World,
    consent_prompt: &dyn ConsentPrompt,
    consent: ConsentHandles,
) -> LoopOutcome {
    if let Some(id) = cfg.task_id.as_deref() {
        save_task_meta(id, &cfg.task);
    }
    let mut transcript: Vec<ChatMessage> = vec![ChatMessage::system(prompt::SYSTEM_PROMPT)];
    // Resume: replay the saved turns (the task message is already in
    // there from the first run); fresh runs start from the task message.
    let mut history: Vec<ChatMessage> = match cfg.task_id.as_deref() {
        Some(id) => {
            let saved = load_history(id);
            if saved.is_empty() {
                vec![ChatMessage::user(prompt::task_message(&cfg.task))]
            } else {
                saved
            }
        }
        None => vec![ChatMessage::user(prompt::task_message(&cfg.task))],
    };
    let mut steps = 0usize;
    let mut model_calls = 0usize;
    let mut approx_tokens = prompt::prompt_tokens() as u64;
    let mut last_error: Option<String> = None;

    while steps < cfg.max_steps {
        let mut messages = transcript.clone();
        messages.extend(history.iter().cloned());
        let sent: u64 = messages.iter().map(|m| m.content.len() as u64 / 4).sum();
        if approx_tokens.saturating_add(sent) > cfg.budget_tokens {
            return LoopOutcome::BudgetExhausted { steps };
        }

        // Compaction: summarize the older history when it dwarfs the cap.
        if history.iter().map(|m| m.content.len()).sum::<usize>() > cfg.compaction_chars {
            match compact(provider, &mut history).await {
                Ok(()) => {}
                Err(e) => return LoopOutcome::ModelError { steps, error: e },
            }
        }

        let request = ChatRequest {
            messages,
            temperature: 0.0,
            max_tokens: 2048,
        };
        let reply = match provider.complete(&request).await {
            Ok(r) => r,
            Err(e) => {
                return LoopOutcome::ModelError {
                    steps: steps.max(1),
                    error: e,
                };
            }
        };
        model_calls += 1;
        let program = reply.text.trim().to_string();
        if program.is_empty() {
            last_error = Some("model returned an empty program".into());
            steps += 1;
            history.push(ChatMessage::assistant(""));
            history.push(ChatMessage::user(
                "ERROR: empty reply — respond with exactly one JavaScript program.",
            ));
            continue;
        }

        steps += 1;
        if let Some(id) = cfg.task_id.as_deref() {
            append_turn(id, "assistant", &program);
        }
        history.push(ChatMessage::assistant(program.clone()));

        // Blocking eval: parks this task thread for the program duration (same
        // tradeoff as the REPL; the runtime has other workers).
        let outcome = world.eval(&program);
        match outcome {
            Ok((value, done)) => {
                let mut result_json = serde_json::to_string(&value).unwrap_or_default();
                if result_json.len() > cfg.result_cap_chars {
                    result_json.truncate(
                        result_json
                            .char_indices()
                            .nth(cfg.result_cap_chars)
                            .map(|(i, _)| i)
                            .unwrap_or(result_json.len()),
                    );
                    result_json.push_str("…[truncated]");
                }
                if let Some(id) = cfg.task_id.as_deref() {
                    append_turn(id, "user", &format!("RESULT: {result_json}"));
                }
                history.push(ChatMessage::user(format!("RESULT: {result_json}")));
                if let Some(done) = done {
                    return LoopOutcome::Done {
                        steps,
                        result: done,
                        model_calls,
                        approx_tokens: approx_tokens + sent,
                    };
                }
                last_error = None;
            }
            Err(error) => {
                // Consent flows: approve → inject → retry the same program
                // once; otherwise the error goes back to the model verbatim.
                let handled =
                    handle_consent(&error, &program, consent_prompt, &consent, world).await;
                match handled {
                    ConsentResult::Retried(Ok((value, done))) => {
                        let result_json = serde_json::to_string(&value).unwrap_or_default();
                        history.push(ChatMessage::user(format!(
                            "RESULT (after consent): {}",
                            &result_json[..result_json.len().min(cfg.result_cap_chars)]
                        )));
                        if let Some(done) = done {
                            return LoopOutcome::Done {
                                steps,
                                result: done,
                                model_calls,
                                approx_tokens: approx_tokens + sent,
                            };
                        }
                        last_error = None;
                    }
                    ConsentResult::Retried(Err(retry_error)) => {
                        last_error = Some(retry_error.clone());
                        history.push(ChatMessage::user(format!("ERROR: {retry_error}")));
                    }
                    ConsentResult::Declined(reason) => {
                        last_error = Some(reason.clone());
                        history.push(ChatMessage::user(format!(
                            "ERROR: {reason}\n(consent was declined — do not retry the same action; report or work around it.)"
                        )));
                    }
                    ConsentResult::NotApplicable => {
                        last_error = Some(error.clone());
                        history.push(ChatMessage::user(format!("ERROR: {error}")));
                    }
                }
            }
        }
    }

    LoopOutcome::StepsExhausted { steps, last_error }
}

enum ConsentResult {
    Retried(Result<(Value, Option<Value>), String>),
    Declined(String),
    NotApplicable,
}

/// Parse + route the two consentable error classes.
async fn handle_consent(
    error: &str,
    program: &str,
    prompt: &dyn ConsentPrompt,
    consent: &ConsentHandles,
    world: &World,
) -> ConsentResult {
    // (a) credential consent: the broker reason names the credential,
    // the origin, and the action ("no active consent for X at Y (login)").
    if error.contains("CONSENT_REQUIRED") {
        let reason = error
            .split_once("ConsentRequired { reason: \"")
            .and_then(|(_, r)| r.split_once('"'))
            .map(|(r, _)| r.to_string())
            .unwrap_or_else(|| error.to_string());
        if prompt.ask_credential_consent(&reason) {
            if let (Some(store), Some((credential, origin))) =
                (consent.store.as_ref(), parse_consent_reason(&reason))
            {
                let record = ConsentRecord::new(
                    ConsentSubject::Credential {
                        credential: CredentialId(credential.into()),
                    },
                    &origin,
                    &["login"],
                    chrono::Duration::minutes(10),
                    2,
                );
                if let Err(e) = store.grant(record) {
                    return ConsentResult::Declined(format!("consent grant failed: {e}"));
                }
                return ConsentResult::Retried(world.eval(program));
            }
            return ConsentResult::Declined(
                "consent approved but the loop lacks the store/credential to grant it".into(),
            );
        }
        return ConsentResult::Declined("credential consent declined".into());
    }

    // (b) irreversible gate: the message names the pattern and account.
    if error.contains("irreversibleActionRequiresGrant") {
        let pattern = error
            .split_once("pattern '")
            .and_then(|(_, r)| r.split_once('\''))
            .map(|(p, _)| p.to_string());
        let account = error
            .split_once("account grant ")
            .and_then(|(_, r)| r.split_whitespace().next())
            .map(str::to_string);
        if let (Some(pattern), Some(account)) = (pattern, account) {
            if prompt.ask_irreversible_grant(&account, &pattern) {
                if let Some(surface) = consent.surface.as_ref() {
                    // Live capability set — checked at call time in this
                    // process, so no context recreation is needed here
                    // (the CDP path snapshots at creation; that is where
                    // "recreate the context" applies).
                    if let Some(ctx_id) = consent.context_id.as_deref() {
                        surface.mark_irreversible(ctx_id);
                        return ConsentResult::Retried(world.eval(program));
                    }
                }
                return ConsentResult::Declined(
                    "grant approved but no login surface is attached".into(),
                );
            }
            return ConsentResult::Declined("irreversible action declined by the human".into());
        }
    }

    ConsentResult::NotApplicable
}

/// `no active consent for {credential} at {origin} ({action})` →
/// (credential, origin).
fn parse_consent_reason(reason: &str) -> Option<(String, String)> {
    let rest = reason.split_once("no active consent for ")?.1;
    let (credential, rest) = rest.split_once(" at ")?;
    let (origin, _) = rest.split_once(" (")?;
    Some((credential.to_string(), origin.to_string()))
}

/// Replace all but the last two exchanges with a provider summary.
async fn compact(
    provider: &dyn ModelProvider,
    history: &mut Vec<ChatMessage>,
) -> Result<(), String> {
    let keep = 4.min(history.len()); // last two exchanges
    let split = history.len() - keep;
    if split == 0 {
        return Ok(());
    }
    let older: Vec<String> = history[..split]
        .iter()
        .map(|m| format!("{}: {}", m.role, m.content))
        .collect();
    let summary_req = ChatRequest {
        messages: vec![
            ChatMessage::system(
                "Summarize the browser-work transcript below in under 200 words. Keep: current URL, completed steps, discovered selectors/refs, open problems. Drop everything else.",
            ),
            ChatMessage::user(older.join("\n\n")),
        ],
        temperature: 0.0,
        max_tokens: 400,
    };
    let summary = provider.complete(&summary_req).await?.text;
    history.splice(
        ..split,
        std::iter::once(ChatMessage::user(format!(
            "EARLIER WORK (summarized): {summary}"
        ))),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Transcript persistence (M4): `agent resume <id>` replays the saved
// history and continues the loop from the last observation.
// ---------------------------------------------------------------------------

fn task_dir(id: &str) -> Option<std::path::PathBuf> {
    if id.is_empty() {
        return None;
    }
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let base = std::env::var_os("OXIBROWSER_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            std::path::PathBuf::from(home).join(".local/state/oxibrowser")
        });
    Some(base.join("tasks").join(safe))
}

/// Save the task metadata (for resume prompts).
fn save_task_meta(id: &str, task: &str) {
    if let Some(dir) = task_dir(id) {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join("task.json"),
            serde_json::json!({"task": task}).to_string(),
        );
    }
}

/// Load a saved task string.
pub fn load_task_meta(id: &str) -> Option<String> {
    let dir = task_dir(id)?;
    let text = std::fs::read_to_string(dir.join("task.json")).ok()?;
    serde_json::from_str::<Value>(&text).ok()?["task"]
        .as_str()
        .map(str::to_string)
}

/// Append one transcript turn.
fn append_turn(id: &str, role: &str, content: &str) {
    let Some(dir) = task_dir(id) else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("transcript.jsonl"))
    {
        use std::io::Write as _;
        let _ = writeln!(
            f,
            "{}",
            serde_json::json!({"role": role, "content": content})
        );
    }
}

/// Load a saved transcript as loop history (resume).
pub fn load_history(id: &str) -> Vec<ChatMessage> {
    let Some(dir) = task_dir(id) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(dir.join("transcript.jsonl")) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<ChatMessage>(l).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_consent_reason() {
        let (cred, origin) = parse_consent_reason(
            "no active consent for kch:main/example.test/password@pw at http://127.0.0.1:8080 (login)",
        )
        .unwrap();
        assert_eq!(cred, "kch:main/example.test/password@pw");
        assert_eq!(origin, "http://127.0.0.1:8080");
    }

    #[test]
    fn default_budgets_are_sane() {
        let cfg = LoopConfig::default();
        assert!(cfg.max_steps >= 10 && cfg.max_steps <= 50);
        assert!(cfg.result_cap_chars >= 2_000);
    }

    fn _json_shape() -> Value {
        json!({})
    }
}
