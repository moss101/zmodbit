//! Run aggregate (docs/13 § Run): one concrete execution attempt of a task.
//! A task may have multiple runs after retry/fork/handoff; `attempt` is
//! 1-based and monotonically increasing per task.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Prepared,
    Running,
    Completed,
    Failed,
    Cancelled,
}
