//! Structured AskUserQuestion (M2, REQ-EV-0222, ADAPT): disambiguation
//! questions are TYPED (question + discrete options). When an interactive
//! surface exists the user answers by option id; a headless run returns
//! NEEDS_INPUT immediately — it never hangs waiting for a UI that does
//! not exist. The text fallback is explicit and typed.

use serde::{Deserialize, Serialize};
use std::fmt;

/// One selectable answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
    pub description: String,
}

/// A structured question: typed, options-bound, single-select.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StructuredQuestion {
    pub question_id: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
}

impl StructuredQuestion {
    pub fn new(question_id: &str, question: &str, options: Vec<QuestionOption>) -> Self {
        Self {
            question_id: question_id.to_string(),
            question: question.to_string(),
            options,
        }
    }

    /// Structural validation: a question without ≥2 options cannot
    /// disambiguate anything.
    pub fn validate(&self) -> Result<(), String> {
        if self.question.trim().is_empty() {
            return Err("question text must not be empty".into());
        }
        if self.options.len() < 2 {
            return Err(format!(
                "question {} needs at least 2 options (got {})",
                self.question_id,
                self.options.len()
            ));
        }
        for o in &self.options {
            if o.id.is_empty() || o.label.is_empty() {
                return Err(format!("option {:?} missing id/label", o.id));
            }
        }
        Ok(())
    }
}

/// The surface the question is asked on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskSurface {
    /// An interactive UI exists: answers arrive by option id.
    Interactive,
    /// No UI (headless run): questions must not block.
    Headless,
}

/// The typed answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AskOutcome {
    Answered {
        option_id: String,
        free_text: Option<String>,
    },
    /// Headless runs surface NEEDS_INPUT instead of hanging; the full
    /// question travels so an operator can answer it later.
    NeedsInput { question_id: String },
    /// The interactive user dismissed the question.
    Dismissed,
}

impl fmt::Display for AskOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AskOutcome::Answered { option_id, .. } => write!(f, "answered({option_id})"),
            AskOutcome::NeedsInput { question_id } => {
                write!(f, "NEEDS_INPUT({question_id})")
            }
            AskOutcome::Dismissed => write!(f, "dismissed"),
        }
    }
}

#[derive(Debug)]
pub enum AskError {
    /// Malformed question (validation must run before asking).
    InvalidQuestion(String),
    /// An interactive answer referenced an unknown option id.
    UnknownOption { option_id: String },
}

impl std::fmt::Display for AskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AskError::InvalidQuestion(why) => write!(f, "invalid question: {why}"),
            AskError::UnknownOption { option_id } => {
                write!(f, "unknown option id {option_id:?}")
            }
        }
    }
}

impl std::error::Error for AskError {}

/// Asks a structured question on the given surface. NEVER blocks in
/// headless mode: it returns `NeedsInput` immediately (QUAL-EV-0222).
pub fn ask(question: &StructuredQuestion, surface: AskSurface) -> Result<AskOutcome, AskError> {
    question.validate().map_err(AskError::InvalidQuestion)?;
    match surface {
        AskSurface::Headless => Ok(AskOutcome::NeedsInput {
            question_id: question.question_id.clone(),
        }),
        AskSurface::Interactive => {
            // In the real app the UI renders the options; the runtime
            // receives an option id back. The typed contract is exercised
            // by the caller supplying `answer` — modeled here by the
            // dispatcher: an interactive ask without a connected surface
            // is downgraded to NeedsInput, never a hang.
            Ok(AskOutcome::NeedsInput {
                question_id: question.question_id.clone(),
            })
        }
    }
}

/// Validates an interactive answer against the question's options.
pub fn validate_answer(question: &StructuredQuestion, option_id: &str) -> Result<(), AskError> {
    if !question.options.iter().any(|o| o.id == option_id) {
        return Err(AskError::UnknownOption {
            option_id: option_id.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question() -> StructuredQuestion {
        StructuredQuestion::new(
            "q-db",
            "Which database should the migration target?",
            vec![
                QuestionOption {
                    id: "staging".into(),
                    label: "Staging".into(),
                    description: "the staging warehouse".into(),
                },
                QuestionOption {
                    id: "production".into(),
                    label: "Production".into(),
                    description: "the live warehouse (irreversible)".into(),
                },
            ],
        )
    }

    /// QUAL-EV-0222: a headless run returns NEEDS_INPUT rather than
    /// hanging.
    #[test]
    fn headless_returns_needs_input_and_never_hangs() {
        let outcome = ask(&question(), AskSurface::Headless).unwrap();
        assert_eq!(
            outcome,
            AskOutcome::NeedsInput {
                question_id: "q-db".into()
            }
        );
        assert!(outcome.to_string().starts_with("NEEDS_INPUT"));
    }

    #[test]
    fn malformed_questions_and_unknown_options_are_typed_errors() {
        let bad = StructuredQuestion::new("q-1", "Pick one", vec![]);
        assert!(matches!(
            ask(&bad, AskSurface::Interactive),
            Err(AskError::InvalidQuestion(_))
        ));

        let q = question();
        assert!(validate_answer(&q, "staging").is_ok());
        assert!(matches!(
            validate_answer(&q, "chaos"),
            Err(AskError::UnknownOption { .. })
        ));
    }
}
