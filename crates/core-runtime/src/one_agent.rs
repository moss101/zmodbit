//! One-agent runtime (M2.7, docs/14 § Main runtime loop — single-agent
//! durable slice): compile the prompt, invoke the model through the
//! NORMALIZED provider gateway, parse typed events, drive the turn state
//! machine, and execute any tool requests under capability policy —
//! invalid tool payloads are rejected BEFORE side effects. The model
//! transport is an injected trait: tests run against a stub; the live
//! qualification runs the real gateway (docs/15 live proof).

use modbit_domain::turn::{can_transition, TurnState};
use modbit_policy::{PolicyDecision, PolicyKernel, ToolCallRequest};
use modbit_prompt_compiler::{compile, CompilerInputs};
use modbit_providers::gateway::{ChatMessage, ModelRequest, StreamEvent, ToolCallData};
use modbit_tools::ToolRegistry;
use serde::Serialize;
use serde_json::Value;

/// How the runtime talks to a model. The production transport wraps the
/// provider gateway over HTTPS; tests inject a stub.
pub trait ModelTransport {
    fn stream(&self, request: &ModelRequest) -> Result<Vec<StreamEvent>, String>;
}

/// Live run control (Phase 2.3): the scheduler's stop/pause/steer surface
/// reaches the loop through this trait. All methods default to "no
/// control" so unit tests run uncontrolled.
pub trait RunControl: Sync + Send {
    /// StopTask fired: abort at the next boundary (or sooner from the
    /// transport); the task state is already Cancelled on the store side.
    fn is_cancelled(&self) -> bool {
        false
    }
    /// PauseTask fired: park the run at the next turn boundary.
    fn is_paused(&self) -> bool {
        false
    }
    /// SteerTask notes queued since the last turn; injected as user
    /// messages BEFORE the next invoke.
    fn take_steer_notes(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Durable-run observer (docs/13 Run/Turn/RunStep): the runtime reports
/// every state change it drives; the scheduler writes them as events into
/// the store. All methods default to no-ops so tests observe subsets.
pub trait RunObserver: Sync + Send {
    fn run_started(&self, _run_id: &str, _attempt: u32) {}
    fn turn_prepared(&self, _turn_id: &str, _ordinal: u32) {}
    fn model_invoke_started(&self, _turn_id: &str, _step_id: &str) {}
    fn model_invoke_finished(
        &self,
        _turn_id: &str,
        _step_id: &str,
        _usage: Option<modbit_providers::TokenUsage>,
    ) {
    }
    fn tool_step_started(&self, _turn_id: &str, _step_id: &str, _call_id: &str, _name: &str) {}
    fn tool_step_finished(
        &self,
        _turn_id: &str,
        _step_id: &str,
        _call_id: &str,
        _name: &str,
        _ok: bool,
    ) {
    }
    fn turn_completed(&self, _turn_id: &str) {}
    fn turn_failed(&self, _turn_id: &str, _failure_code: &str) {}
    fn run_completed(&self, _run_id: &str) {}
    fn run_failed(&self, _run_id: &str, _failure_code: &str) {}
    /// Context compaction applied before an invoke (Phase 2.2, docs/19):
    /// the model-visible projection shrank; canonical history is intact.
    fn compaction_applied(
        &self,
        _turn_id: &str,
        _epoch_id: &str,
        _affected_messages: u32,
        _reclaimed_tokens: u64,
        _manifest_digest: &str,
    ) {
    }
    /// Conversation checkpoint at a turn boundary (Phase 2.5, docs/19 §
    /// Checkpoint epochs): recovery data for resuming after a Core kill.
    fn conversation_checkpointed(
        &self,
        _run_id: &str,
        _turn_ordinal: u32,
        _conversation_json: &str,
    ) {
    }
}

/// Default input-token budget before the loop compacts the conversation.
pub const DEFAULT_MAX_INPUT_TOKENS: u64 = 32_768;

/// The task the agent is asked to run.
#[derive(Clone, Debug)]
pub struct AgentTask {
    pub task_id: String,
    pub objective: String,
    pub model: String,
    pub provider: String,
    pub system_policy: String,
    pub workspace_rules: String,
    pub context_pack: String,
    /// Per-model request settings (Phase 2.2): output budget, sampling
    /// temperature, optional reasoning/thinking effort. Defaults resolve
    /// from `modbit_providers::profiles`; env overrides land here via the
    /// scheduler config.
    pub model_settings: modbit_providers::profiles::ModelSettings,
    /// Input-token budget for the model-visible conversation (Phase 2.2).
    /// When the estimate exceeds it, the loop compacts BEFORE the invoke
    /// (oldest tool results first, then epoch summaries).
    pub max_input_tokens: u64,
}

impl Default for AgentTask {
    fn default() -> Self {
        AgentTask {
            task_id: String::new(),
            objective: String::new(),
            model: String::new(),
            provider: String::new(),
            system_policy: String::new(),
            workspace_rules: String::new(),
            context_pack: String::new(),
            model_settings: modbit_providers::profiles::ModelSettings::BASE,
            max_input_tokens: DEFAULT_MAX_INPUT_TOKENS,
        }
    }
}

/// A tool request that was rejected or denied — kept as evidence.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolOutcome {
    pub call_id: String,
    pub name: String,
    /// Executed, with the result JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Rejected/denied, with the reason (never executed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

/// The durable result of one agent run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AgentRunResult {
    pub task_id: String,
    /// The run aggregate id (docs/13 § Run) of this execution attempt.
    pub run_id: String,
    pub final_state: TurnState,
    pub turns_used: u32,
    pub assembled_text: String,
    pub tool_outcomes: Vec<ToolOutcome>,
    pub stop_reason: Option<String>,
    /// The run aborted at a turn boundary because StopTask fired
    /// (Phase 2.3); the task state is Cancelled on the store side.
    pub cancelled: bool,
    /// The run parked at a turn boundary because PauseTask fired
    /// (Phase 2.3); the task state is Waiting(UserInput).
    pub paused: bool,
    /// Last usage snapshot reported by the provider stream (docs/15).
    pub usage: Option<modbit_providers::TokenUsage>,
    /// Compactions applied from a PRECOMPUTED (async worker) plan whose
    /// revision was still current (M4.2). Test/telemetry visibility.
    pub async_compactions: u32,
}

#[derive(Debug)]
pub enum AgentError {
    Transport(String),
    /// A turn state transition was illegal — a runtime bug, fail loudly.
    IllegalTransition {
        from: TurnState,
        to: TurnState,
    },
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::Transport(e) => write!(f, "model transport: {e}"),
            AgentError::IllegalTransition { from, to } => {
                write!(f, "illegal turn transition {from:?} → {to:?}")
            }
        }
    }
}

impl std::error::Error for AgentError {}

fn transition(state: &mut TurnState, to: TurnState) -> Result<(), AgentError> {
    if !can_transition(*state, to) {
        return Err(AgentError::IllegalTransition { from: *state, to });
    }
    *state = to;
    Ok(())
}

/// Refusal recorded when a model tool payload fails validation (docs/14
/// step 5). Shared by the outcome evidence and the error tool result that
/// answers the call on the conversation.
const INVALID_TOOL_PAYLOAD: &str = "invalid tool payload: arguments must be a JSON object";

/// M4.2 async worker slot: a precomputed compaction plan plus the
/// arrival signal the boundary waits on (bounded window, docs/19).
type PrecomputedPlanSlot = std::sync::Arc<(
    std::sync::Mutex<Option<(u64, modbit_compaction::hot_path::CompactionPlan)>>,
    std::sync::Condvar,
)>;

/// One agent, one durable loop (docs/14): prompt → stream → typed events →
/// policy-checked tools → verify → complete. Bounded by `max_turns`.
pub struct OneAgentRuntime<'a> {
    pub transport: &'a dyn ModelTransport,
    pub registry: &'a ToolRegistry,
    pub kernel: &'a PolicyKernel,
    /// Capability grants the agent's tool calls ride on.
    pub grants: &'a [modbit_policy::CapabilityGrant],
    pub max_turns: u32,
    /// Durable-run observer (the scheduler); None in unit tests.
    pub observer: Option<&'a dyn RunObserver>,
    /// Live stop/pause/steer control (Phase 2.3); None in unit tests.
    pub control: Option<&'a dyn RunControl>,
    /// Resume payload (Phase 2.5): a conversation checkpoint restored
    /// from the store after a store kill. When set, the run continues
    /// from this conversation (plus an interruption note) instead of
    /// compiling a fresh prompt.
    pub resume_conversation: Option<Vec<ChatMessage>>,
    /// M4.2 async compaction worker: precompute plans off the turn
    /// thread once the conversation passes a soft threshold; the next
    /// boundary applies the plan only if the conversation revision is
    /// unchanged (stale plans are discarded and recomputed inline).
    pub async_compaction: bool,
}

impl<'a> OneAgentRuntime<'a> {
    pub fn run(&self, task: &AgentTask) -> Result<AgentRunResult, AgentError> {
        let run_id = modbit_domain::RunId::generate().to_string();
        if let Some(o) = self.observer {
            o.run_started(&run_id, 1);
        }
        // Step 3: compile the prompt through the canonical compiler.
        let compiled = compile(&CompilerInputs {
            model: task.model.clone(),
            provider: task.provider.clone(),
            system_policy: task.system_policy.clone(),
            workspace_rules: task.workspace_rules.clone(),
            compaction_epoch: None,
            task_context_pack: format!("objective: {}\n{}", task.objective, task.context_pack),
            recent_events: String::new(),
        });

        let mut state = TurnState::Prepared;
        transition(&mut state, TurnState::Streaming)?;
        let mut turns_used = 0u32;
        let mut assembled = String::new();
        let mut tool_outcomes: Vec<ToolOutcome> = Vec::new();
        let mut stop_reason: Option<String> = None;
        let mut last_usage: Option<modbit_providers::TokenUsage> = None;
        // Conversation carries typed roles across the repair loop (docs/15
        // § Provider contract): the compiled prompt as the user turn, then
        // each model turn as an assistant message (with the tool calls it
        // issued) answered by tool-result messages keyed by call id.
        // Phase 2.5: a resumed run continues from the checkpointed
        // conversation (the compiled prompt is already message 0) with an
        // explicit interruption note — effects between the checkpoint and
        // the kill may have partially applied, so the model must verify.
        let mut conversation: Vec<ChatMessage> = match self.resume_conversation.clone() {
            Some(restored) if !restored.is_empty() => restored,
            _ => vec![ChatMessage::user(compiled.compiled.clone())],
        };
        if self.resume_conversation.is_some() {
            // Every resume attempt — from a checkpoint or fresh — carries
            // the interruption note: effects between the last committed
            // point and the kill may have partially applied, so the model
            // must verify worktree state before continuing.
            conversation.push(ChatMessage::user(
                "[system] the previous run attempt was interrupted after this point; effects between here and the interruption may have partially applied; verify worktree state before continuing.",
            ));
        }
        // Compaction epoch lineage (docs/19 § compaction epochs): the root epoch
        // covers the initial projection; every compaction extends it.
        let mut epochs = modbit_compaction::EpochRegistry::new();
        let mut current_epoch = epochs.create(0, compiled.compiled.as_bytes());
        // M4.2 async worker state: a precomputed plan is INDEX-BASED
        // (truncate at i, summarize [s,e)), so appended messages (new
        // turns, steer notes) do not shift it — it goes stale only when
        // the conversation STRUCTURE changes (a compaction apply). The
        // structure epoch tracks exactly that (docs/19 stale rejection).
        let mut structure_epoch: u64 = 0;
        // Worker slot + arrival signal (docs/19): the boundary gives a
        // pending worker a bounded wait window; if the result does not
        // arrive in time, bounded SYNCHRONOUS compaction runs instead.
        let precomputed: PrecomputedPlanSlot = PrecomputedPlanSlot::default();
        let worker_pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut async_compactions: u32 = 0;

        loop {
            if turns_used >= self.max_turns {
                transition(&mut state, TurnState::Failed)?;
                if let Some(o) = self.observer {
                    o.run_failed(&run_id, "max_turns_exceeded");
                }
                return Ok(AgentRunResult {
                    task_id: task.task_id.clone(),
                    run_id,
                    final_state: state,
                    turns_used,
                    assembled_text: assembled.clone(),
                    tool_outcomes,
                    stop_reason: Some("max_turns_exceeded".into()),
                    cancelled: false,
                    paused: false,
                    usage: last_usage,
                    async_compactions,
                });
            }
            turns_used += 1;
            let turn_id = modbit_domain::TurnId::generate().to_string();
            if let Some(o) = self.observer {
                o.turn_prepared(&turn_id, turns_used);
            }

            // Phase 2.3 — turn boundary control surface:
            // 1. queued steer notes ride as user messages BEFORE the invoke;
            // 2. StopTask aborts the run here (or sooner, from the transport);
            // 3. PauseTask parks the run here.
            if let Some(control) = self.control {
                for note in control.take_steer_notes() {
                    conversation.push(ChatMessage::user(format!("user steer: {note}")));
                }
                if control.is_cancelled() {
                    if let Some(o) = self.observer {
                        o.turn_failed(&turn_id, "cancelled");
                        o.run_failed(&run_id, "cancelled");
                    }
                    return Ok(AgentRunResult {
                        task_id: task.task_id.clone(),
                        run_id,
                        final_state: state,
                        turns_used,
                        assembled_text: assembled,
                        tool_outcomes,
                        stop_reason: Some("cancelled".into()),
                        cancelled: true,
                        paused: false,
                        usage: last_usage,
                        async_compactions,
                    });
                }
                if control.is_paused() {
                    if let Some(o) = self.observer {
                        o.turn_failed(&turn_id, "paused");
                        o.run_failed(&run_id, "paused");
                    }
                    return Ok(AgentRunResult {
                        task_id: task.task_id.clone(),
                        run_id,
                        final_state: state,
                        turns_used,
                        assembled_text: assembled,
                        tool_outcomes,
                        stop_reason: Some("paused".into()),
                        cancelled: false,
                        paused: true,
                        usage: last_usage,
                        async_compactions,
                    });
                }
            }

            // M4.2 async worker: once the conversation passes a soft
            // threshold, precompute the NEXT compaction plan off the turn
            // thread. The plan applies at the next boundary only if the
            // revision is still current (computed against this snapshot).
            if self.async_compaction {
                let estimate: u64 = conversation
                    .iter()
                    .map(|m| modbit_compaction::hot_path::estimate_tokens(&m.content))
                    .sum();
                if estimate > task.max_input_tokens / 5 {
                    let snapshot: Vec<modbit_compaction::hot_path::ConversationItem> =
                        conversation
                            .iter()
                            .map(|m| {
                                use modbit_compaction::hot_path::ItemKind;
                                modbit_compaction::hot_path::ConversationItem {
                                    kind: match m.role {
                                        modbit_providers::gateway::Role::User => {
                                            ItemKind::UserTurn
                                        }
                                        modbit_providers::gateway::Role::Assistant => {
                                            if m.tool_calls.is_empty() {
                                                ItemKind::AssistantText
                                            } else {
                                                ItemKind::AssistantToolCalls
                                            }
                                        }
                                        modbit_providers::gateway::Role::Tool => {
                                            ItemKind::ToolResult
                                        }
                                    },
                                    text: m.content.clone(),
                                }
                            })
                            .collect();
                    let epoch = structure_epoch;
                    let budget = task.max_input_tokens;
                    let slot = precomputed.clone();
                    let pending = worker_pending.clone();
                    pending.store(true, std::sync::atomic::Ordering::SeqCst);
                    std::thread::spawn(move || {
                        let plan = modbit_compaction::hot_path::plan_compaction(&snapshot, budget);
                        let (lock, signal) = &*slot;
                        *lock.lock().expect("precomputed slot") = plan.map(|p| (epoch, p));
                        pending.store(false, std::sync::atomic::Ordering::SeqCst);
                        signal.notify_all();
                    });
                }
            }
            // Token budget (Phase 2.2, docs/19 § compaction): when the
            // conversation estimate exceeds the input budget, compact the
            // MODEL-VISIBLE projection BEFORE the invoke — oldest tool
            // results truncated first, then whole blocks summarized into
            // an epoch line. The canonical history is untouched.
            {
                use modbit_compaction::hot_path::{
                    plan_compaction, CompactionAction, ConversationItem, ItemKind,
                };
                let view: Vec<ConversationItem> = conversation
                    .iter()
                    .map(|m| ConversationItem {
                        kind: match m.role {
                            modbit_providers::gateway::Role::User => ItemKind::UserTurn,
                            modbit_providers::gateway::Role::Assistant => {
                                if m.tool_calls.is_empty() {
                                    ItemKind::AssistantText
                                } else {
                                    ItemKind::AssistantToolCalls
                                }
                            }
                            modbit_providers::gateway::Role::Tool => ItemKind::ToolResult,
                        },
                        text: m.content.clone(),
                    })
                    .collect();
                // M4.2 async worker: a plan precomputed after the previous
                // turn applies ONLY if the conversation revision is still
                // current; anything else (steer note, new turns) is stale
                // and discarded (docs/19 stale rejection) — recompute.
                let mut from_async_worker = false;
                let candidate: Option<modbit_compaction::hot_path::CompactionPlan> = {
                    let (lock, signal) = &*precomputed;
                    let mut slot = lock.lock().expect("precomputed slot");
                    if slot.is_none()
                        && worker_pending.load(std::sync::atomic::Ordering::SeqCst)
                    {
                        let (guard, _timeout) = signal
                            .wait_timeout(slot, std::time::Duration::from_millis(5))
                            .expect("precomputed slot");
                        slot = guard;
                    }
                    match slot.take() {
                        Some((epoch, plan)) if epoch == structure_epoch => {
                            from_async_worker = true;
                            Some(plan)
                        }
                        // Stale or none: recompute synchronously now.
                        _ => plan_compaction(&view, task.max_input_tokens),
                    }
                };
                if let Some(plan) = candidate {
                    let before: u64 = view.iter().map(|i| modbit_compaction::hot_path::estimate_tokens(&i.text)).sum();
                    // Apply actions in reverse order so earlier indices
                    // stay valid while ranges are replaced.
                    let mut affected: u32 = 0;
                    for action in plan.actions.iter().rev() {
                        match action {
                            CompactionAction::TruncateToolResult { index, replacement } => {
                                if let Some(message) = conversation.get_mut(*index) {
                                    if message.role == modbit_providers::gateway::Role::Tool {
                                        affected += 1;
                                        message.content = replacement.clone();
                                    }
                                }
                            }
                            CompactionAction::SummarizeBlock { start, end, replacement } => {
                                if *start < *end && *end <= conversation.len() {
                                    affected += (end - start) as u32;
                                    conversation.splice(
                                        *start..*end,
                                        std::iter::once(ChatMessage::user(replacement.clone())),
                                    );
                                }
                            }
                        }
                    }
                    let projection =
                        serde_json::to_vec(&plan.manifest).unwrap_or_default();
                    if let Ok(epoch) =
                        epochs.compact(&current_epoch.epoch_id, turns_used as u64, &projection)
                    {
                        current_epoch = epoch;
                    }
                    let reclaimed = before.saturating_sub(plan.projected_tokens);
                    if from_async_worker {
                        async_compactions += 1;
                    }
                    structure_epoch += 1; // a compaction changes the structure
                    if let Some(o) = self.observer {
                        o.compaction_applied(
                            &turn_id,
                            &current_epoch.epoch_id,
                            affected,
                            reclaimed,
                            &modbit_compaction::sha256_hex(&projection),
                        );
                    }
                }
            }

            // Step 4: invoke the model through the normalized gateway shape.
            // Tool projection (docs/15): the registry's typed schemas
            // travel to the provider so the model can call real tools.
            let tools = self
                .registry
                .tool_definitions()
                .into_iter()
                .map(|t| modbit_providers::gateway::ToolDefinition {
                    name: t.name,
                    description: t.description,
                    parameters: t.parameters,
                })
                .collect();
            let request = ModelRequest {
                request_id: format!("{}-turn-{turns_used}", task.task_id),
                model: task.model.clone(),
                system: task.system_policy.clone(),
                messages: conversation.clone(),
                max_output_tokens: task.model_settings.max_output_tokens,
                temperature: task.model_settings.temperature,
                reasoning_effort: task.model_settings.reasoning_effort,
                tools,
            };
            let model_step = modbit_domain::RunStepId::generate().to_string();
            if let Some(o) = self.observer {
                o.model_invoke_started(&turn_id, &model_step);
            }
            let events = self
                .transport
                .stream(&request)
                .map_err(AgentError::Transport)?;
            let events = {
                // Merge streamed tool-call fragments into the uniform
                // contract before the runtime consumes them (docs/14 step 5).
                let mut assembler = modbit_providers::gateway::ToolCallAssembler::new();
                let mut normalized = Vec::with_capacity(events.len());
                for event in events {
                    normalized.extend(assembler.feed(event));
                }
                normalized
            };

            // Step 5: parse typed events; reject invalid tool payloads
            // BEFORE side effects. Fragmented tool calls are merged by the
            // assembler so both providers yield the same uniform contract.
            let mut turn_text = String::new();
            let mut issued_calls: Vec<ToolCallData> = Vec::new();
            let mut requested_tools: Vec<(String, String, Value)> = Vec::new();
            for event in events {
                match event {
                    StreamEvent::Delta(text) => {
                        turn_text.push_str(&text);
                        assembled.push_str(&text);
                    }
                    StreamEvent::Usage(usage) => {
                        last_usage = Some(usage);
                    }
                    StreamEvent::Completed {
                        stop_reason: reason,
                    } => {
                        stop_reason = reason;
                    }
                    StreamEvent::ToolCallDelta { .. } => {
                        // The assembler consumes fragments; reaching this
                        // arm is a runtime invariant violation.
                        return Err(AgentError::Transport(
                            "internal: ToolCallDelta escaped the assembler".into(),
                        ));
                    }
                    StreamEvent::ToolRequest {
                        call_id,
                        name,
                        arguments,
                    } => {
                        // EVERY issued call is recorded on the assistant
                        // turn — providers require each tool call to be
                        // answered by exactly one tool result, including
                        // calls whose payload is rejected below.
                        issued_calls.push(ToolCallData {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        });
                        // Payload validation gate: arguments MUST parse to a
                        // JSON object. Invalid payloads never reach tools.
                        match serde_json::from_str::<Value>(&arguments) {
                            Ok(v) if v.is_object() => {
                                requested_tools.push((call_id, name, v));
                            }
                            _ => {
                                tool_outcomes.push(ToolOutcome {
                                    call_id,
                                    name,
                                    result: None,
                                    refusal: Some(INVALID_TOOL_PAYLOAD.to_string()),
                                });
                            }
                        }
                    }
                }
            }

            // The model step ends once its whole event stream is parsed
            // (usage arrives in the final frames, so report after parsing).
            if let Some(o) = self.observer {
                o.model_invoke_finished(&turn_id, &model_step, last_usage);
            }

            if issued_calls.is_empty() {
                // No tools requested: run the verification stage and
                // complete. (Streaming → Executing → Verifying → Completed.)
                transition(&mut state, TurnState::Executing)?;
                transition(&mut state, TurnState::Verifying)?;
                transition(&mut state, TurnState::Completed)?;
                if let Some(o) = self.observer {
                    o.turn_completed(&turn_id);
                    o.run_completed(&run_id);
                }
                return Ok(AgentRunResult {
                    task_id: task.task_id.clone(),
                    run_id,
                    final_state: state,
                    turns_used,
                    assembled_text: assembled,
                    tool_outcomes,
                    stop_reason,
                    cancelled: false,
                    paused: false,
                    usage: last_usage,
                    async_compactions,
                });
            }

            // Steps 6-7: record the assistant turn exactly as issued (its
            // text plus the tool calls), then answer EVERY call in issued
            // order — executed results for valid payloads, error results
            // for rejected ones — so the conversation stays provider-
            // well-formed (docs/15: tool_result / tool role keyed by id).
            conversation.push(ChatMessage::assistant_with_tool_calls(
                turn_text,
                issued_calls.clone(),
            ));
            transition(&mut state, TurnState::Executing)?;
            let parsed_arguments: std::collections::HashMap<String, Value> = requested_tools
                .into_iter()
                .map(|(call_id, _name, arguments)| (call_id, arguments))
                .collect();
            for call in &issued_calls {
                let call_id = call.call_id.clone();
                let name = call.name.clone();
                let Some(arguments) = parsed_arguments.get(&call_id).cloned() else {
                    // Payload rejected during parsing: the call never ran,
                    // but it MUST still be answered by an error result.
                    conversation.push(ChatMessage::tool_result(
                        call_id,
                        INVALID_TOOL_PAYLOAD.to_string(),
                        true,
                    ));
                    continue;
                };
                let tool_step = modbit_domain::RunStepId::generate().to_string();
                let call_id_for_step = call_id.clone();
                if let Some(o) = self.observer {
                    o.tool_step_started(&turn_id, &tool_step, &call_id_for_step, &name);
                }
                let name_for_log = name.clone();
                let effect_class = self
                    .registry
                    .list()
                    .iter()
                    .find(|(n, _, _)| *n == name)
                    .map(|(_, _, class)| *class)
                    .unwrap_or(modbit_policy::EffectClass::ReadOnly);
                let request = ToolCallRequest {
                    tool: name.clone(),
                    effect_class,
                    arguments: arguments.clone(),
                };
                // Step 5 (cont.): the kernel decision is consumed by the
                // fail-closed registry — denial means NO side effect.
                let decision = self.kernel.check(&request, self.grants);
                let outcome = match &decision {
                    PolicyDecision::Deny { reason } => ToolOutcome {
                        call_id,
                        name,
                        result: None,
                        refusal: Some(format!("policy denied: {reason}")),
                    },
                    PolicyDecision::Allow => {
                        match self.registry.execute(&name, &arguments, &decision) {
                            Ok(execution) => ToolOutcome {
                                call_id,
                                name,
                                result: Some(execution.result),
                                refusal: None,
                            },
                            Err(e) => ToolOutcome {
                                call_id,
                                name,
                                result: None,
                                refusal: Some(format!("tool error: {e}")),
                            },
                        }
                    }
                };
                if let Some(o) = self.observer {
                    o.tool_step_finished(
                        &turn_id,
                        &tool_step,
                        &call_id_for_step,
                        &name_for_log,
                        outcome.refusal.is_none(),
                    );
                }
                // The tool-result message answers THIS call id: the result
                // JSON on success, the refusal text as an error otherwise.
                let is_error = outcome.refusal.is_some();
                let content = match (&outcome.result, &outcome.refusal) {
                    (Some(result), _) => result.to_string(),
                    (None, Some(refusal)) => refusal.clone(),
                    (None, None) => String::new(),
                };
                conversation.push(ChatMessage::tool_result(
                    outcome.call_id.clone(),
                    content,
                    is_error,
                ));
                tool_outcomes.push(outcome);
            }
            // Phase 2.5: checkpoint the turn boundary — the durable
            // recovery point for a Core kill mid-run. Bounded: giant
            // conversations rely on compaction (Phase 2.2) instead.
            {
                const MAX_CHECKPOINT_BYTES: usize = 256 * 1024;
                if let Ok(json) = serde_json::to_string(&conversation) {
                    if json.len() <= MAX_CHECKPOINT_BYTES {
                        if let Some(o) = self.observer {
                            o.conversation_checkpointed(&run_id, turns_used, &json);
                        }
                    }
                }
            }
            // Repair loop: back to streaming with the tool evidence.
            transition(&mut state, TurnState::Streaming)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modbit_policy::{CapabilityGrant, EffectClass};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    struct StubTransport {
        /// Scripted responses, consumed per call.
        script: Vec<Vec<StreamEvent>>,
        calls: AtomicUsize,
        /// Every request the runtime sent (asserted by role tests).
        seen: std::sync::Mutex<Vec<ModelRequest>>,
    }

    impl StubTransport {
        fn new(script: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                script,
                calls: AtomicUsize::new(0),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl ModelTransport for StubTransport {
        fn stream(&self, request: &ModelRequest) -> Result<Vec<StreamEvent>, String> {
            self.seen
                .lock()
                .expect("seen requests")
                .push(request.clone());
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            self.script
                .get(index)
                .cloned()
                .ok_or_else(|| "script exhausted".to_string())
        }
    }

    fn runtime<'a>(
        transport: &'a StubTransport,
        registry: &'a ToolRegistry,
        kernel: &'a PolicyKernel,
        grants: &'a [CapabilityGrant],
    ) -> OneAgentRuntime<'a> {
        OneAgentRuntime {
            transport,
            registry,
            kernel,
            grants,
            max_turns: 4,
            observer: None,
            control: None,
            resume_conversation: None,
            async_compaction: false,
        }
    }

    fn task() -> AgentTask {
        AgentTask {
            task_id: "task-1".into(),
            objective: "summarize the workspace".into(),
            model: "test-model".into(),
            provider: "openai".into(),
            system_policy: "be terse".into(),
            workspace_rules: String::new(),
            context_pack: String::new(),
            model_settings: modbit_providers::profiles::ModelSettings::BASE,
            max_input_tokens: DEFAULT_MAX_INPUT_TOKENS,
        }
    }

    /// The completion path: stream → events → verify → Completed, with the
    /// prompt compiled through the canonical compiler.
    #[test]
    fn simple_task_completes_with_assembled_text() {
        let transport = StubTransport::new(vec![vec![
            StreamEvent::Delta("summary: ".into()),
            StreamEvent::Delta("42 files".into()),
            StreamEvent::Completed {
                stop_reason: Some("stop".into()),
            },
        ]]);
        let registry = ToolRegistry::new();
        let kernel = PolicyKernel::new(vec![]);
        let rt = runtime(&transport, &registry, &kernel, &[]);

        let result = rt.run(&task()).unwrap();
        assert_eq!(result.final_state, TurnState::Completed);
        assert_eq!(result.assembled_text, "summary: 42 files");
        assert_eq!(result.turns_used, 1);
        assert_eq!(result.stop_reason.as_deref(), Some("stop"));
        assert!(result.tool_outcomes.is_empty());
    }

    /// A tool request is executed under policy and its result feeds the
    /// repair turn (docs/14 steps 6-7).
    #[test]
    fn tool_request_executes_under_policy_and_feeds_back() {
        use std::sync::Mutex;
        let invocations = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = invocations.clone();

        let transport = StubTransport::new(vec![
            vec![StreamEvent::ToolRequest {
                call_id: "call-1".into(),
                name: "modbit.file.read".into(),
                arguments: r#"{"path":"src/lib.rs"}"#.into(),
            }],
            vec![
                StreamEvent::Delta("file is 12 lines".into()),
                StreamEvent::Completed {
                    stop_reason: Some("stop".into()),
                },
            ],
        ]);
        let registry = ToolRegistry::new();
        registry
            .register(
                "modbit.file.read",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(move |args| {
                    sink.lock().unwrap().push(args.clone());
                    Ok(serde_json::json!({"lines": 12}))
                }),
            )
            .unwrap();
        let kernel = PolicyKernel::new(vec![]);
        let grants = vec![CapabilityGrant {
            grant_id: "g1".into(),
            tool: "modbit.file.read".into(),
            effect_class: EffectClass::ReadOnly,
        }];
        let rt = runtime(&transport, &registry, &kernel, &grants);

        let result = rt.run(&task()).unwrap();
        assert_eq!(result.final_state, TurnState::Completed);
        assert_eq!(result.turns_used, 2, "tool turn + repair turn");
        assert_eq!(
            result.tool_outcomes[0],
            ToolOutcome {
                call_id: "call-1".into(),
                name: "modbit.file.read".into(),
                result: Some(serde_json::json!({"lines": 12})),
                refusal: None,
            }
        );
        assert_eq!(
            invocations.lock().unwrap().len(),
            1,
            "executed exactly once"
        );
    }

    /// Phase 2.1 (Future-tasks §2.1): the repair-turn request carries typed
    /// roles — user prompt, assistant turn WITH the tool calls it issued,
    /// and a tool-result message keyed by the SAME call id. No flattened
    /// "tool <name> → …" user strings.
    #[test]
    fn repair_turn_carries_typed_roles_and_call_id_linkage() {
        use modbit_providers::gateway::Role;
        let transport = StubTransport::new(vec![
            vec![
                StreamEvent::Delta("reading the file first".into()),
                StreamEvent::ToolRequest {
                    call_id: "call-1".into(),
                    name: "modbit.file.read".into(),
                    arguments: r#"{"path":"src/lib.rs"}"#.into(),
                },
                StreamEvent::Completed {
                    stop_reason: Some("tool_calls".into()),
                },
            ],
            vec![
                StreamEvent::Delta("done".into()),
                StreamEvent::Completed {
                    stop_reason: Some("stop".into()),
                },
            ],
        ]);
        let registry = ToolRegistry::new();
        registry
            .register(
                "modbit.file.read",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(|_args| Ok(serde_json::json!({"lines": 12}))),
            )
            .unwrap();
        let kernel = PolicyKernel::new(vec![]);
        let grants = vec![CapabilityGrant {
            grant_id: "g1".into(),
            tool: "modbit.file.read".into(),
            effect_class: EffectClass::ReadOnly,
        }];
        let rt = runtime(&transport, &registry, &kernel, &grants);

        let result = rt.run(&task()).unwrap();
        assert_eq!(result.final_state, TurnState::Completed);

        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "tool turn + repair turn");
        let first = &seen[0];
        // Turn 1: exactly one user message — the compiled prompt.
        assert_eq!(first.messages.len(), 1);
        assert_eq!(first.messages[0].role, Role::User);
        assert!(!first.messages[0].content.is_empty());

        let second = &seen[1];
        assert_eq!(second.messages.len(), 3, "user + assistant + tool result");
        // Assistant turn carries the model's text AND the issued call.
        let assistant = &second.messages[1];
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.content, "reading the file first");
        assert_eq!(assistant.tool_calls.len(), 1);
        assert_eq!(assistant.tool_calls[0].call_id, "call-1");
        assert_eq!(assistant.tool_calls[0].name, "modbit.file.read");
        assert_eq!(assistant.tool_calls[0].arguments, r#"{"path":"src/lib.rs"}"#);
        // Tool result answers the same call id with the result JSON.
        let tool = &second.messages[2];
        assert_eq!(tool.role, Role::Tool);
        assert_eq!(tool.tool_call_id.as_deref(), Some("call-1"));
        assert!(!tool.is_error);
        let content: Value = serde_json::from_str(&tool.content).unwrap();
        assert_eq!(content, serde_json::json!({"lines": 12}));
        // The old flattened format is gone.
        assert!(second
            .messages
            .iter()
            .all(|m| !(m.role == Role::User && m.content.starts_with("tool "))));
    }

    /// Phase 2.1: a call with an INVALID payload is still recorded on the
    /// assistant turn and answered by an error tool result — providers
    /// require every tool call to be answered exactly once.
    #[test]
    fn invalid_payload_call_is_answered_with_error_tool_result() {
        use modbit_providers::gateway::Role;
        let transport = StubTransport::new(vec![
            vec![StreamEvent::ToolRequest {
                call_id: "call-bad".into(),
                name: "modbit.file.read".into(),
                arguments: "not json".into(),
            }],
            vec![
                StreamEvent::Delta("handled".into()),
                StreamEvent::Completed {
                    stop_reason: Some("stop".into()),
                },
            ],
        ]);
        let registry = ToolRegistry::new();
        registry
            .register(
                "modbit.file.read",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(|_args| Ok(serde_json::json!({}))),
            )
            .unwrap();
        let kernel = PolicyKernel::new(vec![]);
        let grants = vec![CapabilityGrant {
            grant_id: "g1".into(),
            tool: "modbit.file.read".into(),
            effect_class: EffectClass::ReadOnly,
        }];
        let rt = runtime(&transport, &registry, &kernel, &grants);

        let result = rt.run(&task()).unwrap();
        assert_eq!(result.final_state, TurnState::Completed);

        let seen = transport.seen.lock().unwrap();
        let second = &seen[1];
        let assistant = &second.messages[1];
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.tool_calls.len(), 1, "invalid call still recorded");
        assert_eq!(assistant.tool_calls[0].call_id, "call-bad");
        let tool = &second.messages[2];
        assert_eq!(tool.role, Role::Tool);
        assert_eq!(tool.tool_call_id.as_deref(), Some("call-bad"));
        assert!(tool.is_error, "refusal marks the tool result as error");
        assert!(tool.content.contains("invalid tool payload"));
    }

    /// Phase 2.1: a policy-denied call feeds back as an error tool result
    /// so the model can repair — with no side effect (existing behavior,
    /// now asserted on the wire shape too).
    #[test]
    fn denied_call_feeds_back_as_error_tool_result() {
        use modbit_providers::gateway::Role;
        let transport = StubTransport::new(vec![
            vec![StreamEvent::ToolRequest {
                call_id: "call-2".into(),
                name: "modbit.shell.run".into(),
                arguments: r#"{"argv":["rm","-rf","/"]}"#.into(),
            }],
            vec![
                StreamEvent::Delta("refused".into()),
                StreamEvent::Completed {
                    stop_reason: Some("stop".into()),
                },
            ],
        ]);
        let registry = ToolRegistry::new();
        registry
            .register(
                "modbit.shell.run",
                "1.0.0",
                EffectClass::External,
                Arc::new(|_args| Ok(serde_json::json!({}))),
            )
            .unwrap();
        let kernel = PolicyKernel::new(vec![]);
        let rt = runtime(&transport, &registry, &kernel, &[]); // NO grants

        let result = rt.run(&task()).unwrap();
        assert_eq!(result.final_state, TurnState::Completed);
        let seen = transport.seen.lock().unwrap();
        let tool = &seen[1].messages[2];
        assert_eq!(tool.role, Role::Tool);
        assert_eq!(tool.tool_call_id.as_deref(), Some("call-2"));
        assert!(tool.is_error);
        assert!(tool.content.contains("policy denied"));
    }

    /// docs/14 step 5: an INVALID tool payload is rejected BEFORE any side
    /// effect — the handler never runs.
    #[test]
    fn invalid_tool_payload_is_rejected_before_side_effects() {
        let invoked = Arc::new(AtomicUsize::new(0));
        let sink = invoked.clone();

        let transport = StubTransport::new(vec![
            vec![StreamEvent::ToolRequest {
                call_id: "call-bad".into(),
                name: "modbit.file.read".into(),
                arguments: "not json at all".into(),
            }],
            vec![
                StreamEvent::Delta("handled".into()),
                StreamEvent::Completed {
                    stop_reason: Some("stop".into()),
                },
            ],
        ]);
        let registry = ToolRegistry::new();
        registry
            .register(
                "modbit.file.read",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(move |_args| {
                    sink.fetch_add(1, Ordering::SeqCst);
                    Ok(serde_json::json!({}))
                }),
            )
            .unwrap();
        let kernel = PolicyKernel::new(vec![]);
        let grants = vec![CapabilityGrant {
            grant_id: "g1".into(),
            tool: "modbit.file.read".into(),
            effect_class: EffectClass::ReadOnly,
        }];
        let rt = runtime(&transport, &registry, &kernel, &grants);

        let result = rt.run(&task()).unwrap();
        assert_eq!(result.final_state, TurnState::Completed);
        assert_eq!(invoked.load(Ordering::SeqCst), 0, "handler must not run");
        assert!(result.tool_outcomes[0]
            .refusal
            .as_deref()
            .unwrap()
            .contains("invalid tool payload"));
    }

    /// A tool the kernel denies produces a refusal — never an execution.
    #[test]
    fn policy_denied_tool_is_refused_not_executed() {
        let invoked = Arc::new(AtomicUsize::new(0));
        let sink = invoked.clone();

        let transport = StubTransport::new(vec![
            vec![StreamEvent::ToolRequest {
                call_id: "call-2".into(),
                name: "modbit.shell.run".into(),
                arguments: r#"{"argv":["rm","-rf","/"]}"#.into(),
            }],
            vec![
                StreamEvent::Delta("refused and continued".into()),
                StreamEvent::Completed {
                    stop_reason: Some("stop".into()),
                },
            ],
        ]);
        let registry = ToolRegistry::new();
        registry
            .register(
                "modbit.shell.run",
                "1.0.0",
                EffectClass::External,
                Arc::new(move |_args| {
                    sink.fetch_add(1, Ordering::SeqCst);
                    Ok(serde_json::json!({}))
                }),
            )
            .unwrap();
        let kernel = PolicyKernel::new(vec![]);
        let rt = runtime(&transport, &registry, &kernel, &[]); // NO grants

        let result = rt.run(&task()).unwrap();
        assert_eq!(invoked.load(Ordering::SeqCst), 0, "denied tool never runs");
        assert!(result.tool_outcomes[0]
            .refusal
            .as_deref()
            .unwrap()
            .contains("policy denied"));
    }

    /// A runaway tool loop is bounded by max_turns and fails closed.
    #[test]
    fn runaway_tool_loop_is_bounded() {        let endless = || {
            vec![StreamEvent::ToolRequest {
                call_id: format!("call-{}", rand_suffix()),
                name: "modbit.file.read".into(),
                arguments: r#"{"path":"x"}"#.into(),
            }]
        };
        fn rand_suffix() -> u32 {
            use std::sync::atomic::AtomicU32;
            static N: AtomicU32 = AtomicU32::new(0);
            N.fetch_add(1, Ordering::SeqCst)
        }

        let transport = StubTransport::new((0..16).map(|_| endless()).collect());
        let registry = ToolRegistry::new();
        registry
            .register(
                "modbit.file.read",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(|_args| Ok(serde_json::json!({"ok": true}))),
            )
            .unwrap();
        let kernel = PolicyKernel::new(vec![]);
        let grants = vec![CapabilityGrant {
            grant_id: "g1".into(),
            tool: "modbit.file.read".into(),
            effect_class: EffectClass::ReadOnly,
        }];
        let rt = OneAgentRuntime {
            transport: &transport,
            registry: &registry,
            kernel: &kernel,
            grants: &grants,
            max_turns: 3,
            observer: None,
            control: None,
            resume_conversation: None,
            async_compaction: false,
        };

        let result = rt.run(&task()).unwrap();
        assert_eq!(result.final_state, TurnState::Failed);
        assert_eq!(result.turns_used, 3);
        assert_eq!(result.stop_reason.as_deref(), Some("max_turns_exceeded"));
    }

    /// Records compaction notifications for Phase 2.2 assertions.
    struct RecordingCompactions {
        events: std::sync::Mutex<Vec<(String, u32, u64)>>,
    }

    impl RunObserver for RecordingCompactions {
        fn compaction_applied(
            &self,
            _turn_id: &str,
            epoch_id: &str,
            affected: u32,
            reclaimed: u64,
            _digest: &str,
        ) {
            self.events
                .lock()
                .unwrap()
                .push((epoch_id.to_string(), affected, reclaimed));
        }
    }

    /// Phase 2.2: an over-budget conversation compacts BEFORE the next
    /// invoke — the tool-result payload travels truncated, call-id
    /// linkage survives, and the observer sees the compaction with a
    /// positive reclaim. Default budget sends everything verbatim.
    #[test]
    fn over_budget_conversation_compacts_before_invoke() {
        use modbit_providers::gateway::Role;
        let big_result = "x".repeat(8_000); // ~2000 estimated tokens

        let transport = StubTransport::new(vec![
            vec![StreamEvent::ToolRequest {
                call_id: "call-big".into(),
                name: "modbit.file.read".into(),
                arguments: r#"{"path":"big.txt"}"#.into(),
            }],
            vec![
                StreamEvent::Delta("done".into()),
                StreamEvent::Completed {
                    stop_reason: Some("stop".into()),
                },
            ],
        ]);
        let registry = ToolRegistry::new();
        registry
            .register(
                "modbit.file.read",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(move |_args| {
                    Ok(serde_json::json!({ "content": big_result }))
                }),
            )
            .unwrap();
        let kernel = PolicyKernel::new(vec![]);
        let grants = vec![CapabilityGrant {
            grant_id: "g1".into(),
            tool: "modbit.file.read".into(),
            effect_class: EffectClass::ReadOnly,
        }];

        let observer = RecordingCompactions {
            events: std::sync::Mutex::new(Vec::new()),
        };
        let mut small_budget = task();
        small_budget.max_input_tokens = 700; // prompt + marker fits; result does not
        let rt = OneAgentRuntime {
            transport: &transport,
            registry: &registry,
            kernel: &kernel,
            grants: &grants,
            max_turns: 4,
            observer: Some(&observer),
            control: None,
            resume_conversation: None,
            async_compaction: false,
        };
        let result = rt.run(&small_budget).unwrap();
        assert_eq!(result.final_state, TurnState::Completed);

        // The repair-turn request carried a TRUNCATED tool result with
        // the compaction marker, linkage intact.
        let seen = transport.seen.lock().unwrap();
        let messages = &seen[1].messages;
        let tool = messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("tool result still travels");
        assert_eq!(tool.tool_call_id.as_deref(), Some("call-big"));
        assert!(
            tool.content.contains("[compacted:"),
            "result truncated in flight: {} bytes",
            tool.content.len()
        );
        assert!(tool.content.len() < 1_000, "payload actually shrank");
        // The observer recorded the compaction with a positive reclaim.
        let events = observer.events.lock().unwrap();
        assert!(!events.is_empty(), "compaction observed");
        assert!(events[0].2 > 0, "reclaimed tokens positive");
        assert!(events[0].0.starts_with("epoch-"), "epoch lineage: {}", events[0].0);

        // Default budget: the SAME run shape sends the result verbatim.
        let transport = StubTransport::new(vec![
            vec![StreamEvent::ToolRequest {
                call_id: "call-big".into(),
                name: "modbit.file.read".into(),
                arguments: r#"{"path":"big.txt"}"#.into(),
            }],
            vec![
                StreamEvent::Delta("done".into()),
                StreamEvent::Completed {
                    stop_reason: Some("stop".into()),
                },
            ],
        ]);
        let registry = ToolRegistry::new();
        registry
            .register(
                "modbit.file.read",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(|_args| Ok(serde_json::json!({ "content": "x".repeat(8_000) }))),
            )
            .unwrap();
        let rt = OneAgentRuntime {
            transport: &transport,
            registry: &registry,
            kernel: &kernel,
            grants: &grants,
            max_turns: 4,
            observer: None,
            control: None,
            resume_conversation: None,
            async_compaction: false,
        };
        rt.run(&task()).unwrap();
        let seen = transport.seen.lock().unwrap();
        let tool = seen[1]
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .unwrap();
        assert!(tool.content.len() > 8_000, "verbatim under default budget");
    }

    /// Phase 2.3 boundary control stub: flags flip at a given turn
    /// ordinal; steer notes drain once per turn.
    struct BoundaryControl {
        turn: std::sync::atomic::AtomicUsize,
        cancel_at: Option<usize>,
        pause_at: Option<usize>,
        notes: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    impl BoundaryControl {
        fn new() -> Self {
            BoundaryControl {
                turn: std::sync::atomic::AtomicUsize::new(0),
                cancel_at: None,
                pause_at: None,
                notes: std::sync::Mutex::new(std::collections::VecDeque::new()),
            }
        }

        fn cancelled_at(mut self, turn: usize) -> Self {
            self.cancel_at = Some(turn);
            self
        }

        fn paused_at(mut self, turn: usize) -> Self {
            self.pause_at = Some(turn);
            self
        }

        fn queue_note(&self, note: &str) {
            self.notes.lock().unwrap().push_back(note.to_string());
        }
    }

    impl RunControl for BoundaryControl {
        fn is_cancelled(&self) -> bool {
            self.cancel_at == Some(self.turn.load(Ordering::SeqCst))
        }

        fn is_paused(&self) -> bool {
            self.pause_at == Some(self.turn.load(Ordering::SeqCst))
        }

        fn take_steer_notes(&self) -> Vec<String> {
            self.turn.fetch_add(1, Ordering::SeqCst);
            self.notes.lock().unwrap().drain(..).collect()
        }
    }

    /// Phase 2.3: StopTask aborts the run at the next turn boundary — no
    /// further invokes, cancelled result, run-level failure code recorded.
    #[test]
    fn stop_aborts_the_run_at_the_turn_boundary() {
        let transport = StubTransport::new(vec![
            vec![StreamEvent::ToolRequest {
                call_id: "call-1".into(),
                name: "modbit.file.read".into(),
                arguments: r#"{"path":"x"}"#.into(),
            }],
            // Turn 2 never happens: the boundary check fires first.
            vec![
                StreamEvent::Delta("unreachable".into()),
                StreamEvent::Completed {
                    stop_reason: Some("stop".into()),
                },
            ],
        ]);
        let registry = ToolRegistry::new();
        registry
            .register(
                "modbit.file.read",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(|_args| Ok(serde_json::json!({"ok": true}))),
            )
            .unwrap();
        let kernel = PolicyKernel::new(vec![]);
        let grants = vec![CapabilityGrant {
            grant_id: "g1".into(),
            tool: "modbit.file.read".into(),
            effect_class: EffectClass::ReadOnly,
        }];
        let control = BoundaryControl::new().cancelled_at(2);
        let rt = OneAgentRuntime {
            transport: &transport,
            registry: &registry,
            kernel: &kernel,
            grants: &grants,
            max_turns: 8,
            observer: None,
            control: Some(&control),
            resume_conversation: None,
            async_compaction: false,
        };

        let result = rt.run(&task()).unwrap();
        assert!(result.cancelled);
        assert!(!result.paused);
        assert_eq!(result.stop_reason.as_deref(), Some("cancelled"));
        assert_eq!(result.turns_used, 2, "boundary consumed turn 2");
        assert_eq!(
            transport.seen.lock().unwrap().len(),
            1,
            "no invoke after the stop boundary"
        );
    }

    /// Phase 2.3: PauseTask parks the run at the next turn boundary.
    #[test]
    fn pause_parks_the_run_at_the_turn_boundary() {
        let transport = StubTransport::new(vec![vec![StreamEvent::ToolRequest {
            call_id: "call-1".into(),
            name: "modbit.file.read".into(),
            arguments: r#"{"path":"x"}"#.into(),
        }]]);
        let registry = ToolRegistry::new();
        registry
            .register(
                "modbit.file.read",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(|_args| Ok(serde_json::json!({"ok": true}))),
            )
            .unwrap();
        let kernel = PolicyKernel::new(vec![]);
        let grants = vec![CapabilityGrant {
            grant_id: "g1".into(),
            tool: "modbit.file.read".into(),
            effect_class: EffectClass::ReadOnly,
        }];
        let control = BoundaryControl::new().paused_at(2);
        let rt = OneAgentRuntime {
            transport: &transport,
            registry: &registry,
            kernel: &kernel,
            grants: &grants,
            max_turns: 8,
            observer: None,
            control: Some(&control),
            resume_conversation: None,
            async_compaction: false,
        };

        let result = rt.run(&task()).unwrap();
        assert!(result.paused);
        assert!(!result.cancelled);
        assert_eq!(result.stop_reason.as_deref(), Some("paused"));
    }

    /// Phase 2.5: a resumed run continues from the checkpointed
    /// conversation with the interruption note appended.
    #[test]
    fn resumed_run_continues_from_checkpoint_with_note() {
        use modbit_providers::gateway::{Role, ToolCallData};
        let transport = StubTransport::new(vec![vec![
            StreamEvent::Delta("continued".into()),
            StreamEvent::Completed {
                stop_reason: Some("stop".into()),
            },
        ]]);
        let registry = ToolRegistry::new();
        let kernel = PolicyKernel::new(vec![]);
        let checkpoint = vec![
            ChatMessage::user("original prompt"),
            ChatMessage::assistant_with_tool_calls(
                "",
                vec![ToolCallData {
                    call_id: "c1".into(),
                    name: "fs.read".into(),
                    arguments: r#"{"path":"x"}"#.into(),
                }],
            ),
            ChatMessage::tool_result("c1", r#"{"content":"file"}"#, false),
        ];
        let rt = OneAgentRuntime {
            transport: &transport,
            registry: &registry,
            kernel: &kernel,
            grants: &[],
            max_turns: 4,
            observer: None,
            control: None,
            resume_conversation: Some(checkpoint),
            async_compaction: false,
        };

        let result = rt.run(&task()).unwrap();
        assert_eq!(result.final_state, TurnState::Completed);
        let seen = transport.seen.lock().unwrap();
        let messages = &seen[0].messages;
        assert_eq!(messages.len(), 4, "checkpoint + interruption note");
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        let note = &messages[3];
        assert_eq!(note.role, Role::User);
        assert!(note.content.contains("previous run attempt was interrupted"));
    }

    /// M4.2: a plan precomputed off the turn thread applies at the next
    /// boundary when the conversation revision is still current.
    #[test]
    fn async_compaction_plan_applies_when_fresh() {
        use modbit_providers::gateway::Role;
        let big = "y".repeat(8_000);
        // Three turns: the plan precomputed at boundary 2 (while turn 2's
        // tool runs) is consumed at boundary 3.
        let transport = StubTransport::new(vec![
            vec![StreamEvent::ToolRequest {
                call_id: "call-1".into(),
                name: "modbit.file.read".into(),
                arguments: r#"{"path":"big"}"#.into(),
            }],
            vec![StreamEvent::ToolRequest {
                call_id: "call-2".into(),
                name: "modbit.file.read".into(),
                arguments: r#"{"path":"big2"}"#.into(),
            }],
            vec![
                StreamEvent::Delta("done".into()),
                StreamEvent::Completed {
                    stop_reason: Some("stop".into()),
                },
            ],
        ]);
        let registry = ToolRegistry::new();
        registry
            .register(
                "modbit.file.read",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(move |_args| {
                    // Give the async worker the turn's window.
                    std::thread::sleep(Duration::from_millis(25));
                    Ok(serde_json::json!({ "content": big }))
                }),
            )
            .unwrap();
        let kernel = PolicyKernel::new(vec![]);
        let grants = vec![CapabilityGrant {
            grant_id: "g1".into(),
            tool: "modbit.file.read".into(),
            effect_class: EffectClass::ReadOnly,
        }];
        let mut budget = task();
        budget.max_input_tokens = 700;
        let rt = OneAgentRuntime {
            transport: &transport,
            registry: &registry,
            kernel: &kernel,
            grants: &grants,
            max_turns: 4,
            observer: None,
            control: None,
            resume_conversation: None,
            async_compaction: true,
        };

        let result = rt.run(&budget).unwrap();
        assert_eq!(result.final_state, TurnState::Completed);
        assert!(
            result.async_compactions >= 1,
            "the precomputed plan applied (async_compactions: {})",
            result.async_compactions
        );
        // The turn-3 request carries truncated tool results.
        let seen = transport.seen.lock().unwrap();
        let tools: Vec<_> = seen[2]
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert!(!tools.is_empty());
        assert!(tools
            .iter()
            .any(|m| m.content.contains("[compacted:")));
    }

    /// M4.2 freshness semantics: plans are index-based, so APPENDED
    /// messages (a steer note riding between the precompute and the
    /// boundary) do not invalidate the precomputed plan — it still
    /// applies from the worker. (Staleness by structural change is the
    /// compaction crate's epoch/apply guard, unit-proven there.)
    #[test]
    fn appended_steer_note_does_not_invalidate_the_async_plan() {
        use modbit_providers::gateway::Role;
        use std::sync::atomic::AtomicBool;
        let big = "z".repeat(8_000);
        let steered = Arc::new(AtomicBool::new(false));
        let flag = steered.clone();
        let transport = StubTransport::new(vec![
            vec![StreamEvent::ToolRequest {
                call_id: "call-1".into(),
                name: "modbit.file.read".into(),
                arguments: r#"{"path":"big"}"#.into(),
            }],
            vec![StreamEvent::ToolRequest {
                call_id: "call-2".into(),
                name: "modbit.file.read".into(),
                arguments: r#"{"path":"big2"}"#.into(),
            }],
            vec![
                StreamEvent::Delta("done".into()),
                StreamEvent::Completed {
                    stop_reason: Some("stop".into()),
                },
            ],
        ]);
        let registry = ToolRegistry::new();
        registry
            .register(
                "modbit.file.read",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(move |_args| {
                    std::thread::sleep(Duration::from_millis(25));
                    // Steer lands during the tools (appended at the next
                    // boundary): an append must NOT invalidate the plan.
                    flag.store(true, Ordering::SeqCst);
                    Ok(serde_json::json!({ "content": big }))
                }),
            )
            .unwrap();
        struct SteerOnce(AtomicBool);
        impl RunControl for SteerOnce {
            fn take_steer_notes(&self) -> Vec<String> {
                if self.0.swap(false, Ordering::SeqCst) {
                    vec!["revised priorities".into()]
                } else {
                    Vec::new()
                }
            }
        }
        let control = SteerOnce(AtomicBool::new(true));
        let kernel = PolicyKernel::new(vec![]);
        let grants = vec![CapabilityGrant {
            grant_id: "g1".into(),
            tool: "modbit.file.read".into(),
            effect_class: EffectClass::ReadOnly,
        }];
        let mut budget = task();
        budget.max_input_tokens = 700;
        let rt = OneAgentRuntime {
            transport: &transport,
            registry: &registry,
            kernel: &kernel,
            grants: &grants,
            max_turns: 4,
            observer: None,
            control: Some(&control),
            resume_conversation: None,
            async_compaction: true,
        };

        let result = rt.run(&budget).unwrap();
        assert_eq!(result.final_state, TurnState::Completed);
        assert!(
            result.async_compactions >= 1,
            "the append-only change keeps the plan fresh (async_compactions: {})",
            result.async_compactions
        );
        // The steer note still rode the conversation, and the plan still
        // compacted the tool results on the turn-3 request.
        let seen = transport.seen.lock().unwrap();
        let tools: Vec<_> = seen[2]
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert!(tools
            .iter()
            .any(|m| m.content.contains("[compacted:")));
        let steered_request_has_note = seen[2]
            .messages
            .iter()
            .any(|m| m.role == Role::User && m.content.contains("revised priorities"));
        assert!(steered_request_has_note, "the steer note rode the turn");
    }

    /// Phase 2.3: SteerTask notes ride as user messages before the next
    /// invoke (Future-tasks §2.5: stored AND injected).
    #[test]
    fn steer_notes_are_injected_as_user_messages() {
        use modbit_providers::gateway::Role;
        let transport = StubTransport::new(vec![
            vec![StreamEvent::ToolRequest {
                call_id: "call-1".into(),
                name: "modbit.file.read".into(),
                arguments: r#"{"path":"x"}"#.into(),
            }],
            vec![
                StreamEvent::Delta("steered".into()),
                StreamEvent::Completed {
                    stop_reason: Some("stop".into()),
                },
            ],
        ]);
        let registry = ToolRegistry::new();
        let control = Arc::new(BoundaryControl::new());
        let queuer = control.clone();
        registry
            .register(
                "modbit.file.read",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(move |_args| {
                    // The steer arrives mid-turn-1 (while the tool runs);
                    // the loop must inject it before the NEXT invoke.
                    queuer.queue_note("prefer the typed roles");
                    Ok(serde_json::json!({"ok": true}))
                }),
            )
            .unwrap();
        let kernel = PolicyKernel::new(vec![]);
        let grants = vec![CapabilityGrant {
            grant_id: "g1".into(),
            tool: "modbit.file.read".into(),
            effect_class: EffectClass::ReadOnly,
        }];
        let rt = OneAgentRuntime {
            transport: &transport,
            registry: &registry,
            kernel: &kernel,
            grants: &grants,
            max_turns: 8,
            observer: None,
            control: Some(&*control),
            resume_conversation: None,
            async_compaction: false,
        };

        let result = rt.run(&task()).unwrap();
        assert_eq!(result.final_state, TurnState::Completed);
        let seen = transport.seen.lock().unwrap();
        let second = &seen[1];
        let steer = second
            .messages
            .iter()
            .find(|m| m.role == Role::User && m.content.starts_with("user steer: "))
            .expect("steer note rides as a user message");
        assert!(steer.content.contains("prefer the typed roles"));
        // Placement: after the tool result it answers, before the invoke
        // consumes it — i.e. it is the LAST message of the request.
        assert_eq!(steer.content, second.messages.last().unwrap().content);
    }
}
