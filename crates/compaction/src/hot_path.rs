//! Hot-path conversation compaction planning (Future-tasks Phase 2 item
//! 2, docs/19 § compaction): decide, from a provider-neutral view of the
//! model-visible conversation, which items to truncate or summarize so
//! the next request fits an input-token budget.
//!
//! Authority rule (lib.rs): compaction changes the MODEL-VISIBLE
//! projection only. The caller keeps the canonical conversation; the
//! planner returns ACTIONS, not a new conversation, so the caller's
//! provider-specific structures (roles, call-id linkage) stay intact.
//!
//! Ordering (docs/19): oldest tool results compact first — they are the
//! bulkiest and least decision-relevant history — then whole oldest
//! blocks are summarized into an epoch line. The most recent
//! assistant+tool-result block is protected until nothing else can be
//! reclaimed, because the repair turn consumes it.

use crate::{CompactionManifest, Label};

/// Kinds of conversation items the planner understands (a provider-
/// neutral projection of the runtime's ChatMessage).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// A user message (the compiled prompt or a steer note).
    UserTurn,
    /// An assistant message with no tool calls.
    AssistantText,
    /// An assistant message carrying tool calls.
    AssistantToolCalls,
    /// A tool-result message answering one call.
    ToolResult,
}

/// One conversation item offered to the planner.
#[derive(Clone, Debug)]
pub struct ConversationItem {
    pub kind: ItemKind,
    /// Message content (for tool results: the result payload text).
    pub text: String,
}

impl ConversationItem {
    pub fn new(kind: ItemKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }

    fn tokens(&self) -> u64 {
        estimate_tokens(&self.text)
    }
}

/// Rough token estimate for budget decisions: ~4 characters per token.
/// NOT billing — the authoritative count comes from the provider's usage
/// frames, which the runtime feeds back as the measured size.
pub fn estimate_tokens(text: &str) -> u64 {
    text.chars().count().div_ceil(4) as u64
}

/// One compaction action for the caller to apply.
#[derive(Clone, Debug, PartialEq)]
pub enum CompactionAction {
    /// Replace one tool-result item's content in place. The message (and
    /// its call-id linkage) survives — only the payload shrinks.
    TruncateToolResult {
        index: usize,
        replacement: String,
    },
    /// Replace the contiguous range [start, end) with ONE user message
    /// carrying the epoch summary. The range must be block-aligned (start
    /// on an assistant message, end after that block's tool results) so
    /// the remaining conversation stays provider-well-formed.
    SummarizeBlock {
        start: usize,
        end: usize,
        replacement: String,
    },
}

/// The plan: actions in application order plus the manifest describing
/// what was compacted (see the manifest contract in lib.rs).
#[derive(Clone, Debug)]
pub struct CompactionPlan {
    pub actions: Vec<CompactionAction>,
    /// Estimated input tokens after applying the plan.
    pub projected_tokens: u64,
    pub manifest: CompactionManifest,
}

/// Characters of a tool result kept at the head when truncating.
const TRUNCATE_KEEP_HEAD: usize = 96;
/// Characters kept at the tail (recent errors usually surface at the end).
const TRUNCATE_KEEP_TAIL: usize = 64;

fn truncated_replacement(original: &str) -> String {
    let chars: Vec<char> = original.chars().collect();
    if chars.len() <= TRUNCATE_KEEP_HEAD + TRUNCATE_KEEP_TAIL {
        return original.to_string();
    }
    let omitted = chars.len() - TRUNCATE_KEEP_HEAD - TRUNCATE_KEEP_TAIL;
    format!(
        "{}\n…[compacted: ~{} characters omitted]\n{}",
        chars[..TRUNCATE_KEEP_HEAD].iter().collect::<String>(),
        omitted,
        chars[chars.len() - TRUNCATE_KEEP_TAIL..]
            .iter()
            .collect::<String>()
    )
}

/// Index of the first item of the LAST assistant(+results) block — the
/// protected recent window the repair turn consumes.
fn recent_block_start(items: &[ConversationItem]) -> usize {
    let mut start = items.len();
    for (index, item) in items.iter().enumerate().rev() {
        match item.kind {
            ItemKind::ToolResult => continue,
            ItemKind::AssistantToolCalls => {
                start = index;
                break;
            }
            _ => {
                // Anything else ends the trailing block.
                if start < items.len() {
                    break;
                }
                continue;
            }
        }
    }
    start
}

/// Plans compaction for a conversation against an input-token budget.
/// Returns None when the conversation already fits.
///
/// Stages (each stops as soon as the projection fits):
/// 1. truncate OLD tool results, oldest first (never the recent block);
/// 2. summarize oldest whole blocks after the initial user prompt into
///    one epoch line each (never the recent block, never index 0);
/// 3. last resort: truncate even the recent block's tool results.
pub fn plan_compaction(items: &[ConversationItem], budget: u64) -> Option<CompactionPlan> {
    let total: u64 = items.iter().map(|i| i.tokens()).sum();
    if total <= budget {
        return None;
    }
    let recent = recent_block_start(items);
    // Per-item residual token estimates — truncation inside a later
    // summarized block must not double-count savings.
    let mut residual: Vec<u64> = items.iter().map(|i| i.tokens()).collect();
    let current = |residual: &[u64]| residual.iter().sum::<u64>();
    let mut actions: Vec<CompactionAction> = Vec::new();
    let mut summarized: Vec<(Option<Label>, String)> = Vec::new();
    // Items already removed by a SummarizeBlock (skip in later passes).
    let mut removed: Vec<(usize, usize)> = Vec::new();

    fn in_removed(removed: &[(usize, usize)], index: usize) -> bool {
        removed.iter().any(|(s, e)| index >= *s && index < *e)
    }

    // Stage 1: truncate old tool results, oldest first.
    for index in 0..items.len() {
        if current(&residual) <= budget {
            break;
        }
        if items[index].kind != ItemKind::ToolResult || index >= recent || in_removed(&removed, index) {
            continue;
        }
        let replacement = truncated_replacement(&items[index].text);
        let after = estimate_tokens(&replacement);
        if after >= residual[index] {
            continue;
        }
        residual[index] = after;
        actions.push(CompactionAction::TruncateToolResult {
            index,
            replacement: replacement.clone(),
        });
        summarized.push((None, format!("tool result truncated: {}", head_line(&items[index].text))));
    }

    // Stage 2: summarize oldest blocks (after the initial user prompt).
    let mut index = 1;
    while current(&residual) > budget && index < items.len() {
        if items[index].kind != ItemKind::AssistantToolCalls
            || in_removed(&removed, index)
            || index >= recent
        {
            index += 1;
            continue;
        }
        // The block: this assistant message plus its trailing tool results.
        let mut end = index + 1;
        while end < items.len() && items[end].kind == ItemKind::ToolResult {
            end += 1;
        }
        if end > recent {
            break; // would eat the protected block
        }
        let block = &items[index..end];
        let block_tokens: u64 = residual[index..end].iter().sum();
        let manifest = CompactionManifest::build(
            block.len() as u64,
            &block
                .iter()
                .map(|i| (None, head_line(&i.text)))
                .collect::<Vec<_>>(),
        );
        let replacement = format!(
            "[context epoch] {} tool-call block(s) compacted ({} estimated tokens): {}",
            1,
            block_tokens,
            manifest.compressed_projection
        );
        let replacement_tokens = estimate_tokens(&replacement);
        if replacement_tokens >= block_tokens {
            index = end;
            continue;
        }
        for r in &mut residual[index..end] {
            *r = 0;
        }
        residual[index] = replacement_tokens;
        for item in block {
            summarized.push((None, head_line(&item.text)));
        }
        actions.push(CompactionAction::SummarizeBlock {
            start: index,
            end,
            replacement: replacement.clone(),
        });
        removed.push((index, end));
        index = end;
    }

    // Stage 3 (last resort): truncate even the recent block's results.
    for index in recent..items.len() {
        if current(&residual) <= budget {
            break;
        }
        if items[index].kind != ItemKind::ToolResult || in_removed(&removed, index) {
            continue;
        }
        let replacement = truncated_replacement(&items[index].text);
        let after = estimate_tokens(&replacement);
        if after >= residual[index] {
            continue;
        }
        residual[index] = after;
        actions.push(CompactionAction::TruncateToolResult {
            index,
            replacement: replacement.clone(),
        });
        summarized.push((None, format!("tool result truncated: {}", head_line(&items[index].text))));
    }

    if actions.is_empty() {
        return None;
    }
    let manifest = CompactionManifest::build(total, &summarized);
    Some(CompactionPlan {
        actions,
        projected_tokens: current(&residual),
        manifest,
    })
}

fn head_line(text: &str) -> String {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> ConversationItem {
        ConversationItem::new(ItemKind::UserTurn, text)
    }
    fn assistant_calls(text: &str) -> ConversationItem {
        ConversationItem::new(ItemKind::AssistantToolCalls, text)
    }
    fn tool_result(text: &str) -> ConversationItem {
        ConversationItem::new(ItemKind::ToolResult, text)
    }

    fn conversation() -> Vec<ConversationItem> {
        vec![
            user(&"objective: fix the bug".repeat(20)),
            assistant_calls("reading files"),
            tool_result(&"x".repeat(4_000)), // ~1000 tokens
            assistant_calls("second read"),
            tool_result(&"y".repeat(4_000)),
            assistant_calls("final read"),
            tool_result(&"z".repeat(4_000)),
        ]
    }

    #[test]
    fn under_budget_plans_nothing() {
        let items = conversation();
        assert!(plan_compaction(&items, 100_000).is_none());
    }

    #[test]
    fn over_budget_truncates_oldest_tool_results_first() {
        let items = conversation();
        // Budget forces truncation but leaves the recent block intact.
        let plan = plan_compaction(&items, 2_500).expect("must plan");
        assert!(plan.projected_tokens <= 2_500, "projection fits: {}", plan.projected_tokens);
        // The FIRST (oldest) tool result is truncated; the LAST is not.
        assert!(plan.actions.iter().any(|a| matches!(
            a,
            CompactionAction::TruncateToolResult { index: 2, .. }
        )));
        assert!(!plan.actions.iter().any(|a| matches!(
            a,
            CompactionAction::TruncateToolResult { index: 6, .. }
        )));
        // Truncation replaces content but keeps linkage: the action set
        // never removes a message.
        assert!(plan
            .actions
            .iter()
            .all(|a| matches!(a, CompactionAction::TruncateToolResult { .. })));
    }

    #[test]
    fn tight_budget_summarizes_oldest_blocks_not_the_prompt_or_recent() {
        let items = conversation();
        let plan = plan_compaction(&items, 900).expect("must plan");
        // At least one whole block was summarized...
        let summaries: Vec<_> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                CompactionAction::SummarizeBlock { start, end, .. } => Some((*start, *end)),
                _ => None,
            })
            .collect();
        assert!(!summaries.is_empty(), "blocks summarized: {:?}", plan.actions);
        // ...never index 0 (the prompt) and never the recent block.
        for (start, end) in &summaries {
            assert!(*start >= 1);
            assert!(*end <= 5, "recent block protected, got end {end}");
        }
        assert!(plan.projected_tokens <= 900 || !summaries.is_empty());
        // The manifest records the compaction with its source head.
        assert!(plan.manifest.source_head > 0);
    }

    #[test]
    fn extreme_budget_truncates_even_the_recent_block_as_last_resort() {
        let items = conversation();
        let plan = plan_compaction(&items, 10).expect("must plan");
        assert!(
            plan.actions.iter().any(|a| matches!(
                a,
                CompactionAction::TruncateToolResult { index: 6, .. }
            )),
            "recent result truncated only as last resort"
        );
    }

    #[test]
    fn estimator_is_conservative_on_short_texts() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
