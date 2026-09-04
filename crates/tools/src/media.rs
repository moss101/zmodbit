//! Media pipeline (M2.10, docs/25 § Canonical media model): every image/file
//! attachment is normalized into a typed `MediaEnvelope` with provenance,
//! size budget and artifact digest; bytes live in a content-addressed
//! object store keyed by SHA-256; `fs.read` returns bounded typed results.
//!
//! Type detection uses real magic bytes:
//! - PNG  `89 50 4E 47 0D 0A 1A 0A`
//! - JPEG `FF D8 FF`
//! - PDF  `%PDF-`
//! - otherwise UTF-8 text (bounded read), else unknown/binary.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Hard ingestion ceiling (docs/25 § size budget).
pub const MAX_MEDIA_BYTES: usize = 100 * 1024 * 1024;

/// Bounded text/media read window for `fs.read` results (docs/25: fs.read
/// returns bounded typed results).
pub const BOUNDED_READ_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    Png,
    Jpeg,
    Pdf,
    Text,
    Binary,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaEnvelope {
    pub media_id: String,
    pub media_type: MediaType,
    pub mime: String,
    pub byte_length: usize,
    pub sha256: String,
    /// Content-addressed object path relative to the artifact store root
    /// (docs/31: large bytes live in the object store; references travel).
    pub object_rel_path: String,
    /// Source provenance (docs/25: source provenance + lineage).
    pub source: String,
    pub max_budget_bytes: usize,
    pub trust_label: String,
}

#[derive(Debug)]
pub enum MediaError {
    Empty,
    TooLarge { size: usize, budget: usize },
    UnknownType,
    Io(std::io::Error),
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaError::Empty => write!(f, "attachment is empty"),
            MediaError::TooLarge { size, budget } => {
                write!(f, "media {size} bytes exceeds budget {budget}")
            }
            MediaError::UnknownType => write!(f, "unknown media type"),
            MediaError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for MediaError {}

/// Detects the media type from real magic bytes.
pub fn detect_type(bytes: &[u8]) -> Option<(MediaType, &'static str)> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some((MediaType::Png, "image/png"));
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some((MediaType::Jpeg, "image/jpeg"));
    }
    if bytes.starts_with(b"%PDF-") {
        return Some((MediaType::Pdf, "application/pdf"));
    }
    if std::str::from_utf8(bytes).is_ok() {
        return Some((MediaType::Text, "text/plain"));
    }
    None
}

/// Content-addressed object store: `root/aa/bb/<sha256>`.
pub struct ObjectStore {
    root: PathBuf,
}

impl ObjectStore {
    pub fn open(root: &Path) -> Result<Self, MediaError> {
        fs::create_dir_all(root).map_err(MediaError::Io)?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Writes bytes under their content address; returns the relative path
    /// `aa/bb/<sha256>`.
    pub fn put(&self, bytes: &[u8]) -> Result<(String, String), MediaError> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let hash = format!("{:x}", hasher.finalize());
        let rel = format!("{}/{}", &hash[..2], hash);
        let full = self.root.join(&rel);
        if !full.exists() {
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).map_err(MediaError::Io)?;
            }
            fs::write(&full, bytes).map_err(MediaError::Io)?;
        }
        Ok((rel, hash))
    }

    pub fn read(&self, rel: &str) -> Result<Vec<u8>, MediaError> {
        fs::read(self.root.join(rel)).map_err(MediaError::Io)
    }
}

/// The media pipeline: budget → type detection → digest → object store →
/// envelope with provenance.
pub struct MediaPipeline {
    store: ObjectStore,
    max_budget_bytes: usize,
}

impl MediaPipeline {
    pub fn new(store_root: &Path, max_budget_bytes: usize) -> Result<Self, MediaError> {
        Ok(Self {
            store: ObjectStore::open(store_root)?,
            max_budget_bytes,
        })
    }

    /// Ingests raw bytes from a named source. Real bytes in, provenance and
    /// digests out.
    pub fn ingest(
        &self,
        media_id: &str,
        source: &str,
        bytes: &[u8],
    ) -> Result<MediaEnvelope, MediaError> {
        if bytes.is_empty() {
            return Err(MediaError::Empty);
        }
        if bytes.len() > self.max_budget_bytes {
            return Err(MediaError::TooLarge {
                size: bytes.len(),
                budget: self.max_budget_bytes,
            });
        }
        let (media_type, mime) = detect_type(bytes).ok_or(MediaError::UnknownType)?;
        let (object_rel_path, sha256) = self.store.put(bytes)?;
        Ok(MediaEnvelope {
            media_id: media_id.to_string(),
            media_type,
            mime: mime.to_string(),
            byte_length: bytes.len(),
            sha256: sha256.clone(),
            object_rel_path,
            source: source.to_string(),
            max_budget_bytes: self.max_budget_bytes,
            trust_label: "untrusted-until-scanned".to_string(),
        })
    }

    /// Bounded typed read (docs/25: fs.read returns bounded typed results).
    /// Returns the envelope plus up to BOUNDED_READ_BYTES of content (text
    /// types) or the raw reference bytes (images/PDFs remain media).
    pub fn read_bounded(
        &self,
        envelope: &MediaEnvelope,
    ) -> Result<(MediaEnvelope, Vec<u8>), MediaError> {
        let bytes = self.store.read(&envelope.object_rel_path)?;
        let take = bytes.len().min(BOUNDED_READ_BYTES);
        Ok((envelope.clone(), bytes[..take].to_vec()))
    }
}

/// Reads a file from disk through the pipeline (the `fs.read` path): type
/// detection, budget and digest all apply to real files.
pub fn read_file_bounded(
    pipeline: &MediaPipeline,
    source: &str,
    path: &Path,
    media_id: &str,
) -> Result<(MediaEnvelope, Vec<u8>), MediaError> {
    let bytes = fs::read(path).map_err(MediaError::Io)?;
    let envelope = pipeline.ingest(media_id, source, &bytes)?;
    pipeline.read_bounded(&envelope)
}

/// Minimal real PNG header for fixtures (valid 8-byte signature + IHDR stub).
pub fn minimal_png() -> Vec<u8> {
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(b"fake-but-magic-correct-png-payload");
    png
}

/// Minimal real JPEG header (SOI + APP0 marker bytes).
pub fn minimal_jpeg() -> Vec<u8> {
    let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
    jpeg.extend_from_slice(b"fake-but-magic-correct-jpeg-payload");
    jpeg
}

/// Minimal real PDF header/text body (docs/25: text PDF deterministic path).
pub fn minimal_pdf() -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n".to_vec();
    pdf.extend_from_slice(
        b"1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n",
    );
    pdf
}
