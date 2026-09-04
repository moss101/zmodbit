//! Channel ingestion → canonical MediaEnvelope (M1, REQ-EV-0190; MOD-MEDIA-
//! 001). Every image/file attachment — desktop upload or API — normalizes to
//! the same typed envelope with tenant/task/source provenance. There is no
//! consumer chat-product scope here.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ids::{SessionId, TaskId, TenantId};

/// Where the attachment entered the system (provenance).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestSource {
    DesktopUpload,
    Api,
}

/// Hard ingestion ceiling (docs/25 § media budgets; bounded, not unlimited).
pub const MAX_INGEST_BYTES: usize = 100 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaEnvelope {
    pub media_id: String,
    pub tenant_id: TenantId,
    /// Provenance: the task this attachment belongs to, when task-scoped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub source: IngestSource,
    pub content_type: String,
    pub byte_length: usize,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngestError {
    Empty,
    TooLarge { size: usize, max: usize },
    UnknownContentType(String),
    BadIds(String),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::Empty => write!(f, "attachment is empty"),
            IngestError::TooLarge { size, max } => {
                write!(f, "attachment {size} bytes exceeds {max}")
            }
            IngestError::UnknownContentType(ct) => write!(f, "unknown content type {ct:?}"),
            IngestError::BadIds(e) => write!(f, "bad id: {e}"),
        }
    }
}

impl std::error::Error for IngestError {}

const KNOWN_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "application/pdf",
    "text/plain",
    "application/octet-stream",
];

/// Normalizes a raw attachment into the canonical envelope. The same bytes,
/// tenant and content type produce the same envelope regardless of channel —
/// only `source` differs (QUAL-EV-0190).
pub fn normalize_upload(
    media_id: String,
    tenant_id: TenantId,
    task_id: Option<TaskId>,
    session_id: Option<SessionId>,
    source: IngestSource,
    content_type: &str,
    bytes: &[u8],
) -> Result<MediaEnvelope, IngestError> {
    if bytes.is_empty() {
        return Err(IngestError::Empty);
    }
    if bytes.len() > MAX_INGEST_BYTES {
        return Err(IngestError::TooLarge {
            size: bytes.len(),
            max: MAX_INGEST_BYTES,
        });
    }
    if !KNOWN_TYPES.contains(&content_type) {
        return Err(IngestError::UnknownContentType(content_type.to_string()));
    }
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let sha256 = format!("{:x}", hasher.finalize());
    Ok(MediaEnvelope {
        media_id,
        tenant_id,
        task_id,
        session_id,
        source,
        content_type: content_type.to_string(),
        byte_length: bytes.len(),
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake-image-bytes";

    fn tenant() -> TenantId {
        TenantId::generate()
    }

    /// QUAL-EV-0190: desktop upload and API upload of the same attachment
    /// normalize to the same canonical envelope (source differs only).
    #[test]
    fn same_bytes_through_desktop_and_api_share_the_canonical_hash() {
        let via_desktop = normalize_upload(
            "m-1".into(),
            tenant(),
            None,
            None,
            IngestSource::DesktopUpload,
            "image/png",
            PNG,
        )
        .unwrap();
        let via_api = normalize_upload(
            "m-2".into(),
            tenant(),
            None,
            None,
            IngestSource::Api,
            "image/png",
            PNG,
        )
        .unwrap();
        assert_eq!(via_desktop.sha256, via_api.sha256);
        assert_eq!(via_desktop.byte_length, via_api.byte_length);
        assert_eq!(via_desktop.content_type, via_api.content_type);
        assert_ne!(via_desktop.source, via_api.source);
    }

    #[test]
    fn provenance_is_recorded() {
        let tenant = tenant();
        let task = TaskId::generate();
        let envelope = normalize_upload(
            "m-3".into(),
            tenant,
            Some(task),
            None,
            IngestSource::DesktopUpload,
            "image/png",
            PNG,
        )
        .unwrap();
        assert_eq!(envelope.task_id, Some(task));
        assert_eq!(envelope.source, IngestSource::DesktopUpload);
    }

    #[test]
    fn empty_and_oversized_and_unknown_types_are_rejected() {
        assert_eq!(
            normalize_upload(
                "m".into(),
                tenant(),
                None,
                None,
                IngestSource::DesktopUpload,
                "image/png",
                &[]
            ),
            Err(IngestError::Empty)
        );
        let big = vec![0u8; MAX_INGEST_BYTES + 1];
        assert!(matches!(
            normalize_upload(
                "m".into(),
                tenant(),
                None,
                None,
                IngestSource::Api,
                "image/png",
                &big
            ),
            Err(IngestError::TooLarge { .. })
        ));
        assert!(matches!(
            normalize_upload(
                "m".into(),
                tenant(),
                None,
                None,
                IngestSource::Api,
                "video/x-odd",
                PNG
            ),
            Err(IngestError::UnknownContentType(_))
        ));
    }
}
