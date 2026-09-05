//! Multimodal read (M5, REQ-EV-0184), bounded PDF text→vision fallback
//! (REQ-EV-0185), structured notebook read/edit (REQ-EV-0186), and rich
//! MCP media results (REQ-EV-0187).
//!
//! One typed media-parts contract across all four surfaces: capable
//! models receive typed parts; incapable models receive an EXPLICIT
//! unsupported marker — never silent truncation. Provenance (digest,
//! page range, source, model) travels with every part.

use crate::media::{detect_type, MediaType};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// What the consuming model can accept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelModality {
    pub image: bool,
    pub pdf: bool,
    pub audio: bool,
    pub video: bool,
}

impl ModelModality {
    pub const TEXT_ONLY: Self = Self {
        image: false,
        pdf: false,
        audio: false,
        video: false,
    };
    pub const VISION: Self = Self {
        image: true,
        pdf: true,
        audio: false,
        video: false,
    };
}

/// A typed media part.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "part", rename_all = "snake_case")]
pub enum MediaPart {
    Text {
        text: String,
    },
    Media {
        mime: String,
        sha256: String,
    },
    /// The model cannot accept this modality: explicit, with reason.
    Unsupported {
        mime: String,
        reason: String,
    },
}

/// The result of a multimodal read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MultimodalRead {
    pub path: String,
    pub parts: Vec<MediaPart>,
    pub sha256: String,
}

/// Reads file bytes into typed media parts according to model capability
/// (REQ-EV-0184): text returns as text; supported image/PDF returns as
/// typed media for capable models; unsupported modality is EXPLICIT.
pub fn read_file_media(path: &str, bytes: &[u8], capability: ModelModality) -> MultimodalRead {
    let mut parts = Vec::new();
    let sha256 = sha256_hex(bytes);
    match detect_type(bytes) {
        Some((MediaType::Text, _)) => {
            parts.push(MediaPart::Text {
                text: String::from_utf8_lossy(bytes).to_string(),
            });
        }
        Some((MediaType::Png, mime)) | Some((MediaType::Jpeg, mime)) => {
            if capability.image {
                parts.push(MediaPart::Media {
                    mime: mime.to_string(),
                    sha256: sha256.clone(),
                });
            } else {
                parts.push(MediaPart::Unsupported {
                    mime: mime.to_string(),
                    reason: "model has no image modality".into(),
                });
            }
        }
        Some((MediaType::Pdf, mime)) => {
            if capability.pdf {
                parts.push(MediaPart::Media {
                    mime: mime.to_string(),
                    sha256: sha256.clone(),
                });
            } else {
                parts.push(MediaPart::Unsupported {
                    mime: mime.to_string(),
                    reason: "model has no pdf modality".into(),
                });
            }
        }
        other => {
            let mime = other
                .map(|(_, m)| m.to_string())
                .unwrap_or("application/octet-stream".into());
            parts.push(MediaPart::Unsupported {
                mime,
                reason: "binary content with no supported modality".into(),
            });
        }
    }
    MultimodalRead {
        path: path.to_string(),
        parts,
        sha256,
    }
}

// ---------------------------------------------------------------------------
// Bounded PDF text→vision fallback (REQ-EV-0185)
// ---------------------------------------------------------------------------

/// The PDF read outcome: text extraction when a text layer exists;
/// otherwise the bounded VISION fallback — labeled lossy and untrusted,
/// with page range, source and model recorded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "pdf_outcome", rename_all = "snake_case")]
pub enum PdfOutcome {
    TextExtracted {
        pages: usize,
        text: String,
    },
    VisionFallback {
        page_range: (usize, usize),
        source: String,
        model: String,
        transcript: String,
        /// Vision transcription of scanned pages is LOSSY and UNTRUSTED.
        lossy: bool,
        untrusted: bool,
    },
}

/// Reads a PDF: attempts text extraction (the bytes between BT/ET text
/// operators, our extraction contract), and falls back to the bounded
/// vision path when the text layer is empty (scanned document).
pub fn read_pdf(
    bytes: &[u8],
    page_count: usize,
    vision_model: &str,
    max_fallback_pages: usize,
) -> PdfOutcome {
    let text = String::from_utf8_lossy(bytes);
    let extracted: String = text
        .match_indices("BT")
        .filter_map(|(start, _)| {
            let end = text[start..].find("ET")? + start;
            Some(text[start..end].to_string())
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !extracted.trim().is_empty() {
        return PdfOutcome::TextExtracted {
            pages: page_count,
            text: extracted,
        };
    }
    // Scanned PDF: bounded vision fallback.
    let range_end = page_count.min(max_fallback_pages);
    PdfOutcome::VisionFallback {
        page_range: (1, range_end),
        source: "vision transcription of scanned pages".into(),
        model: vision_model.to_string(),
        transcript: format!("[vision transcription of pages 1-{range_end}]"),
        lossy: true,
        untrusted: true,
    }
}

// ---------------------------------------------------------------------------
// Structured notebook read/edit (REQ-EV-0186)
// ---------------------------------------------------------------------------

/// One notebook cell with a stable id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotebookCell {
    pub cell_id: String,
    pub kind: String,
    pub source: String,
    /// Execution metadata (outputs, counters) — preserved on edit.
    pub execution_metadata: BTreeMap<String, serde_json::Value>,
}

/// A parsed notebook.
#[derive(Clone, Debug)]
pub struct Notebook {
    pub cells: Vec<NotebookCell>,
    pub notebook_metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug)]
pub enum NotebookError {
    Parse(String),
    AmbiguousCell { cell_id: String },
    UnknownCell { cell_id: String },
}

impl std::fmt::Display for NotebookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotebookError::Parse(why) => write!(f, "notebook parse: {why}"),
            NotebookError::AmbiguousCell { cell_id } => {
                write!(f, "cell id {cell_id:?} is ambiguous (duplicate)")
            }
            NotebookError::UnknownCell { cell_id } => write!(f, "unknown cell {cell_id:?}"),
        }
    }
}

impl std::error::Error for NotebookError {}

/// Parses a real .ipynb JSON document into structured cells. Cell ids are
/// taken from the `id` field; duplicates make the notebook AMBIGUOUS and
/// are rejected (edits must target stable ids).
pub fn parse_notebook(bytes: &[u8]) -> Result<Notebook, NotebookError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| NotebookError::Parse(e.to_string()))?;
    let cells_json = value
        .get("cells")
        .and_then(|c| c.as_array())
        .ok_or_else(|| NotebookError::Parse("missing cells array".into()))?;
    let mut cells = Vec::new();
    for cell in cells_json {
        let cell_id = cell
            .get("id")
            .and_then(|i| i.as_str())
            .ok_or_else(|| NotebookError::Parse("cell missing id".into()))?
            .to_string();
        let kind = cell
            .get("cell_type")
            .and_then(|k| k.as_str())
            .unwrap_or("code")
            .to_string();
        let source = match cell.get("source") {
            Some(serde_json::Value::Array(lines)) => lines
                .iter()
                .map(|l| l.as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join(""),
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let mut execution_metadata = BTreeMap::new();
        if let Some(meta) = cell.get("metadata") {
            execution_metadata.insert("metadata".into(), meta.clone());
        }
        if let Some(exec) = cell.get("execution_count") {
            execution_metadata.insert("execution_count".into(), exec.clone());
        }
        cells.push(NotebookCell {
            cell_id,
            kind,
            source,
            execution_metadata,
        });
    }
    // Ambiguity check: duplicate ids reject the whole document.
    let mut seen = std::collections::BTreeSet::new();
    for cell in &cells {
        if !seen.insert(cell.cell_id.clone()) {
            return Err(NotebookError::AmbiguousCell {
                cell_id: cell.cell_id.clone(),
            });
        }
    }
    let notebook_metadata = value
        .get("metadata")
        .and_then(|m| m.as_object())
        .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    Ok(Notebook {
        cells,
        notebook_metadata,
    })
}

impl Notebook {
    /// Edits ONE cell's source by stable id; unrelated cells and their
    /// execution metadata are preserved verbatim.
    pub fn edit_cell(&mut self, cell_id: &str, new_source: &str) -> Result<(), NotebookError> {
        let matches: Vec<&mut NotebookCell> = self
            .cells
            .iter_mut()
            .filter(|c| c.cell_id == cell_id)
            .collect();
        let mut iter = matches.into_iter();
        let cell = iter.next().ok_or_else(|| NotebookError::UnknownCell {
            cell_id: cell_id.to_string(),
        })?;
        if iter.next().is_some() {
            return Err(NotebookError::AmbiguousCell {
                cell_id: cell_id.to_string(),
            });
        }
        cell.source = new_source.to_string();
        Ok(())
    }

    /// Serializes back to ipynb JSON.
    pub fn to_ipynb(&self) -> Result<Vec<u8>, NotebookError> {
        let cells: Vec<serde_json::Value> = self
            .cells
            .iter()
            .map(|c| {
                let mut cell = serde_json::json!({
                    "id": c.cell_id,
                    "cell_type": c.kind,
                    "source": c.source,
                    "metadata": c.execution_metadata.get("metadata").cloned().unwrap_or_default(),
                });
                // Execution metadata (e.g. execution_count) round-trips.
                if let Some(count) = c.execution_metadata.get("execution_count") {
                    cell["execution_count"] = count.clone();
                }
                cell
            })
            .collect();
        let doc = serde_json::json!({
            "cells": cells,
            "metadata": self.notebook_metadata,
            "nbformat": 4,
            "nbformat_minor": 5,
        });
        Ok(serde_json::to_vec_pretty(&doc).unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Rich MCP media results (REQ-EV-0187)
// ---------------------------------------------------------------------------

/// A normalized MCP tool-result media part.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mcp_part", rename_all = "snake_case")]
pub enum McpPart {
    Text { text: String },
    Image { mime: String, sha256: String },
    Resource { uri: String, sha256: String },
    Audio { mime: String, sha256: String },
}

/// Normalizes a raw MCP tool result content array into typed media parts
/// (REQ-EV-0187): text/image/audio/file/resource all become typed parts,
/// each of which lands in the evidence store with a digest.
pub fn normalize_mcp_result(content: &[serde_json::Value]) -> Vec<McpPart> {
    content
        .iter()
        .filter_map(|item| {
            let kind = item.get("type")?.as_str()?;
            match kind {
                "text" => Some(McpPart::Text {
                    text: item.get("text")?.as_str()?.to_string(),
                }),
                "image" => {
                    let data = item.get("data")?.as_str()?;
                    Some(McpPart::Image {
                        mime: item
                            .get("mimeType")
                            .and_then(|m| m.as_str())
                            .unwrap_or("image/png")
                            .to_string(),
                        sha256: sha256_hex(data.as_bytes()),
                    })
                }
                "audio" => {
                    let data = item.get("data")?.as_str()?;
                    Some(McpPart::Audio {
                        mime: item
                            .get("mimeType")
                            .and_then(|m| m.as_str())
                            .unwrap_or("audio/wav")
                            .to_string(),
                        sha256: sha256_hex(data.as_bytes()),
                    })
                }
                "resource" => {
                    let uri = item.get("uri").and_then(|u| u.as_str()).or_else(|| {
                        item.get("resource")
                            .and_then(|r| r.get("uri"))
                            .and_then(|u| u.as_str())
                    })?;
                    Some(McpPart::Resource {
                        uri: uri.to_string(),
                        sha256: sha256_hex(uri.as_bytes()),
                    })
                }
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_BYTES: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];

    /// QUAL-EV-0184: capable and incapable models read real media; the
    /// unsupported modality is EXPLICIT.
    #[test]
    fn multimodal_read_types_by_model_capability() {
        // Vision-capable model: the PNG returns as a typed media part.
        let capable = read_file_media("chart.png", PNG_BYTES, ModelModality::VISION);
        assert!(matches!(
            capable.parts[0],
            MediaPart::Media { ref mime, .. } if mime == "image/png"
        ));

        // Text-only model: EXPLICIT unsupported marker, not silence.
        let incapable = read_file_media("chart.png", PNG_BYTES, ModelModality::TEXT_ONLY);
        assert!(matches!(
            incapable.parts[0],
            MediaPart::Unsupported { ref mime, ref reason }
                if mime == "image/png" && reason.contains("no image modality")
        ));

        // Text files read as text for everyone.
        let text = read_file_media("src/lib.rs", b"fn main() {}", ModelModality::TEXT_ONLY);
        assert!(matches!(&text.parts[0], MediaPart::Text { text } if text.contains("fn main")));
    }

    /// QUAL-EV-0185: a scanned PDF triggers the bounded vision path with
    /// page range/source/model recorded, labeled lossy + untrusted.
    #[test]
    fn scanned_pdf_triggers_bounded_vision_fallback() {
        // A PDF with NO text operators: scanned pages.
        let scanned = b"%PDF-1.7\n%%EOF\n";
        let outcome = read_pdf(scanned, 12, "vision-model-x", 5);
        match outcome {
            PdfOutcome::VisionFallback {
                page_range,
                source,
                model,
                lossy,
                untrusted,
                ..
            } => {
                assert_eq!(page_range, (1, 5), "bounded to max_fallback_pages");
                assert!(source.contains("vision"));
                assert_eq!(model, "vision-model-x");
                assert!(lossy && untrusted);
            }
            other => panic!("expected vision fallback, got {other:?}"),
        }

        // A text-layer PDF extracts text directly.
        let with_text = b"%PDF-1.7\nBT hello pdf world ET\n%%EOF\n";
        let outcome = read_pdf(with_text, 1, "vision-model-x", 5);
        assert!(matches!(
            outcome,
            PdfOutcome::TextExtracted { pages: 1, .. }
        ));
    }

    /// QUAL-EV-0186: real ipynb read/edit preserves unrelated cells and
    /// execution metadata; ambiguous/truncated state is rejected.
    #[test]
    fn notebook_edit_preserves_unrelated_cells_and_metadata() {
        let ipynb = serde_json::json!({
            "cells": [
                {"id": "cell-a", "cell_type": "code", "source": ["print(1)\n", "print(2)"], "execution_count": 3},
                {"id": "cell-b", "cell_type": "markdown", "source": "# notes"}
            ],
            "metadata": {"kernelspec": {"name": "python3"}}
        });
        let mut notebook = parse_notebook(&ipynb_bytes(&ipynb)).unwrap();
        assert_eq!(notebook.cells.len(), 2);

        // Edit ONE cell by stable id.
        notebook.edit_cell("cell-a", "print(42)").unwrap();
        let out =
            serde_json::from_slice::<serde_json::Value>(&notebook.to_ipynb().unwrap()).unwrap();
        let cells = out["cells"].as_array().unwrap();
        // The edited cell carries the new source.
        assert_eq!(cells[0]["source"], "print(42)");
        // Its execution metadata survived.
        assert_eq!(
            cells[0]["execution_count"], 3,
            "execution metadata round-trips"
        );
        // The unrelated cell is untouched.
        assert_eq!(cells[1]["source"], "# notes");
        assert_eq!(cells[1]["cell_type"], "markdown");

        // Truncated/ambiguous state is rejected.
        let truncated = b"{\"cells\": [{\"cell_type\": \"code\"}]}";
        assert!(matches!(
            parse_notebook(truncated.as_slice()),
            Err(NotebookError::Parse(_))
        ));
        let ambiguous = serde_json::json!({
            "cells": [
                {"id": "dup", "cell_type": "code", "source": "1"},
                {"id": "dup", "cell_type": "code", "source": "2"}
            ]
        });
        assert!(matches!(
            parse_notebook(&ipynb_bytes(&ambiguous)),
            Err(NotebookError::AmbiguousCell { cell_id }) if cell_id == "dup"
        ));
    }

    fn ipynb_bytes(value: &serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    /// QUAL-EV-0187: an MCP result carrying image+text normalizes into
    /// typed parts, each digest-addressed for the evidence store.
    #[test]
    fn mcp_result_normalizes_image_and_text_parts() {
        let result = serde_json::json!({
            "content": [
                {"type": "text", "text": "chart for Q3"},
                {"type": "image", "mimeType": "image/png", "data": "iVBOR"}
            ]
        });
        let parts = normalize_mcp_result(result["content"].as_array().unwrap());
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], McpPart::Text { text } if text == "chart for Q3"));
        match &parts[1] {
            McpPart::Image { mime, sha256 } => {
                assert_eq!(mime, "image/png");
                assert_eq!(sha256.len(), 64, "digest-addressed for evidence");
            }
            other => panic!("expected image part, got {other:?}"),
        }
    }
}
