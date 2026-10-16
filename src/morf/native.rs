//! Typed broker transport for Morf's native root controller. Transport success
//! is not action completion, rendered context or recognizer ownership.
use super::*;
use crate::{ipc::monotonic, protocol::Fence};
mod handoff;
pub use handoff::{HandoffPhase, HandoffState};

const MAX_REQUEST: usize = 4096;
const LEASE: Duration = Duration::from_secs(1);
const GRANT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
struct Decimal(u64);
impl TryFrom<String> for Decimal {
    type Error = &'static str;
    fn try_from(text: String) -> Result<Self, Self::Error> {
        let number = text
            .parse::<u64>()
            .map_err(|_| "invalid decimal identity")?;
        if text != number.to_string() {
            return Err("noncanonical decimal identity");
        }
        Ok(Self(number))
    }
}
impl From<Decimal> for String {
    fn from(value: Decimal) -> Self {
        value.0.to_string()
    }
}
fn nanos(value: Duration) -> io::Result<Decimal> {
    u64::try_from(value.as_nanos())
        .map(Decimal)
        .map_err(|_| invalid("native deadline exceeds u64 nanoseconds"))
}

/// A read-only observation of one native endpoint and Lua VM generation.
/// Every use revalidates it against the verified connection and negotiation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    version: u32,
    epoch: String,
    ui_generation: Decimal,
    root_only: bool,
    queue: usize,
    receipts: usize,
    lease_ms: u64,
    grant_ms: u64,
    recognizer_ownership: bool,
    #[serde(default)]
    cancel_gesture: bool,
    #[serde(default)]
    rendered_context: bool,
}
impl Capabilities {
    pub fn epoch(&self) -> &str {
        &self.epoch
    }
    pub fn ui_generation(&self) -> u64 {
        self.ui_generation.0
    }
    pub fn queue_limit(&self) -> usize {
        self.queue
    }
    pub fn receipt_limit(&self) -> usize {
        self.receipts
    }
    pub fn lease_limit(&self) -> Duration {
        Duration::from_millis(self.lease_ms)
    }
    pub fn grant_limit(&self) -> Duration {
        Duration::from_millis(self.grant_ms)
    }
    pub fn can_cancel_gestures(&self) -> bool {
        self.cancel_gesture
    }
    pub fn has_rendered_context(&self) -> bool {
        self.rendered_context
    }
    fn validate(&self, state: &ShellState, previous: Option<u64>) -> io::Result<()> {
        if self.version != 1
            || self.epoch != state.epoch
            || self.ui_generation.0 == 0
            || previous.is_some_and(|old| self.ui_generation.0 < old)
            || !self.root_only
            || self.queue != 8
            || self.receipts != 64
            || self.lease_ms != 1000
            || self.grant_ms != 250
            || self.recognizer_ownership
        {
            return Err(invalid(
                "unsupported or regressed native controller capabilities",
            ));
        }
        Ok(())
    }
}

/// Correlates a request with the last negotiated native VM and broker fence.
/// The broker must separately approve context, action and recognizer handoff.
#[derive(Clone, Copy)]
pub struct Scope<'a> {
    pub capabilities: &'a Capabilities,
    pub fence: Fence,
}
pub struct Submission<'a> {
    pub id: u64,
    pub deadline: Duration,
    pub request: &'a controls::Request,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Accepted,
    Queued,
    Dispatched,
    Rejected,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    UiReplaced,
    InvalidDeadline,
    OwnerAlive,
    StaleFence,
    Retired,
    Duplicate,
    ExpiredLease,
    InvalidRequest,
    Capacity,
    DuplicateGesture,
    Expired,
    ControllerRefused,
    HandoffRequired,
    HandoffUnavailable,
    StaleOwnership,
    Busy,
    InhibitionFailed,
    NotNeutral,
    ReleaseUnconfirmed,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    version: u32,
    epoch: String,
    ui_generation: Decimal,
    status: Status,
    #[serde(deserialize_with = "Option::deserialize")]
    error: Option<Refusal>,
    #[serde(default)]
    id: Option<Decimal>,
}
impl Reply {
    pub fn status(&self) -> Status {
        self.status
    }
    pub fn refusal(&self) -> Option<Refusal> {
        self.error
    }
    pub fn id(&self) -> Option<u64> {
        self.id.map(|value| value.0)
    }
    pub fn ui_generation(&self) -> u64 {
        self.ui_generation.0
    }
    fn validate(&self, scope: Scope<'_>, operation: &Operation<'_>) -> io::Result<()> {
        let generation = scope.capabilities.ui_generation();
        let replaced = self.status == Status::Rejected
            && self.error == Some(Refusal::UiReplaced)
            && self.ui_generation.0 > generation;
        if self.version != 1
            || self.epoch != scope.capabilities.epoch
            || (self.ui_generation.0 != generation && !replaced)
            || self.ui_generation.0 == 0
            || (self.status == Status::Rejected) != self.error.is_some()
        {
            return Err(invalid(
                "native control reply changed correlation or status",
            ));
        }
        let expected = match operation {
            Operation::Submit { id, .. } | Operation::Receipt { id } => Some(id.0),
            _ => None,
        };
        if self.id().is_some_and(|id| Some(id) != expected)
            || (self.status != Status::Rejected && self.id() != expected)
            || match operation {
                Operation::Lease { .. }
                | Operation::Revoke
                | Operation::CancelGesture { .. }
                | Operation::HandoffQuiesce { .. }
                | Operation::HandoffActivate { .. }
                | Operation::HandoffRelease { .. } => {
                    !matches!(self.status, Status::Accepted | Status::Rejected)
                }
                Operation::Submit { .. } => {
                    !matches!(self.status, Status::Queued | Status::Rejected)
                }
                Operation::Receipt { .. } => self.status == Status::Accepted,
            }
        {
            return Err(invalid("native control reply does not match its operation"));
        }
        Ok(())
    }
}

/// `uncertain` means mutation bytes were sent and an effect cannot be excluded.
/// Do not resend that mutation; query its receipt on a newly verified connection.
/// Local refusals and read-only receipt errors carry `uncertain=false`.
#[derive(Debug)]
pub struct Error {
    pub source: io::Error,
    pub uncertain: bool,
}
impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}{}",
            self.source,
            if self.uncertain {
                " (native mutation may have been sent)"
            } else {
                ""
            }
        )
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Serialize)]
struct Envelope<'a> {
    version: u32,
    epoch: &'a str,
    ui_generation: Decimal,
    fence: Fence,
    #[serde(flatten)]
    operation: Operation<'a>,
}
#[derive(Clone, Copy, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Operation<'a> {
    Lease {
        expires_ns: Decimal,
    },
    Submit {
        id: Decimal,
        deadline_ns: Decimal,
        request: &'a controls::Request,
    },
    Receipt {
        id: Decimal,
    },
    Revoke,
    #[serde(rename = "cancel-gesture")]
    CancelGesture {
        gesture_epoch: &'a str,
        gesture: &'a str,
    },
    #[serde(rename = "handoff-quiesce")]
    HandoffQuiesce {
        ownership_generation: Decimal,
    },
    #[serde(rename = "handoff-activate")]
    HandoffActivate {
        ownership_generation: Decimal,
    },
    #[serde(rename = "handoff-release")]
    HandoffRelease {
        ownership_generation: Decimal,
        descriptors_released: bool,
    },
}
impl Operation<'_> {
    fn validate(&self, state: &ShellState, at: Duration) -> io::Result<()> {
        let deadline = |value: Decimal, maximum: Duration| {
            Duration::from_nanos(value.0)
                .checked_sub(at)
                .is_some_and(|remaining| !remaining.is_zero() && remaining <= maximum)
        };
        let valid = match self {
            Self::Lease { expires_ns } => deadline(*expires_ns, LEASE),
            Self::Submit {
                id,
                deadline_ns,
                request,
            } => {
                request.validate()?;
                id.0 > 0 && request.output == state.output && deadline(*deadline_ns, GRANT)
            }
            Self::Receipt { id } => id.0 > 0,
            Self::Revoke => true,
            Self::CancelGesture {
                gesture_epoch,
                gesture,
            } => {
                let valid = |value: &str| {
                    value.len() == 32
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                };
                valid(gesture_epoch) && valid(gesture)
            }
            Self::HandoffQuiesce {
                ownership_generation,
            }
            | Self::HandoffActivate {
                ownership_generation,
            } => ownership_generation.0 > 0,
            Self::HandoffRelease {
                ownership_generation,
                descriptors_released,
            } => ownership_generation.0 > 0 && *descriptors_released,
        };
        if !valid {
            return Err(invalid(
                "invalid native control target, identity or deadline",
            ));
        }
        Ok(())
    }
    fn mutates(&self) -> bool {
        !matches!(self, Self::Receipt { .. })
    }
    fn expires(&self) -> Option<Duration> {
        match self {
            Self::Lease { expires_ns } => Some(Duration::from_nanos(expires_ns.0)),
            Self::Submit { deadline_ns, .. } => Some(Duration::from_nanos(deadline_ns.0)),
            _ => None,
        }
    }
}

fn require_root() -> io::Result<()> {
    // SAFETY: read-only process credentials; JSON cannot supply authority.
    if unsafe { libc::geteuid() } != 0 {
        return Err(denied("native controller requests require the root broker"));
    }
    Ok(())
}

fn exchange_control(
    transport: &mut Transport,
    scope: Scope<'_>,
    operation: Operation<'_>,
    state: &ShellState,
    until: &mut Instant,
    sent: &mut bool,
) -> io::Result<Reply> {
    operation.validate(state, monotonic()?)?;
    let mut bytes = serde_json::to_vec(&Envelope {
        version: 1,
        epoch: &state.epoch,
        ui_generation: scope.capabilities.ui_generation,
        fence: scope.fence,
        operation,
    })?;
    bytes.push(b'\n');
    if bytes.len() > MAX_REQUEST {
        return Err(invalid("native control request exceeds limit"));
    }
    let now = monotonic()?;
    operation.validate(state, now)?;
    if let Some(expires) = operation.expires() {
        *until = (*until).min(Instant::now() + expires.saturating_sub(now));
    }
    let result = (|| {
        let reply = transport.exchange(&bytes, *until, sent)?;
        let reply: Reply = serde_json::from_slice(&reply)?;
        reply.validate(scope, &operation)?;
        Ok(reply)
    })();
    if result.is_err() && *sent {
        transport.retire();
    }
    result
}

fn control_error(
    transport: &mut Transport,
    operation: Operation<'_>,
    sent: bool,
    source: io::Error,
) -> Error {
    if sent {
        transport.retire();
    }
    Error {
        source,
        uncertain: sent && operation.mutates(),
    }
}

impl Client<ShellState> {
    /// Read-only native limits, bracketed by verified compositor/session samples.
    /// This grants no action or ownership capability.
    pub fn control_capabilities(
        &mut self,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> io::Result<Capabilities> {
        self.control_capabilities_until(logind, compositor, Instant::now() + RPC_TIME)
    }
    pub(crate) fn control_capabilities_until(
        &mut self,
        logind: &Logind,
        compositor: &hyprland::Client,
        until: Instant,
    ) -> io::Result<Capabilities> {
        let result = (|| {
            let state = self.sample_until(logind, compositor, until)?;
            let bytes = self.transport.exchange(
                b"{\"version\":1,\"op\":\"control-capabilities\"}\n",
                until,
                &mut false,
            )?;
            let capabilities: Capabilities = serde_json::from_slice(&bytes)?;
            capabilities.validate(&state, self.control_generation)?;
            let after = self.sample_until(logind, compositor, until)?;
            capabilities.validate(&after, self.control_generation)?;
            self.control_generation = Some(capabilities.ui_generation());
            Ok(capabilities)
        })();
        if result.is_err() {
            self.transport.retire();
        }
        result
    }
    /// Root-only lease with an absolute CLOCK_MONOTONIC expiry, at most one
    /// second ahead. A retired fence requires a newer broker generation.
    pub fn lease_control(
        &mut self,
        scope: Scope<'_>,
        expires: Duration,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> Result<Reply, Error> {
        let expires_ns = nanos(expires).map_err(|source| Error {
            source,
            uncertain: false,
        })?;
        self.control(scope, Operation::Lease { expires_ns }, logind, compositor)
    }
    /// Queues once with a positive ID and a deadline within 250 ms. A queued
    /// reply does not prove controller execution or completed rendering.
    pub fn submit_control(
        &mut self,
        scope: Scope<'_>,
        submission: Submission<'_>,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> Result<Reply, Error> {
        let deadline_ns = nanos(submission.deadline).map_err(|source| Error {
            source,
            uncertain: false,
        })?;
        self.control(
            scope,
            Operation::Submit {
                id: Decimal(submission.id),
                deadline_ns,
                request: submission.request,
            },
            logind,
            compositor,
        )
    }
    /// Queries an existing record without executing or retrying its operation.
    /// A missing or evicted receipt cannot justify resubmission.
    pub fn control_receipt(
        &mut self,
        scope: Scope<'_>,
        id: u64,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> Result<Reply, Error> {
        self.control(
            scope,
            Operation::Receipt { id: Decimal(id) },
            logind,
            compositor,
        )
    }
    /// Revokes native authorization and queued work for this exact fence.
    /// Recognizer ownership is a separate, still-required handoff contract.
    pub fn revoke_control(
        &mut self,
        scope: Scope<'_>,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> Result<Reply, Error> {
        self.control(scope, Operation::Revoke, logind, compositor)
    }
    /// Retires one gesture's queued work and native UI authorization without
    /// revoking the owning fence. Accepted is not rendered cancellation proof.
    pub fn cancel_control_gesture(
        &mut self,
        scope: Scope<'_>,
        gesture_epoch: &str,
        gesture: &str,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> Result<Reply, Error> {
        if !scope.capabilities.can_cancel_gestures() {
            return Err(Error {
                source: io::Error::new(
                    io::ErrorKind::Unsupported,
                    "native provider has no gesture cancellation operation",
                ),
                uncertain: false,
            });
        }
        self.control(
            scope,
            Operation::CancelGesture {
                gesture_epoch,
                gesture,
            },
            logind,
            compositor,
        )
    }
    fn control(
        &mut self,
        scope: Scope<'_>,
        operation: Operation<'_>,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> Result<Reply, Error> {
        let mut sent = false;
        let result = (|| {
            require_root()?;
            scope.fence.validate()?;
            let mut until = Instant::now() + RPC_TIME;
            let state = self.sample_until(logind, compositor, until)?;
            scope
                .capabilities
                .validate(&state, self.control_generation)?;
            if state.phase != ShellPhase::Configured
                || self.control_generation != Some(scope.capabilities.ui_generation())
            {
                return Err(denied(
                    "native controller needs a current configured negotiation",
                ));
            }
            let reply = exchange_control(
                &mut self.transport,
                scope,
                operation,
                &state,
                &mut until,
                &mut sent,
            )?;
            self.sample_until(logind, compositor, until)?;
            self.control_generation = Some(reply.ui_generation());
            Ok(reply)
        })();
        result.map_err(|source| control_error(&mut self.transport, operation, sent, source))
    }
}

#[cfg(test)]
mod tests;
