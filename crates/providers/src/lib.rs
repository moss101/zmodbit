//! modbit-providers — normalized model/embedding adapters
//!
//! Canonical owner subsystem: model-gateway (docs/81). Layout: docs/12_REPOSITORY_AND_MODULE_LAYOUT.md.
pub mod envelope;
pub mod gateway;
pub mod media_split;
pub mod profiles;
pub mod routing;
pub mod transport;
pub mod usage;
pub use gateway::{
    anthropic_request_body, extract_usage_frame, openai_request_body, parse_anthropic_sse_payload,
    parse_openai_sse_payload, sse_data_line, test_request, ChatMessage, ToolCallAssembler,
    ToolCallData, ToolDefinition, ModelRequest, Role, StreamEvent, UsageFrame,
};
pub use usage::TokenUsage;
pub use transport::{
    EnvSecretBroker, EventStream, HttpStreamTransport, ModelTransport, OutgoingRequest, RetryPolicy,
    SecretBroker, TransportError, TransportEvent,
};
pub use routing::{
    fallback_chain, route, EnterprisePolicy, ModelCapability, RouteDecision, TaskFingerprint,
};
