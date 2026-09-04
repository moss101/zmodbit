//! Live one-agent e2e (M2.7, docs/14 main loop against docs/15 live
//! proof): the REAL OneAgentRuntime loop — compile → stream → typed
//! events → verify → complete — driven through a production provider
//! endpoint.
//!
//! Env-gated so CI never requires paid API calls:
//!   MODBIT_LIVE_OPENAI=1 + OPENAI_API_KEY, or
//!   MODBIT_LIVE_ANTHROPIC=1 + ANTHROPIC_API_KEY
//!
//! Until run with real credentials this e2e has NOT been performed; the
//! runtime is covered by stub-transport tests in src/one_agent.rs.

use modbit_core_runtime::one_agent::{AgentTask, ModelTransport, OneAgentRuntime};
use modbit_policy::PolicyKernel;
use modbit_providers::gateway::{
    parse_anthropic_sse_payload, parse_openai_sse_payload, Provider, StreamEvent,
};
use modbit_tools::ToolRegistry;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn live_enabled() -> Option<Provider> {
    if std::env::var("MODBIT_LIVE_OPENAI").is_ok() && std::env::var("OPENAI_API_KEY").is_ok() {
        Some(Provider::OpenAi)
    } else if std::env::var("MODBIT_LIVE_ANTHROPIC").is_ok()
        && std::env::var("ANTHROPIC_API_KEY").is_ok()
    {
        Some(Provider::Anthropic)
    } else {
        None
    }
}

/// Real transport: streams over HTTPS via curl and normalizes SSE frames
/// through the production parsers.
struct LiveTransport {
    provider: Provider,
    key: String,
}

impl ModelTransport for LiveTransport {
    fn stream(
        &self,
        request: &modbit_providers::gateway::ModelRequest,
    ) -> Result<Vec<StreamEvent>, String> {
        let body = serde_json::to_vec(&match self.provider {
            Provider::OpenAi => modbit_providers::gateway::openai_request_body(request),
            Provider::Anthropic => modbit_providers::gateway::anthropic_request_body(request),
        })
        .map_err(|e| e.to_string())?;

        let mut child = Command::new("curl")
            .args([
                "-sS",
                "-N",
                "--max-time",
                "90",
                "-X",
                "POST",
                &self.provider.endpoint(),
                "-H",
                &format!("Authorization: Bearer {}", self.key),
                "-H",
                "Content-Type: application/json",
                "-H",
                "anthropic-version: 2023-06-01",
                "--data-binary",
                "@-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn curl: {e}"))?;
        // take() the stdin handle: dropping it closes the pipe. With
        // --data-binary @- curl waits for stdin EOF before sending —
        // leaving it open stalls the request forever.
        {
            let mut stdin = child.stdin.take().expect("stdin");
            stdin
                .write_all(&body)
                .map_err(|e| format!("write body: {e}"))?;
        }
        let stdout = child.stdout.take().expect("stdout");
        let reader = BufReader::new(stdout);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;
            if let Some(payload) = modbit_providers::gateway::sse_data_line(&line) {
                let event = match self.provider {
                    Provider::OpenAi => parse_openai_sse_payload(&payload),
                    Provider::Anthropic => parse_anthropic_sse_payload(&payload),
                };
                if let Some(event) = event {
                    let done = matches!(event, StreamEvent::Completed { .. });
                    events.push(event);
                    if done {
                        break;
                    }
                }
            }
        }
        let _ = child.wait();
        Ok(events)
    }
}

fn task() -> AgentTask {
    AgentTask {
        task_id: format!("live-{}", uuid::Uuid::now_v7().simple()),
        objective: "In one short sentence, state that the modbit one-agent runtime loop is live."
            .into(),
        model: std::env::var("MODBIT_LIVE_MODEL").unwrap_or_else(|_| {
            if live_enabled() == Some(Provider::Anthropic) {
                "claude-3-5-haiku-20241022".into()
            } else {
                "gpt-4o-mini".into()
            }
        }),
        provider: "live".into(),
        system_policy: "Answer in at most two sentences.".into(),
        workspace_rules: String::new(),
        context_pack: String::new(),
    }
}

/// The REAL loop against the production endpoint: the turn state machine
/// must reach Completed with non-empty assembled text.
#[test]
fn live_one_agent_loop_reaches_completed() {
    let Some(provider) = live_enabled() else {
        eprintln!("skipped: no live provider credentials in env (docs/15 live proof pending)");
        return;
    };
    let key = std::env::var(provider.credential_env()).unwrap();
    let transport = LiveTransport { provider, key };

    let registry = ToolRegistry::new();
    let kernel = PolicyKernel::new(vec![]);
    let rt = OneAgentRuntime {
        transport: &transport,
        registry: &registry,
        kernel: &kernel,
        grants: &[],
        max_turns: 2,
    };

    let result = rt.run(&task()).unwrap();
    assert_eq!(
        result.final_state,
        modbit_domain::turn::TurnState::Completed,
        "live loop must complete: {result:?}"
    );
    assert_eq!(result.turns_used, 1);
    assert!(
        !result.assembled_text.trim().is_empty(),
        "assembled text must be non-empty"
    );
    assert!(result.tool_outcomes.is_empty());
}
