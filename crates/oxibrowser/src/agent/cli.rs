//! `oxibrowser agent` CLI (execution design M3).

use clap::Subcommand;

use super::loop_::{ConsentHandles, LoopConfig, LoopOutcome, TerminalConsentPrompt};
use super::model::provider_from_env;

/// `agent` subcommands.
#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Run a task autonomously: observe → model → REPL program loop.
    Run {
        /// The task, in natural language.
        task: String,
        /// Bind an account (credential-mode context + envelope restore +
        /// `oxi.fillCredential` + consent surfaces).
        #[arg(long, value_name = "ID")]
        account: Option<String>,
        /// Allow navigation to private/internal IP ranges (local fixtures).
        #[arg(long)]
        allow_private_ips: bool,
        /// Correlation tag stamped on audit events.
        #[arg(long = "ref", value_name = "TAG")]
        ref_tag: Option<String>,
        /// Maximum model steps.
        #[arg(long, default_value_t = 25)]
        max_steps: usize,
        /// Approximate token budget for the whole run.
        #[arg(long, default_value_t = 200_000)]
        budget_tokens: u64,
    },
}

/// Entry point for `oxibrowser agent …`.
pub async fn run_agent(command: AgentCommand) -> i32 {
    let AgentCommand::Run {
        task,
        account,
        allow_private_ips,
        ref_tag,
        max_steps,
        budget_tokens,
    } = command;

    let provider = match provider_from_env() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("model provider: {e}");
            return 1;
        }
    };

    // Same assembly as `oxibrowser repl` — the loop drives the same world.
    let setup =
        super::repl::WorldSetup::new(account.as_deref(), allow_private_ips, ref_tag.as_deref())
            .await;
    let mut setup = match setup {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    let consent = ConsentHandles {
        store: setup
            .account_bound
            .then(|| {
                oxibrowser_credentials::ConsentStore::open_default()
                    .map(std::sync::Arc::new)
                    .ok()
            })
            .flatten(),
        surface: setup.host_surface(),
        context_id: setup.context_id(),
    };

    let cfg = LoopConfig {
        task,
        max_steps,
        budget_tokens,
        ..LoopConfig::default()
    };
    let outcome = super::loop_::run(
        &cfg,
        provider.as_ref(),
        &setup.world,
        &TerminalConsentPrompt,
        consent,
    )
    .await;

    let result = match outcome {
        LoopOutcome::Done {
            steps,
            result,
            model_calls,
            approx_tokens,
        } => serde_json::json!({
            "ok": true,
            "result": result,
            "steps": steps,
            "model_calls": model_calls,
            "approx_tokens": approx_tokens,
        }),
        LoopOutcome::StepsExhausted { steps, last_error } => serde_json::json!({
            "ok": false,
            "error": "steps exhausted",
            "steps": steps,
            "last_error": last_error,
        }),
        LoopOutcome::BudgetExhausted { steps } => serde_json::json!({
            "ok": false,
            "error": "token budget exhausted",
            "steps": steps,
        }),
        LoopOutcome::ModelError { steps, error } => serde_json::json!({
            "ok": false,
            "error": format!("model error: {error}"),
            "steps": steps,
        }),
    };
    let _ = setup.shutdown().await;
    println!("{}", serde_json::to_string(&result).unwrap_or_default());
    if result["ok"].as_bool().unwrap_or(false) {
        0
    } else {
        2
    }
}
