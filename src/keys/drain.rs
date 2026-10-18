//! Private consumer acknowledgement for a quiesced generated keyboard.

use crate::{
    input::Source,
    protocol::{Epoch, Fence},
    transport::Consumer,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Issued only after final local release; the consumer must drain and verify neutral before replying.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub fence: Fence,
    pub source: Source,
    pub marker: String,
    pub consumer: Consumer,
    pub nonce: Epoch,
    pub requested_at: Duration,
    pub deadline: Duration,
}
