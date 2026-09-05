//! Split tool media for provider compliance (M2, REQ-EV-0188): media never
//! travels inline inside tool results. The canonical ModelEvent stays
//! provider-neutral; the provider adapter splits embedded media out into
//! refs without losing semantics. The strict OpenAI-compatible contract
//! REJECTS bodies with embedded media and ACCEPTS the split follow-up.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A provider-neutral media reference: identity + digest, never bytes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaRef {
    pub media_id: String,
    pub mime: String,
    /// SHA-256 of the raw bytes — the media itself lives in the artifact
    /// store, addressed by this digest.
    pub sha256: String,
    pub byte_length: usize,
}

#[derive(Debug)]
pub enum MediaSplitError {
    /// Embedded media detected where the strict contract forbids it.
    EmbeddedMedia {
        context: &'static str,
        offset: usize,
    },
    /// A media payload failed to decode — cannot be safely referenced.
    Undecodable { media_id: String },
}

impl std::fmt::Display for MediaSplitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaSplitError::EmbeddedMedia { context, offset } => {
                write!(f, "embedded media in {context} at byte {offset}")
            }
            MediaSplitError::Undecodable { media_id } => {
                write!(f, "media {media_id} is not decodable base64")
            }
        }
    }
}

impl std::error::Error for MediaSplitError {}

fn media_id_for(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("media-{:x}", hasher.finalize())[..24].to_string()
}

/// Splits data-URI media (`data:<mime>;base64,<payload>`) out of tool
/// result content. Returns the sanitized text (placeholders reference the
/// split media) plus the typed refs. Semantics are preserved: each
/// placeholder maps 1:1 to a ref, in order.
pub fn split_embedded_media(
    context: &'static str,
    content: &str,
) -> Result<(String, Vec<MediaRef>), MediaSplitError> {
    let mut refs = Vec::new();
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    loop {
        let Some(start) = rest.find("data:") else {
            out.push_str(rest);
            break;
        };
        let Some((mime, after_mime)) = rest[start..].split_once(';') else {
            out.push_str(rest);
            break;
        };
        let mime = mime.trim_start_matches("data:");
        let Some((payload, tail)) = after_mime.strip_prefix("base64,").map(|p| {
            p.split_once([',', ' ', '"', ')'])
                .map_or((p, ""), |(payload, tail)| (payload, tail))
        }) else {
            out.push_str(rest);
            break;
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload.trim())
            .map_err(|_| MediaSplitError::Undecodable {
                media_id: "unparsed".to_string(),
            })?;
        let media_id = media_id_for(&bytes);
        refs.push(MediaRef {
            media_id: media_id.clone(),
            mime: mime.to_string(),
            sha256: {
                let mut hasher = Sha256::new();
                hasher.update(&bytes);
                format!("{:x}", hasher.finalize())
            },
            byte_length: bytes.len(),
        });
        out.push_str(&rest[..start]);
        out.push_str(&format!("[media: {media_id}]"));
        rest = &rest[start + ("data:".len() + mime.len() + ";base64,".len() + payload.len())..];
        let _ = tail;
    }
    let _ = context;
    Ok((out, refs))
}

/// Strict OpenAI-compatible validation of a serialized request body: tool
/// results (and message contents) must NOT carry embedded data-URI media.
/// The split representation — plain text placeholders plus separate media
/// refs — passes (REQ-EV-0188 QUAL).
pub fn openai_strict_validate(body: &serde_json::Value) -> Result<(), MediaSplitError> {
    let Some(messages) = body.get("messages").and_then(|m| m.as_array()) else {
        return Ok(());
    };
    for (index, message) in messages.iter().enumerate() {
        let Some(content) = message.get("content").and_then(|c| c.as_str()) else {
            continue;
        };
        if let Some(offset) = content.find("data:") {
            if content[offset..].contains(";base64,") {
                return Err(MediaSplitError::EmbeddedMedia {
                    context: Box::leak(format!("messages[{index}].content").into_boxed_str()),
                    offset,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::openai_request_body;
    use crate::gateway::{ChatMessage, ModelRequest};
    use std::collections::BTreeMap;

    fn request_with_tool_result(content: &str) -> ModelRequest {
        ModelRequest {
            request_id: "req-media".into(),
            model: "gpt-5".into(),
            system: String::new(),
            messages: vec![ChatMessage::user(content)],
            max_output_tokens: 256,
            temperature: 0.2,
        reasoning_effort: None,
            tools: Vec::new(),
        }
    }

    /// QUAL-EV-0188: the strict OpenAI-compatible test rejects embedded
    /// media but passes the split follow-up representation — with
    /// semantics preserved (1:1 placeholders ↔ refs, stable digests).
    #[test]
    fn strict_openai_rejects_embedded_media_and_accepts_split() {
        // PNG header + tiny payload, base64-encoded data URI.
        let png_bytes: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes);
        let embedded = format!("Tool output: chart below data:image/png;base64,{b64} end.");

        // 1. Embedded form: the strict validator REJECTS the body.
        let body = openai_request_body(&request_with_tool_result(&embedded));
        assert!(
            openai_strict_validate(&body).is_err(),
            "embedded media must be rejected"
        );

        // 2. Split form: media becomes refs + placeholder text.
        let (clean, refs) = split_embedded_media("tool_result", &embedded).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].mime, "image/png");
        assert_eq!(refs[0].byte_length, png_bytes.len());
        assert!(refs[0].media_id.len() == 24);
        assert!(clean.contains("[media: "));
        assert!(!clean.contains("base64"));

        // The split body PASSES the strict validator.
        let split_body = openai_request_body(&request_with_tool_result(&clean));
        assert!(openai_strict_validate(&split_body).is_ok());

        // Semantics preserved: re-splits are stable (deterministic ids).
        let (clean_again, refs_again) = split_embedded_media("tool_result", &embedded).unwrap();
        assert_eq!(clean, clean_again);
        assert_eq!(refs, refs_again);
        let _ = BTreeMap::<String, String>::new();
    }

    /// Non-media content passes through untouched with zero refs.
    #[test]
    fn plain_content_is_untouched() {
        let (clean, refs) = split_embedded_media("tool_result", "SELECT 1; rows: 42").unwrap();
        assert_eq!(clean, "SELECT 1; rows: 42");
        assert!(refs.is_empty());
    }
}
