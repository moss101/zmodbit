//! RunStep aggregate (docs/13 § RunStep): typed atomic runtime step.

use serde::{Deserialize, Serialize};

use crate::events::StepType;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStepState {
    Prepared,
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStep {
    pub step_type: StepType,
    pub state: RunStepState,
    pub ordinal: u32,
}
