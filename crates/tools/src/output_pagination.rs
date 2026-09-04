//! OutputRef tool pagination (M2, REQ-EV-0269): large tool outputs return
//! a bounded preview plus a durable, range-addressable OutputRef. Pages
//! carry per-page digests, and the page chain digests back to the FULL
//! output — paging without context overflow, integrity without trust.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Hard cap on any single page's bytes (the model-context budget).
pub const PAGE_MAX_BYTES: usize = 256 * 1024;

/// Durable, range-addressable reference to a full tool output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputRef {
    pub output_id: String,
    /// SHA-256 over the ENTIRE raw output — paging must reproduce this.
    pub total_sha256: String,
    pub total_bytes: usize,
    /// The bounded preview (first bytes) that travels in the transcript.
    pub preview: String,
    pub preview_bytes: usize,
}

/// One page of the output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputPage {
    pub output_id: String,
    pub offset: u64,
    pub data: Vec<u8>,
    /// Digest over THIS page's bytes.
    pub page_sha256: String,
    /// True when offset+len == total (the final page).
    pub is_final: bool,
}

#[derive(Debug)]
pub enum PageError {
    OutOfRange {
        offset: u64,
        total: usize,
    },
    /// A page failed its digest check — refuse rather than corrupt.
    PageDigestMismatch {
        output_id: String,
        offset: u64,
    },
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageError::OutOfRange { offset, total } => {
                write!(f, "offset {offset} beyond output size {total}")
            }
            PageError::PageDigestMismatch { output_id, offset } => {
                write!(f, "page digest mismatch in {output_id} at offset {offset}")
            }
        }
    }
}

impl std::error::Error for PageError {}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Wraps a (possibly huge) tool output into a bounded preview + OutputRef.
pub fn make_output_ref(output_id: &str, raw: &[u8], preview_cap: usize) -> OutputRef {
    let preview_len = preview_cap.min(raw.len());
    OutputRef {
        output_id: output_id.to_string(),
        total_sha256: sha256_hex(raw),
        total_bytes: raw.len(),
        preview: String::from_utf8_lossy(&raw[..preview_len]).to_string(),
        preview_bytes: preview_len,
    }
}

/// Slices the next page from the raw output at `offset`. In production the
/// raw bytes live behind the OutputRef (artifact store / broker log); the
/// slice is the same range-addressable operation either way.
pub fn next_page(
    output_id: &str,
    raw: &[u8],
    offset: u64,
    max_bytes: usize,
) -> Result<OutputPage, PageError> {
    let total = raw.len();
    if offset as usize >= total {
        return Err(PageError::OutOfRange { offset, total });
    }
    let start = offset as usize;
    let end = total.min(start + max_bytes.min(PAGE_MAX_BYTES));
    let data = raw[start..end].to_vec();
    Ok(OutputPage {
        output_id: output_id.to_string(),
        offset,
        page_sha256: sha256_hex(&data),
        is_final: end == total,
        data,
    })
}

/// Verifies a page against the expected bytes at its offset (typed
/// integrity check; QUAL: digest matches raw output).
pub fn verify_page(page: &OutputPage, expected_bytes: &[u8]) -> Result<(), PageError> {
    if sha256_hex(&page.data) != page.page_sha256 || page.data != expected_bytes {
        return Err(PageError::PageDigestMismatch {
            output_id: page.output_id.clone(),
            offset: page.offset,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn huge_output(mib: usize) -> Vec<u8> {
        // Deterministic 10MB+ tool result: repeated structured lines.
        let line = format!(
            "{}: event payload with padding padding padding\n",
            "x".repeat(64)
        );
        // One line is ~100 bytes; 1024 lines ≈ 100KB; 16 units per MiB target.
        let unit = line.repeat(1024 * 16);
        unit.repeat(mib).into()
    }

    /// QUAL-EV-0269: a 10MB+ tool result is paged without context
    /// overflow and the digest of the paged chain matches the raw output.
    #[test]
    fn ten_megabyte_result_pages_with_matching_digest() {
        let raw = huge_output(11); // ~11 MiB
        assert!(raw.len() > 10 * 1024 * 1024);

        // The transcript carries ONLY the bounded preview + ref.
        let output_ref = make_output_ref("out-1", &raw, 512);
        assert_eq!(output_ref.total_bytes, raw.len());
        assert_eq!(output_ref.preview_bytes, 512);
        assert!(output_ref.preview.len() <= 512);

        // Page through everything with bounded reads; reassemble.
        let mut offset = 0u64;
        let mut reassembled = Vec::with_capacity(raw.len());
        let mut pages = 0usize;
        while offset < raw.len() as u64 {
            let page = next_page("out-1", &raw, offset, PAGE_MAX_BYTES).unwrap();
            assert!(page.data.len() <= PAGE_MAX_BYTES);
            verify_page(
                &page,
                &raw[page.offset as usize..page.offset as usize + page.data.len()],
            )
            .unwrap();
            reassembled.extend_from_slice(&page.data);
            offset += page.data.len() as u64;
            pages += 1;
            if page.is_final {
                break;
            }
        }
        assert!(pages >= 40, "a huge output really is split into many pages");
        // The paged chain digests to the FULL output digest.
        assert_eq!(sha256_hex(&reassembled), output_ref.total_sha256);
        assert_eq!(reassembled.len(), raw.len());

        // Out-of-range offsets are typed errors, not panics.
        assert!(matches!(
            next_page("out-1", &raw, raw.len() as u64, 1024),
            Err(PageError::OutOfRange { .. })
        ));

        // A corrupted page is caught by its digest.
        let mut page = next_page("out-1", &raw, 0, 1024).unwrap();
        page.data[0] ^= 0xFF;
        assert!(verify_page(&page, &raw[..1024]).is_err());
    }
}
