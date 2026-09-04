//! Session aggregate (docs/13 § Session): long-lived durable container that
//! survives app restarts.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Active,
    Archived,
}
