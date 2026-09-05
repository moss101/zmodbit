//! ReadMediaFile with per-modality budgets (M5, REQ-EV-0223): media
//! reads are bounded per modality — image/pdf/audio/video have distinct
//! caps, and oversized media is refused with the budget recorded.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Per-modality byte budgets.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaBudgets {
    pub image: usize,
    pub pdf: usize,
    pub audio: usize,
    pub video: usize,
}

impl Default for MediaBudgets {
    fn default() -> Self {
        Self {
            image: 5 * 1024 * 1024,
            pdf: 20 * 1024 * 1024,
            audio: 10 * 1024 * 1024,
            video: 50 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub enum MediaReadError {
    Oversized {
        mime: String,
        bytes: usize,
        budget: usize,
    },
    UnknownModality,
}

impl fmt::Display for MediaReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaReadError::Oversized {
                mime,
                bytes,
                budget,
            } => {
                write!(f, "{mime:?} read of {bytes}B exceeds the {budget}B budget")
            }
            MediaReadError::UnknownModality => write!(f, "unknown media modality"),
        }
    }
}

/// The mime → budget-class mapping.
fn budget_for(budgets: &MediaBudgets, mime: &str) -> Option<usize> {
    if mime.starts_with("image/") {
        Some(budgets.image)
    } else if mime == "application/pdf" {
        Some(budgets.pdf)
    } else if mime.starts_with("audio/") {
        Some(budgets.audio)
    } else if mime.starts_with("video/") {
        Some(budgets.video)
    } else {
        None
    }
}

/// Reads media under the per-modality budget: oversized reads are
/// refused with the budget recorded (REQ-EV-0223).
pub fn read_media_file(
    path: &str,
    mime: &str,
    bytes: &[u8],
    budgets: &MediaBudgets,
) -> Result<(String, usize), MediaReadError> {
    let budget = budget_for(budgets, mime).ok_or(MediaReadError::UnknownModality)?;
    if bytes.len() > budget {
        return Err(MediaReadError::Oversized {
            mime: mime.to_string(),
            bytes: bytes.len(),
            budget,
        });
    }
    Ok((path.to_string(), bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Media budgets: per-modality caps with recorded refusals.
    #[test]
    fn media_reads_respect_per_modality_budgets() {
        let budgets = MediaBudgets::default();

        // Within budget: reads fine.
        let (path, len) = read_media_file("a.png", "image/png", &[0u8; 1024], &budgets).unwrap();
        assert_eq!(path, "a.png");
        assert_eq!(len, 1024);

        // Image over the IMAGE budget (but under the video budget):
        // refused with the image budget recorded.
        let err =
            read_media_file("big.png", "image/png", &[0u8; 6 * 1024 * 1024], &budgets).unwrap_err();
        match err {
            MediaReadError::Oversized {
                mime: _,
                bytes,
                budget,
            } => {
                assert_eq!(bytes, 6 * 1024 * 1024);
                assert_eq!(budget, 5 * 1024 * 1024);
            }
            other => panic!("expected oversized, got {other:?}"),
        }

        // Unknown modality: typed error.
        assert!(matches!(
            read_media_file("x.bin", "application/octet-stream", &[0u8], &budgets),
            Err(MediaReadError::UnknownModality)
        ));
    }
}
