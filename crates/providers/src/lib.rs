//! modbit-providers — normalized model/embedding adapters
//!
//! Canonical owner subsystem: model-gateway (docs/81). Layout: docs/12_REPOSITORY_AND_MODULE_LAYOUT.md.
pub mod gateway;
pub use gateway::{
    anthropic_request_body, openai_request_body, parse_anthropic_sse_payload,
    parse_openai_sse_payload, sse_data_line, test_request, ChatMessage, ModelRequest, Role,
    StreamEvent,
};
