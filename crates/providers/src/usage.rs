//! Provider-neutral usage snapshot (docs/15 § ModelEvent::Usage). Lives at
//! the crate root so the transport, the parsers and the runtime share one
//! definition.

/// Token usage as reported by the provider (cache-tier detail arrives with
/// the observability slice).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}
