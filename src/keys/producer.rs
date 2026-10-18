//! Private sole-worker software-key admission; identities do not grant authority.

use crate::{
    input::Source,
    protocol::{Epoch, Fence},
    transport::Consumer,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub fence: Fence,
    pub source: Source,
    pub marker: String,
    pub consumer: Consumer,
    pub producer: Epoch,
    pub sequence: u64,
    pub operation: Operation,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Open { expires: Duration },
    Renew { expires: Duration },
    Press { id: u64, key: u16 },
    Repeat { id: u64 },
    Release { id: u64 },
    Close {},
}
