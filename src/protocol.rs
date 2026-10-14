//! Versioned local control messages. Raw input is never a protocol payload.
//! Transport authentication and active-seat registration are separate checks;
//! decoding a packet does not authorize its requested operation.

use crate::{
    context::{Snapshot, State},
    rules::Event,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io, time::Duration};

pub const VERSION: u32 = 1;
pub const MAX_PACKET: usize = 64 * 1024;
pub const MAX_QUEUED_PACKETS: usize = 128;
pub const MAX_QUEUED_BYTES: usize = 512 * 1024;
pub const MAX_PACKET_AGE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Epoch(pub [u64; 2]);

/// Every post-registration request is fenced against broker restarts,
/// configuration replacement and session/ownership changes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fence {
    pub broker: Epoch,
    pub configuration: u64,
    pub session: u64,
}
impl Fence {
    pub fn validate(self) -> io::Result<()> {
        if self.broker.0 == [0, 0] || self.configuration == 0 || self.session == 0 {
            Err(invalid("invalid broker/configuration/session fence"))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Context,
    Observations,
    LuaEvents,
    HyprlandActions,
    MorfActions,
    PowerActions,
    Commands,
    SyntheticInput,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Observe,
    Route,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    Agent,
    Inspect,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    Applied,
    Cancelled,
    Unavailable,
    Uncertain,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Administrator asks the broker to reread its own system configuration.
    /// No client-supplied path or configuration is accepted.
    Reload {},
    Register {
        instance: String,
        #[serde(default)]
        role: Role,
        capabilities: BTreeSet<Capability>,
    },
    Heartbeat {
        fence: Fence,
    },
    LuaSource {
        fence: Fence,
        offset: usize,
    },
    Context {
        fence: Fence,
        snapshot: Box<Snapshot>,
    },
    /// Inhibits desktop work; never serves as a lock/unlock acknowledgement.
    LockRequested {
        fence: Fence,
    },
    /// Ask for fresh authorization of a broker-offered action. This request
    /// carries no executable, argv, or target supplied by the session agent.
    Authorize {
        fence: Fence,
        id: u64,
        ordinal: u64,
    },
    Availability {
        fence: Fence,
        capabilities: BTreeSet<Capability>,
    },
    Complete {
        fence: Fence,
        id: u64,
        ordinal: u64,
        result: Completion,
    },
    Status {
        fence: Fence,
    },
    Disconnect {
        fence: Fence,
    },
}
impl Request {
    pub fn fence(&self) -> Option<Fence> {
        match self {
            Self::Register { .. } | Self::Reload { .. } => None,
            Self::Heartbeat { fence }
            | Self::LuaSource { fence, .. }
            | Self::Context { fence, .. }
            | Self::LockRequested { fence }
            | Self::Authorize { fence, .. }
            | Self::Availability { fence, .. }
            | Self::Complete { fence, .. }
            | Self::Status { fence }
            | Self::Disconnect { fence } => Some(*fence),
        }
    }
    pub fn validate(&self) -> io::Result<()> {
        if let Some(fence) = self.fence() {
            fence.validate()?;
        }
        match self {
            Self::LuaSource { offset, .. }
                if *offset > crate::lua::MAX_SNAPSHOT_BYTES
                    || *offset % crate::lua::source_transfer::CHUNK_BYTES != 0 =>
            {
                return Err(invalid("invalid Lua source request offset"));
            }
            Self::Register { instance, .. }
                if instance.is_empty()
                    || instance.len() > 160
                    || matches!(instance.as_str(), "." | "..")
                    || !instance.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                    }) =>
            {
                return Err(invalid("invalid compositor instance"));
            }
            Self::Context { fence, snapshot } => {
                if snapshot.generation != fence.session {
                    return Err(invalid("context generation differs from its fence"));
                }
                snapshot.validate().map_err(io::Error::other)?;
            }
            Self::Complete { id, ordinal, .. } | Self::Authorize { id, ordinal, .. }
                if *id == 0 || *ordinal == 0 =>
            {
                return Err(invalid("invalid action completion identity"));
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Welcome {
        fence: Fence,
        seat: String,
        session: String,
        mode: Mode,
        capabilities: BTreeSet<Capability>,
    },
    Status {
        fence: Fence,
        state: State,
        revision: u64,
        pending: usize,
        #[serde(default)]
        last_reload_error: Option<String>,
        #[serde(default)]
        context_error: Option<String>,
    },
    Accepted {
        fence: Fence,
        revision: u64,
    },
    /// The worker has reconciled held input and activated fresh observation.
    Subscribed {
        fence: Fence,
    },
    Configuration {
        fence: Fence,
    },
    SessionChanged {
        fence: Fence,
        state: State,
    },
    ReloadResult {
        configuration: u64,
        error: Option<String>,
    },
    Observation {
        fence: Fence,
        event: Box<Event>,
    },
    LuaSource {
        fence: Fence,
        offset: usize,
        bytes: usize,
        #[serde(with = "crate::lua::source_transfer::chunk_bytes")]
        data: Vec<u8>,
    },
    /// Only the routing/action service may construct dispatches after checking
    /// ownership, current registration, frozen target and negotiated capability.
    Dispatch {
        fence: Fence,
        event: Box<Event>,
    },
    /// Short-lived permission for an already offered action. The consumer
    /// must still check its current fence and this deadline before execution.
    Execute {
        fence: Fence,
        id: u64,
        ordinal: u64,
        expires: Duration,
    },
    Revoked {
        fence: Fence,
        reason: String,
    },
    Rejected {
        reason: String,
    },
}
impl Reply {
    pub fn fence(&self) -> Option<Fence> {
        match self {
            Self::Rejected { .. } | Self::ReloadResult { .. } => None,
            Self::Welcome { fence, .. }
            | Self::Status { fence, .. }
            | Self::Accepted { fence, .. }
            | Self::Subscribed { fence }
            | Self::Configuration { fence }
            | Self::SessionChanged { fence, .. }
            | Self::Observation { fence, .. }
            | Self::LuaSource { fence, .. }
            | Self::Dispatch { fence, .. }
            | Self::Execute { fence, .. }
            | Self::Revoked { fence, .. } => Some(*fence),
        }
    }
    pub fn validate(&self) -> io::Result<()> {
        if let Some(fence) = self.fence() {
            fence.validate()?;
        }
        let text = |value: &str| {
            !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
        };
        match self {
            Self::LuaSource {
                offset,
                bytes,
                data,
                ..
            } if *bytes > crate::lua::MAX_SNAPSHOT_BYTES
                || *offset > *bytes
                || data.len() > crate::lua::source_transfer::CHUNK_BYTES
                || data.len() != crate::lua::source_transfer::CHUNK_BYTES.min(*bytes - *offset) =>
            {
                return Err(invalid("invalid Lua source reply size or offset"));
            }
            Self::ReloadResult {
                configuration,
                error,
            } if *configuration == 0 || error.as_ref().is_some_and(|error| !text(error)) => {
                return Err(invalid("invalid reload result"));
            }
            Self::Status {
                last_reload_error: Some(error),
                ..
            } if !text(error) => {
                return Err(invalid("invalid reload status"));
            }
            Self::Status {
                context_error: Some(error),
                ..
            } if !text(error) => {
                return Err(invalid("invalid context status"));
            }
            Self::Welcome { seat, session, .. } if !text(seat) || !text(session) => {
                return Err(invalid("invalid session description"));
            }
            Self::Revoked { reason, .. } | Self::Rejected { reason } if !text(reason) => {
                return Err(invalid("invalid protocol rejection"));
            }
            Self::Execute {
                id,
                ordinal,
                expires,
                ..
            } if *id == 0 || *ordinal == 0 || expires.is_zero() => {
                return Err(invalid("invalid action execution identity/deadline"));
            }
            Self::Observation { fence, event } | Self::Dispatch { fence, event } => {
                if let Event::Matched {
                    version,
                    config_generation,
                    session_generation,
                    ..
                } = event.as_ref()
                    && (*version != VERSION
                        || *config_generation != fence.configuration
                        || *session_generation != fence.session)
                {
                    return Err(invalid("event generation differs from its fence"));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Packet<T> {
    pub version: u32,
    pub sequence: u64,
    pub at: Duration,
    pub message: T,
}
impl<T> Packet<T> {
    pub fn validate_header(&self, next: u64, now: Duration) -> io::Result<()> {
        if self.version != VERSION || self.sequence != next || next == 0 {
            return Err(invalid("unsupported protocol version or reordered packet"));
        }
        if self.at > now || now - self.at >= MAX_PACKET_AGE {
            return Err(invalid("protocol packet is stale or from the future"));
        }
        Ok(())
    }
}
pub fn encode<T: Serialize>(packet: &Packet<T>) -> io::Result<Vec<u8>> {
    // Streaming into a bounded writer avoids allocating an unbounded serialized
    // reply before discovering that it exceeds the transport limit.
    struct Buffer(Vec<u8>);
    impl io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0.len() + bytes.len() > MAX_PACKET {
                return Err(invalid("protocol packet exceeds size limit"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Buffer(Vec::new());
    serde_json::to_writer(&mut buffer, packet).map_err(io::Error::other)?;
    Ok(buffer.0)
}
pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> io::Result<Packet<T>> {
    if bytes.is_empty() || bytes.len() > MAX_PACKET {
        return Err(invalid("protocol packet exceeds size limit"));
    }
    serde_json::from_slice(bytes).map_err(io::Error::other)
}
pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
