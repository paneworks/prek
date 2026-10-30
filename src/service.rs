//! Authenticated, bounded broker control plane. Device descriptors belong to a
//! separate supervised worker; this service performs no desktop actions.
//! Desktop context comes from the Wayland protocol, the kernel and logind,
//! never client snapshots. Membership alone never establishes an unlocked desktop.

use crate::{
    compositor::Compositor,
    context::{DECISION_MAX_AGE, SNAPSHOT_MAX_AGE, State},
    ipc::{Channel, Endpoint},
    protocol::{self, Capability, Epoch, Fence, Mode, Reply, Request, Role},
    session::{Logind, Peer, Registration},
    worker::{ProvidedContext, Report, Subscription},
};
use nix::poll::{PollFd, PollFlags};
mod recognizers;
mod rule_control;
mod sampler;
mod source;
mod transport_offer;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs, io,
    os::{
        fd::{AsFd, BorrowedFd},
        unix::fs::MetadataExt,
    },
    path::PathBuf,
    time::Duration,
};

pub const MAX_CLIENTS: usize = 8;
pub const REFRESH: Duration = protocol::KEEPALIVE;
pub const CONTEXT_REFRESH: Duration = protocol::KEEPALIVE;
const CONTEXT_SAMPLE_TIME: Duration = Duration::from_millis(80);

#[derive(Default)]
struct Grace {
    fences: Vec<Fence>,
    expires: Duration,
}
impl Grace {
    fn remember(&mut self, fence: Fence, now: Duration) -> io::Result<()> {
        if now >= self.expires {
            self.fences.clear();
            self.expires = now + protocol::MAX_PACKET_AGE;
        }
        if !self.fences.contains(&fence) {
            if self.fences.len() >= 32 {
                return Err(protocol::invalid(
                    "unacknowledged session transitions exceeded limit",
                ));
            }
            self.fences.push(fence);
        }
        Ok(())
    }
    fn permits(&self, request: &Request, now: Duration) -> bool {
        matches!(request, Request::Heartbeat { fence }
            if now < self.expires && self.fences.contains(fence))
    }
    /// Whether a request sent under a remembered fence is discarded unanswered.
    fn discards(&self, request: &Request, now: Duration) -> bool {
        matches!(request, Request::Status { fence } | Request::LuaSource { fence, .. }
            | Request::ContextRead { fence, .. } | Request::Rules { fence, .. }
            | Request::RecognizerReady { fence, .. } | Request::Recognize { fence, .. }
            | Request::RecognitionCancelled { fence, .. } | Request::Authorize { fence, .. }
            | Request::Complete { fence, .. } | Request::Availability { fence, .. }
            if now < self.expires && self.fences.contains(fence))
    }
}

struct ContextFeed {
    sampler: sampler::Sampler,
    value: Option<Box<ProvidedContext>>,
    state: State,
    usable: bool,
    checked: Duration,
    due: bool,
    error: Option<String>,
}
impl Default for ContextFeed {
    fn default() -> Self {
        Self {
            sampler: sampler::Sampler::default(),
            value: None,
            state: State::Unknown,
            usable: false,
            checked: Duration::ZERO,
            due: false,
            error: None,
        }
    }
}
impl ContextFeed {
    fn fresh(&self, now: Duration) -> bool {
        self.usable
            && now >= self.checked
            && now - self.checked <= SNAPSHOT_MAX_AGE
            && self.value.as_ref().is_none_or(|context| {
                now >= context.snapshot.captured_at
                    && now - context.snapshot.captured_at <= SNAPSHOT_MAX_AGE
            })
    }
    /// Freshness for a caller that took `now` before refreshing this context
    /// in the same step: a later sample is judged from its own time.
    fn fresh_after_sample(&self, now: Duration) -> bool {
        self.fresh(now.max(self.checked))
    }
    fn state(&self, now: Duration) -> State {
        if matches!(self.state, State::Locking | State::Locked) || self.fresh(now) {
            self.state
        } else {
            State::Unknown
        }
    }
}

pub use crate::ipc::monotonic;

pub struct Policy {
    pub runtime_root: PathBuf,
    pub compositor_executable: PathBuf,
    pub agent_executable: PathBuf,
    pub allowed_seats: BTreeSet<String>,
}
struct Bound {
    transport_started: bool,
    client: Compositor,
    registration: Registration,
    seat: String,
    role: Role,
    fence: Fence,
    renewed: Duration,
    checked: Duration,
    capabilities: BTreeSet<Capability>,
    observing: bool,
    previous: Grace,
    context: ContextFeed,
    compositor: (u32, u64),
    actions: ActionLedger,
    available_actions: BTreeSet<Capability>,
    context_export: Option<crate::context_transfer::Export>,
    rules: rule_control::Rules,
    recognizers: recognizers::Ledger,
}

fn registration_capabilities(
    routing: Option<&crate::config::Config>,
    baseline: Option<&crate::config::Config>,
    lua: bool,
    recognizers: bool,
    observations: bool,
    role: Role,
    capabilities: &BTreeSet<Capability>,
) -> io::Result<()> {
    // Permitted runtime rules may bring their own command and key actions.
    let commands = routing.is_some_and(|config| {
        config.policy.allow_runtime_rules
            || config.rules.iter().any(|rule| {
                rule.enabled
                    && matches!(
                        rule.action,
                        crate::config::Action::Command { .. } | crate::config::Action::Lua { .. }
                    )
            })
    });
    let keys = routing.is_some_and(|config| {
        config.policy.allow_runtime_rules
            || config.rules.iter().any(|rule| {
                rule.enabled
                    && matches!(
                        rule.action,
                        crate::config::Action::Keys { .. } | crate::config::Action::Remap { .. }
                    )
            })
    });
    if capabilities.iter().any(|capability| {
        !(matches!(capability, Capability::Observations | Capability::Context)
            || *capability == Capability::LuaEvents
                && lua
                && capabilities.contains(&Capability::Observations)
                && capabilities.contains(&Capability::Context)
            || *capability == Capability::LuaActions
                && routing.is_some()
                && lua
                && capabilities.contains(&Capability::LuaEvents)
                && capabilities.contains(&Capability::Commands)
            || *capability == Capability::RuntimeRules
                && baseline.is_some_and(|config| config.policy.allow_runtime_rules)
                && capabilities.contains(&Capability::LuaEvents)
                && capabilities.contains(&Capability::Context)
                && capabilities.contains(&Capability::Observations)
            || *capability == Capability::InputEvents
                && routing.is_none()
                && lua
                && baseline.is_some_and(|config| {
                    config.policy.allow_input_events && config.mode == crate::config::Mode::Observe
                })
                && capabilities.contains(&Capability::LuaEvents)
                && capabilities.contains(&Capability::Context)
                && capabilities.contains(&Capability::Observations)
            || *capability == Capability::CustomRecognizers
                && lua
                && recognizers
                && baseline.is_some_and(|config| {
                    config.policy.allow_input_events
                        && (routing.is_some() || config.mode == crate::config::Mode::Observe)
                })
                && capabilities.contains(&Capability::LuaEvents)
                && capabilities.contains(&Capability::Context)
                && capabilities.contains(&Capability::Observations)
            || *capability == Capability::Commands
                && commands
                && capabilities.contains(&Capability::Context)
            || *capability == Capability::SyntheticInput
                && keys
                && capabilities.contains(&Capability::Context))
    }) || !capabilities.is_empty() && (!observations || role != Role::Agent)
    {
        return Err(protocol::invalid(
            "requested capabilities are not implemented by this broker",
        ));
    }
    Ok(())
}

#[derive(Default)]
struct ActionLedger {
    pending: BTreeMap<(u64, u64), Offered>,
    seen: BTreeMap<u64, u64>,
}
struct Offered {
    source: crate::input::Source,
    snapshot: crate::context::Snapshot,
    output: Option<String>,
    deadline: Duration,
    granted: bool,
    sent: bool,
    delivered: bool,
    cancelled: bool,
    required: BTreeSet<Capability>,
}
impl Offered {
    fn target_valid(&self, snapshot: &crate::context::Snapshot, now: Duration) -> bool {
        let output = |snapshot: &crate::context::Snapshot| {
            snapshot
                .outputs
                .iter()
                .find(|output| Some(&output.geometry.name) == self.output.as_ref())
                .cloned()
        };
        !self.cancelled
            && now < self.deadline
            && snapshot.captured_at <= now
            && now - snapshot.captured_at < DECISION_MAX_AGE
            && snapshot.state == State::Desktop
            && snapshot.generation == self.snapshot.generation
            && (self.output.is_none()
                || output(&self.snapshot).is_some_and(|old| Some(old) == output(snapshot)))
    }
    fn capabilities_available(
        &self,
        negotiated: &BTreeSet<Capability>,
        available: &BTreeSet<Capability>,
    ) -> bool {
        self.required.is_subset(negotiated)
            && self.required.iter().all(|capability| {
                !protocol::execution_capability(*capability) || available.contains(capability)
            })
    }
}
impl ActionLedger {
    fn offer(
        &mut self,
        event: &crate::rules::Event,
        snapshot: &crate::context::Snapshot,
        now: Duration,
    ) -> io::Result<()> {
        let crate::rules::Event::Matched {
            id,
            ordinal,
            output,
            source,
            action,
            ..
        } = event
        else {
            return Err(protocol::invalid("only matched actions can be offered"));
        };
        if *id == 0
            || *ordinal == 0
            || self.pending.len() >= 64
            || self.seen.get(id).is_some_and(|old| ordinal <= old)
            || !self.seen.contains_key(id) && self.seen.len() >= 4096
        {
            return Err(protocol::invalid(
                "duplicate action or bounded action ledger exhausted",
            ));
        }
        self.seen.insert(*id, *ordinal);
        self.pending.insert(
            (*id, *ordinal),
            Offered {
                source: source.clone(),
                snapshot: snapshot.clone(),
                output: output.clone(),
                deadline: now + protocol::MAX_PACKET_AGE,
                granted: false,
                sent: false,
                delivered: false,
                cancelled: false,
                required: protocol::action_capabilities(action),
            },
        );
        Ok(())
    }
    fn authorize(
        &mut self,
        id: u64,
        ordinal: u64,
        snapshot: &crate::context::Snapshot,
        now: Duration,
    ) -> io::Result<()> {
        let offer = self
            .pending
            .get_mut(&(id, ordinal))
            .ok_or_else(|| protocol::invalid("action authorization has no broker offer"))?;
        if offer.granted || !offer.target_valid(snapshot, now) {
            return Err(protocol::invalid(
                "action target or execution authority expired",
            ));
        }
        offer.granted = true;
        Ok(())
    }
    fn complete(&mut self, id: u64, ordinal: u64) -> io::Result<()> {
        if self
            .pending
            .get(&(id, ordinal))
            .is_none_or(|offer| !offer.granted || !offer.sent)
        {
            return Err(protocol::invalid("completion has no granted broker action"));
        }
        self.pending.remove(&(id, ordinal));
        Ok(())
    }
    fn complete_unexecuted(&mut self, id: u64, ordinal: u64) -> io::Result<()> {
        if self
            .pending
            .get(&(id, ordinal))
            .is_some_and(|offer| offer.delivered && !offer.sent)
        {
            self.pending.remove(&(id, ordinal));
            Ok(())
        } else {
            self.complete(id, ordinal)
        }
    }
    fn cancel(
        &mut self,
        source: &crate::input::Source,
        gesture: Option<u64>,
    ) -> Vec<((u64, u64), bool)> {
        let mut cancelled = Vec::new();
        for (identity, offer) in &mut self.pending {
            if offer.source == *source
                && gesture.is_none_or(|id| identity.0 == id)
                && !offer.cancelled
            {
                offer.cancelled = true;
                cancelled.push((*identity, offer.delivered));
            }
        }
        for (identity, delivered) in &cancelled {
            if !delivered {
                self.pending.remove(identity);
            }
        }
        cancelled
    }
}
struct Connection {
    channel: Channel,
    opened: Duration,
    bound: Option<Bound>,
    outgoing: VecDeque<Reply>,
    bytes: usize,
    administrator: bool,
    waiting_reload: bool,
}
impl Connection {
    fn push(&mut self, reply: Reply) -> io::Result<()> {
        reply.validate()?;
        if matches!(reply, Reply::ActionCancelled { .. }) && self.outgoing.contains(&reply) {
            return Ok(());
        }
        if let Reply::Observation { fence, event } = &reply {
            self.outgoing.retain(|old| {
                !matches!(old,
                Reply::Observation { fence: old_fence, event: old_event }
                if old_fence == fence && event.supersedes_progress(old_event))
            });
            self.bytes = self
                .outgoing
                .iter()
                .map(reply_size)
                .sum::<io::Result<usize>>()?;
        }
        if self.outgoing.len() >= 16 {
            return Err(protocol::invalid("control subscriber queue is full"));
        }
        let bytes = reply_size(&reply)?;
        if self.bytes + bytes > protocol::MAX_QUEUED_BYTES {
            return Err(protocol::invalid("control subscriber byte limit exceeded"));
        }
        self.bytes += bytes;
        self.outgoing.push_back(reply);
        Ok(())
    }
    fn flush(&mut self, now: Duration) -> io::Result<()> {
        if let Some(reply) = self.outgoing.front()
            && self.channel.send(reply, now)?
        {
            let reply = self.outgoing.pop_front().unwrap();
            if let Reply::Recognition { ticket, .. } = &reply
                && let Some(bound) = self.bound.as_mut()
            {
                bound.recognizers.delivered(ticket.id)?;
            }
            if let Reply::Dispatch { event, .. } = &reply
                && let crate::rules::Event::Matched { id, ordinal, .. } = event.as_ref()
                && let Some(bound) = self.bound.as_mut()
                && let Some(offer) = bound.actions.pending.get_mut(&(*id, *ordinal))
            {
                offer.delivered = true;
            }
            if let Reply::Execute { id, ordinal, .. } = &reply
                && let Some(bound) = self.bound.as_mut()
                && let Some(offer) = bound.actions.pending.get_mut(&(*id, *ordinal))
            {
                offer.sent = true;
            }
            self.bytes -= reply_size(&reply)?;
        }
        Ok(())
    }
    fn cancel_actions(
        &mut self,
        source: &crate::input::Source,
        gesture: Option<u64>,
    ) -> io::Result<()> {
        let bound = self.bound.as_mut().unwrap();
        if source.seat != bound.seat || source.generation == 0 {
            return Err(protocol::invalid(
                "action cancellation belongs to another source/seat",
            ));
        }
        let fence = bound.fence;
        let cancelled = bound.actions.cancel(source, gesture);
        let identities = cancelled
            .iter()
            .map(|(identity, _)| *identity)
            .collect::<BTreeSet<_>>();
        self.outgoing.retain(|reply| {
            let identity = match reply {
                Reply::Execute { id, ordinal, .. } => Some((*id, *ordinal)),
                Reply::Dispatch { event, .. } => match event.as_ref() {
                    crate::rules::Event::Matched { id, ordinal, .. } => Some((*id, *ordinal)),
                    _ => None,
                },
                _ => None,
            };
            identity.is_none_or(|identity| !identities.contains(&identity))
        });
        self.bytes = self
            .outgoing
            .iter()
            .map(reply_size)
            .sum::<io::Result<usize>>()?;
        for ((id, ordinal), delivered) in cancelled {
            if delivered {
                self.push(Reply::ActionCancelled { fence, id, ordinal })?;
            }
        }
        Ok(())
    }
}

fn reply_size(reply: &Reply) -> io::Result<usize> {
    Ok(protocol::encode(&protocol::Packet {
        version: protocol::VERSION,
        sequence: u64::MAX,
        at: Duration::MAX,
        message: reply,
    })?
    .len())
}

fn subscriptions_fit(seats: &[Subscription]) -> bool {
    crate::subscription_transfer::fits(seats)
}

pub struct Broker {
    endpoint: Endpoint,
    logind: Logind,
    policy: Policy,
    epoch: Epoch,
    configuration: u64,
    session: u64,
    connections: Vec<Connection>,
    cursor: usize,
    observations: bool,
    reload_requested: bool,
    reload_pending: bool,
    last_reload_error: Option<String>,
    routing: Option<crate::config::Config>,
    lua_source: Option<Vec<u8>>,
    rule_baseline: Option<std::sync::Arc<crate::config::Config>>,
    rule_handlers: Vec<crate::lua::HandlerRegistration>,
    recognizer_bindings: Vec<crate::lua::RecognizerBinding>,
    published: bool,
}
impl Broker {
    /// Context is sampled directly from a verified compositor. Action dispatch
    /// and synthetic input require separate routing/provider capabilities.
    pub fn open(endpoint: Endpoint, library: &std::path::Path, policy: Policy) -> io::Result<Self> {
        if policy.allowed_seats.is_empty()
            || !policy.runtime_root.is_absolute()
            || !policy.agent_executable.is_absolute()
            || !policy.compositor_executable.is_absolute()
        {
            return Err(protocol::invalid(
                "broker requires explicit trusted registration policy",
            ));
        }
        Ok(Self {
            endpoint,
            logind: Logind::open(library)?,
            policy,
            epoch: Epoch::random()?,
            configuration: 1,
            session: 0,
            connections: Vec::new(),
            cursor: 0,
            observations: false,
            reload_requested: false,
            reload_pending: false,
            last_reload_error: None,
            routing: None,
            lua_source: None,
            rule_baseline: None,
            rule_handlers: Vec::new(),
            recognizer_bindings: Vec::new(),
            published: false,
        })
    }
    /// Whether a change notification refreshed an agent's context, or a
    /// session advanced, since the last call.
    pub fn take_published(&mut self) -> bool {
        std::mem::take(&mut self.published)
    }
    /// Marks an agent's context due once logind, its display connection or
    /// one of its registered processes reports a change.
    fn observe(&mut self) {
        let logind = self.logind.changed();
        for bound in self
            .connections
            .iter_mut()
            .filter_map(|connection| connection.bound.as_mut())
            .filter(|bound| bound.role == Role::Agent)
        {
            let display = bound.context.sampler.changed();
            let exited = bound.registration.exited();
            if logind || display || exited {
                bound.context.due = true;
            }
        }
    }
    pub fn clients(&self) -> usize {
        self.connections
            .iter()
            .filter(|connection| {
                connection
                    .bound
                    .as_ref()
                    .is_some_and(|bound| bound.role == Role::Agent)
            })
            .count()
    }
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
    pub fn enable_observations(&mut self) {
        self.observations = true;
    }
    /// Install root-owned action policy before registration. The CLI leaves
    /// this disabled until the complete device handoff is ready.
    pub fn enable_routing(&mut self, config: crate::config::Config) -> io::Result<()> {
        if !self.connections.is_empty() || config.mode != crate::config::Mode::Route {
            return Err(protocol::invalid(
                "routing policy requires a fresh route broker",
            ));
        }
        crate::worker::preflight(&config)?;
        self.routing = Some(config);
        self.observations = true;
        Ok(())
    }
    pub fn configuration(&self) -> u64 {
        self.configuration
    }
    pub fn install_lua_source(
        &mut self,
        definition: Option<&crate::lua::Definition>,
    ) -> io::Result<()> {
        let source = source::Prepared::new(definition)?;
        self.rule_baseline = source.baseline;
        self.lua_source = source.bytes;
        self.rule_handlers = source.handlers;
        self.recognizer_bindings = source.recognizers;
        for connection in &mut self.connections {
            if let Some(bound) = connection.bound.as_mut() {
                bound.rules = rule_control::Rules::new(self.rule_baseline.clone());
                bound.recognizers = Default::default();
            }
        }
        Ok(())
    }
    pub fn take_reload_request(&mut self) -> bool {
        std::mem::take(&mut self.reload_requested)
    }
    pub fn finish_reload(
        &mut self,
        result: Result<BTreeSet<String>, String>,
        now: Duration,
    ) -> io::Result<()> {
        match result {
            Ok(seats) => {
                self.configuration = self
                    .configuration
                    .checked_add(1)
                    .ok_or_else(|| protocol::invalid("configuration generation exhausted"))?;
                self.policy.allowed_seats = seats;
                self.last_reload_error = None;
                self.connections.retain_mut(|connection| {
                    let Some(bound) = connection.bound.as_mut() else {
                        return true;
                    };
                    if !self.policy.allowed_seats.contains(&bound.seat) {
                        return false;
                    }
                    if bound.previous.remember(bound.fence, now).is_err() {
                        return false;
                    }
                    bound.fence.configuration = self.configuration;
                    bound.context_export = None;
                    bound.observing = false;
                    bound.actions = ActionLedger::default();
                    bound.recognizers = Default::default();
                    let fence = bound.fence;
                    connection.outgoing.clear();
                    connection.bytes = 0;
                    connection.push(Reply::Configuration { fence }).is_ok()
                });
            }
            Err(error) => {
                self.last_reload_error = Some(
                    error
                        .lines()
                        .next()
                        .unwrap_or("configuration reload rejected")
                        .chars()
                        .filter(|ch| !ch.is_control())
                        .take(512)
                        .collect(),
                );
            }
        }
        for connection in &mut self.connections {
            if connection.waiting_reload {
                connection.waiting_reload = false;
                connection.push(Reply::ReloadResult {
                    configuration: self.configuration,
                    error: self.last_reload_error.clone(),
                })?;
            }
        }
        self.reload_pending = false;
        Ok(())
    }
    pub fn finish_configuration(
        &mut self,
        result: Result<crate::config::Config, String>,
        now: Duration,
    ) -> io::Result<()> {
        let result = result.and_then(|config| {
            if self.routing.is_some() {
                crate::routing::Routing::new(config.clone()).map_err(|error| error.to_string())?;
            }
            Ok(config)
        });
        match result {
            Ok(config) => {
                self.finish_reload(
                    Ok(config.policy.allowed_seats.iter().cloned().collect()),
                    now,
                )?;
                if self.routing.is_some() {
                    self.routing = Some(config);
                }
                Ok(())
            }
            Err(error) => self.finish_reload(Err(error), now),
        }
    }
    pub fn subscriptions(&mut self, now: Duration) -> io::Result<Vec<Subscription>> {
        self.connections.retain_mut(|connection| {
            let Some(bound) = connection.bound.as_mut() else {
                return true;
            };
            let result = if now < bound.checked {
                Err(protocol::invalid("session timeline moved backward"))
            } else if now.saturating_sub(bound.renewed) >= protocol::LEASE {
                Err(protocol::invalid("session agent lease expired"))
            } else {
                bound.registration.check(&mut self.logind, &bound.client)
            };
            if let Err(error) = result {
                let reason = error
                    .to_string()
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(512)
                    .collect();
                let _ = connection.channel.send(
                    &Reply::Revoked {
                        fence: bound.fence,
                        reason,
                    },
                    now,
                );
                return false;
            }
            true
        });
        let subscriptions: Vec<_> = self
            .connections
            .iter()
            .filter_map(|connection| connection.bound.as_ref())
            .filter(|bound| {
                bound.role == Role::Agent
                    && (bound.capabilities.contains(&Capability::Observations)
                        || self.routing.is_some()
                            && bound.capabilities.iter().copied().any(|capability| {
                                protocol::execution_capability(capability)
                                    || capability == Capability::SyntheticInput
                            }))
                    && bound.context.fresh(now)
            })
            .map(|bound| Subscription {
                seat: bound.seat.clone(),
                fence: bound.fence,
                context: bound.context.value.clone(),
                capabilities: bound
                    .capabilities
                    .iter()
                    .filter(|capability| {
                        if **capability == Capability::CustomRecognizers
                            && !bound.recognizers.requested
                        {
                            return false;
                        }
                        !protocol::execution_capability(**capability)
                            || bound.available_actions.contains(capability)
                                && bound.actions.pending.len() < crate::actions::MAX_RUNNING
                    })
                    .copied()
                    .collect(),
                consumer: Some(bound.client.consumer()),
            })
            .collect();
        if !subscriptions_fit(&subscriptions) {
            // Never publish a truncated target list or let an oversized
            // optional context snapshot stop the supervised input reader.
            for index in (0..self.connections.len()).rev() {
                let mut connection = self.connections.remove(index);
                if let Some(bound) = connection.bound.as_mut()
                    && bound.context.value.is_some()
                {
                    bound.context.value = None;
                    bound.context.state = State::Unknown;
                    bound.context.usable = false;
                    bound.context.error =
                        Some("context snapshots exceed private transfer bounds".into());
                    self.advance_session(&mut connection, now)?;
                }
                self.connections.insert(index, connection);
            }
            return Ok(subscriptions
                .into_iter()
                .filter(|subscription| subscription.context.is_none())
                .collect());
        }
        Ok(subscriptions)
    }
    pub fn report(&mut self, report: Report, now: Duration) -> io::Result<()> {
        let subscription = report.subscription();
        let Some(index) = self.connections.iter().position(|connection| {
            connection.bound.as_ref().is_some_and(|bound| {
                bound.fence == subscription.fence
                    && bound.seat == subscription.seat
                    && bound.role == Role::Agent
                    && (bound.capabilities.contains(&Capability::Observations)
                        || self.routing.is_some()
                            && bound
                                .capabilities
                                .iter()
                                .copied()
                                .any(protocol::execution_capability))
                    && (bound.context.fresh(now)
                        || matches!(
                            &report,
                            Report::ContextLost { .. }
                                | Report::SourceInvalidated { .. }
                                | Report::RecognitionCancelled { .. }
                                | Report::RecognizerReady { ready: false, .. }
                        ))
            })
        }) else {
            return Ok(());
        };
        let mut connection = self.connections.remove(index);
        let bound = connection.bound.as_mut().unwrap();
        let verification = if now.saturating_sub(bound.renewed) >= protocol::LEASE {
            Err(protocol::invalid("session agent lease expired"))
        } else {
            bound
                .registration
                .check(&mut self.logind, &bound.client)
                .and_then(|()| self.verify_agent(connection.channel.peer().pid))
        };
        if let Err(error) = verification {
            let reason = error
                .to_string()
                .chars()
                .filter(|character| !character.is_control())
                .take(512)
                .collect();
            let _ = connection.channel.send(
                &Reply::Revoked {
                    fence: bound.fence,
                    reason,
                },
                now,
            );
            return Ok(());
        }
        match &report {
            Report::SourceInvalidated { source, .. } => {
                connection.cancel_actions(source, None)?;
                self.connections.insert(index, connection);
                return Ok(());
            }
            Report::Observation { event, .. } => {
                if let crate::rules::Event::Cancelled { source, id, .. } = event.as_ref() {
                    connection.cancel_actions(source, Some(*id))?;
                }
            }
            _ => {}
        }
        if matches!(
            &report,
            Report::Observation { .. }
                | Report::Dispatch { .. }
                | Report::Input { .. }
                | Report::RecognizerMetadata { .. }
                | Report::Recognition { .. }
                | Report::RecognizerReady { ready: true, .. }
        ) && !self.guard_observation(&mut connection, now)?
        {
            self.connections.insert(index, connection);
            return Ok(());
        }
        let bound = connection.bound.as_mut().unwrap();
        let fence = bound.fence;
        if matches!(
            &report,
            Report::Recognition { .. }
                | Report::RecognizerMetadata { .. }
                | Report::RecognizerReady { .. }
                | Report::RecognitionCancelled { .. }
        ) {
            if self
                .recognition_report(&mut connection, report, monotonic()?)
                .is_ok()
            {
                self.connections.insert(index, connection);
            }
            return Ok(());
        }
        if matches!(&report, Report::Observation { event, .. } if matches!(event.as_ref(), crate::rules::Event::Matched { evidence: crate::rules::Evidence::Custom { .. }, .. }))
            && (!bound.recognizers.requested || !bound.recognizers.ready)
        {
            self.connections.insert(index, connection);
            return Ok(());
        }
        if bound.rules.busy()
            && (matches!(&report, Report::Dispatch { .. })
                || matches!(&report,Report::Observation {event,..} if matches!(event.as_ref(),crate::rules::Event::Matched {..})))
        {
            self.connections.insert(index, connection);
            return Ok(());
        }
        if matches!(report, Report::ContextLost { .. }) {
            // Context loss also retires pending rule edits.
            if matches!(bound.context.state, State::Locking | State::Locked)
                || !bound.capabilities.contains(&Capability::Context)
            {
                self.connections.insert(index, connection);
                return Ok(());
            }
            bound.context.value = None;
            bound.context.state = State::Unknown;
            bound.context.usable = false;
            bound.context.checked = Duration::ZERO;
            bound.context.error = Some("worker context lease expired".into());
            self.advance_session(&mut connection, now)?;
            self.connections.insert(index, connection);
            return Ok(());
        }
        let reply = match report {
            Report::Ready { .. } if !bound.observing => {
                bound.observing = true;
                Reply::Subscribed { fence }
            }
            Report::Ready { .. } => {
                self.connections.insert(index, connection);
                return Ok(());
            }
            Report::Input { input, .. } if bound.observing => {
                input.validate()?;
                if !bound.capabilities.contains(&Capability::InputEvents) {
                    self.connections.insert(index, connection);
                    return Ok(());
                }
                let delivered_at = monotonic()?;
                let permitted = self.routing.is_none()
                    && self.rule_baseline.as_ref().is_some_and(|config| {
                        crate::input_events::permitted_subscription(
                            config,
                            &bound.capabilities,
                            bound.context.value.as_deref(),
                            &bound.seat,
                            fence.session,
                            delivered_at,
                        ) && input
                            .selectors()
                            .iter()
                            .all(|id| config.policy.allowed_devices.contains(id))
                    });
                if !permitted {
                    bound.context.usable = false;
                    bound.context.value = None;
                    bound.context.state = State::Unknown;
                    bound.context.error = Some("normalized input permit was revoked".into());
                    self.advance_session(&mut connection, delivered_at)?;
                    self.connections.insert(index, connection);
                    return Ok(());
                }
                if input.source().seat != bound.seat {
                    return Err(protocol::invalid(
                        "normalized input belongs to a different seat",
                    ));
                }
                if !input.fresh(delivered_at) {
                    bound.context.usable = false;
                    bound.context.value = None;
                    bound.context.state = State::Unknown;
                    bound.context.error =
                        Some("normalized input delivery exceeded freshness limit".into());
                    self.advance_session(&mut connection, now)?;
                    self.connections.insert(index, connection);
                    return Ok(());
                }
                Reply::Input { fence, input }
            }
            Report::Observation { event, .. } if bound.observing => {
                if !bound.capabilities.contains(&Capability::Observations) {
                    self.connections.insert(index, connection);
                    return Ok(());
                }
                let source = match event.as_ref() {
                    crate::rules::Event::Matched { source, .. }
                    | crate::rules::Event::Skipped { source, .. }
                    | crate::rules::Event::Cancelled { source, .. } => source,
                };
                if source.seat != bound.seat {
                    return Err(protocol::invalid(
                        "worker observation belongs to a different seat",
                    ));
                }
                Reply::Observation { fence, event }
            }
            Report::Dispatch { event, .. } if bound.observing => {
                if bound.rules.busy() {
                    self.connections.insert(index, connection);
                    return Ok(());
                }
                if !bound
                    .available_actions
                    .iter()
                    .copied()
                    .any(protocol::execution_capability)
                    || bound.actions.pending.len() >= crate::actions::MAX_RUNNING
                {
                    self.advance_session(&mut connection, now)?;
                    self.connections.insert(index, connection);
                    return Ok(());
                }
                let root = self
                    .routing
                    .as_ref()
                    .ok_or_else(|| protocol::invalid("observe broker cannot dispatch actions"))?;
                // Accepted runtime rule edits on this seat apply over the root rules.
                let config = bound.rules.current().unwrap_or(root);
                let crate::rules::Event::Matched {
                    rule,
                    source,
                    at,
                    action,
                    output,
                    ..
                } = event.as_ref()
                else {
                    return Err(protocol::invalid("worker action is not a match"));
                };
                let configured = config
                    .rules
                    .iter()
                    .find(|configured| {
                        configured.id == *rule
                            && configured.enabled
                            && config.mode_active(configured)
                            && configured.action == *action
                    })
                    .ok_or_else(|| {
                        protocol::invalid("worker action differs from the seat's configured rules")
                    })?;
                let context = bound
                    .context
                    .value
                    .as_ref()
                    .ok_or_else(|| protocol::invalid("action lacks compositor context"))?;
                if !protocol::action_capabilities(action).is_subset(&bound.capabilities)
                    || protocol::action_capabilities(action)
                        .iter()
                        .any(|capability| {
                            protocol::execution_capability(*capability)
                                && !bound.available_actions.contains(capability)
                        })
                    || !protocol::supported_session_action(action)
                    || configured
                        .output
                        .as_ref()
                        .is_some_and(|name| Some(name) != output.as_ref())
                    || source.seat != bound.seat
                    || source.generation == 0
                    || *at > now
                    || now - *at > protocol::MAX_PACKET_AGE
                    || context.snapshot.state != State::Desktop
                    || !configured
                        .contexts
                        .contains(&crate::config::Context::Desktop)
                    || output.as_ref().is_some_and(|name| {
                        !context
                            .snapshot
                            .outputs
                            .iter()
                            .any(|output| output.geometry.name == *name)
                    })
                {
                    return Err(protocol::invalid(
                        "action is unavailable for this session/context",
                    ));
                }
                Reply::Dispatch {
                    fence,
                    event: event.clone(),
                }
                .validate()?;
                bound.actions.offer(&event, &context.snapshot, now)?;
                Reply::Dispatch { fence, event }
            }
            Report::Ended { reason, .. } => {
                let _ = connection
                    .channel
                    .send(&Reply::Revoked { fence, reason }, now);
                return Ok(());
            }
            _ => return Err(protocol::invalid("worker observation preceded activation")),
        };
        if connection.push(reply).is_ok() {
            self.connections.insert(index, connection);
        }
        // Overflow/disconnection drops only this subscriber, not the worker.
        Ok(())
    }
    pub fn transport(
        &mut self,
        subscription: &Subscription,
        descriptor: &crate::transport::Descriptor,
        now: Duration,
    ) -> io::Result<Option<crate::configuration::Command>> {
        descriptor.validate().map_err(protocol::invalid)?;
        let Some(bound) = self
            .connections
            .iter_mut()
            .filter_map(|connection| connection.bound.as_mut())
            .find(|bound| {
                bound.role == Role::Agent
                    && bound.fence == subscription.fence
                    && bound.seat == subscription.seat
                    && bound.context.fresh(now)
            })
        else {
            return Ok(None);
        };
        if descriptor.source.seat != bound.seat {
            return Err(protocol::invalid(
                "transport source belongs to another seat",
            ));
        }
        // An agent that just exited gets no evidence; its connection closes on its own.
        if bound
            .registration
            .check(&mut self.logind, &bound.client)
            .is_err()
        {
            return Ok(None);
        }
        let evidence = match bound.client.device_evidence(descriptor, monotonic()?) {
            Ok(evidence) => evidence,
            Err(error) if !descriptor.is_generated_keyboard() => {
                eprintln!(
                    "mirror {} keeps no consumer evidence: {error}",
                    descriptor.name
                );
                None
            }
            Err(error) => return Err(error),
        };
        let Some(evidence) = evidence else {
            return Ok(None);
        };
        if bound
            .registration
            .check(&mut self.logind, &bound.client)
            .is_err()
        {
            return Ok(None);
        }
        let reply = transport_offer::reply(
            descriptor,
            bound.fence,
            evidence,
            &mut bound.transport_started,
        )?;
        Ok(Some(reply))
    }
    pub fn deadline(&self) -> Option<Duration> {
        self.connections
            .iter()
            .map(|connection| {
                connection.bound.as_ref().map_or(
                    connection.opened
                        + if connection.administrator {
                            crate::configuration::INSTALL_TIMEOUT
                        } else {
                            protocol::MAX_PACKET_AGE
                        },
                    |bound| {
                        (bound.checked + REFRESH)
                            .min(bound.recognizers.deadline().unwrap_or(Duration::MAX))
                            .min(bound.renewed + protocol::LEASE)
                            .min(match bound.role {
                                Role::Agent if bound.context.due => Duration::ZERO,
                                Role::Agent => bound.context.checked + CONTEXT_REFRESH,
                                _ => Duration::MAX,
                            })
                    },
                )
            })
            .min()
    }
    pub fn poll_sources(&self) -> Vec<PollFd<'_>> {
        let mut fds = vec![PollFd::new(self.endpoint.as_fd(), PollFlags::POLLIN)];
        fds.extend(self.connections.iter().map(|connection| {
            PollFd::new(
                connection.channel.as_fd(),
                PollFlags::POLLIN
                    | if connection.outgoing.is_empty() {
                        PollFlags::empty()
                    } else {
                        PollFlags::POLLOUT
                    },
            )
        }));
        fds.extend(
            self.logind
                .poll_sources()
                .into_iter()
                .map(|(fd, events)| PollFd::new(fd, PollFlags::from_bits_truncate(events))),
        );
        for bound in self
            .connections
            .iter()
            .filter_map(|connection| connection.bound.as_ref())
            .filter(|bound| bound.role == Role::Agent)
        {
            fds.extend(
                bound
                    .registration
                    .poll_sources()
                    .into_iter()
                    .chain(bound.context.sampler.poll_source())
                    .map(|fd| PollFd::new(fd, PollFlags::POLLIN)),
            );
        }
        fds
    }
    /// One incoming message/registration per step prevents a subscriber from
    /// monopolizing supervision. All sends are nonblocking and bounded.
    pub fn step(&mut self, now: Duration) -> io::Result<()> {
        self.observe();
        if let Some(channel) = self.endpoint.accept()?
            && self.connections.len() < MAX_CLIENTS
        {
            self.connections.push(Connection {
                channel,
                opened: now,
                bound: None,
                outgoing: VecDeque::new(),
                bytes: 0,
                administrator: false,
                waiting_reload: false,
            });
        }
        if self.connections.is_empty() {
            return Ok(());
        }
        self.cursor %= self.connections.len();
        let index = self.cursor;
        self.cursor += 1;
        let mut connection = self.connections.remove(index);
        let result = self.process(&mut connection, now);
        if result.is_ok() {
            self.connections.insert(index, connection);
        } else if let Err(error) = result {
            let reason: String = error
                .to_string()
                .chars()
                .filter(|character| !character.is_control())
                .take(512)
                .collect();
            let _ = connection
                .channel
                .send(&Reply::Rejected { reason }, monotonic()?);
        }
        // A bad, stale or disconnected client loses all queued work and its
        // seat owner immediately. No raw payload is logged or returned to it.
        Ok(())
    }
    fn process(&mut self, connection: &mut Connection, now: Duration) -> io::Result<()> {
        let registered = connection.bound.is_some();
        if now < connection.opened {
            return Err(protocol::invalid("broker timeline moved backward"));
        }
        if let Some(bound) = connection.bound.as_mut() {
            if now.saturating_sub(bound.renewed) >= protocol::LEASE {
                return Err(protocol::invalid("session agent lease expired"));
            }
            if now < bound.checked {
                return Err(protocol::invalid("session timeline moved backward"));
            }
            if now - bound.checked >= REFRESH {
                bound.registration.check(&mut self.logind, &bound.client)?;
                bound.checked = now;
            }
        } else if now - connection.opened
            >= if connection.administrator {
                crate::configuration::INSTALL_TIMEOUT
            } else {
                protocol::MAX_PACKET_AGE
            }
        {
            return Err(protocol::invalid("registration deadline expired"));
        }
        if let Some(packet) = connection.channel.receive::<Request>()? {
            packet.message.validate()?;
            match packet.message {
                Request::Reload {} if connection.bound.is_none() && !connection.administrator => {
                    if connection.channel.peer().uid != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "configuration reload requires an administrator",
                        ));
                    }
                    self.verify_agent(connection.channel.peer().pid)?;
                    connection.administrator = true;
                    if self.reload_pending {
                        connection.push(Reply::ReloadResult {
                            configuration: self.configuration,
                            error: Some("configuration reload is already in progress".into()),
                        })?;
                    } else {
                        connection.waiting_reload = true;
                        self.reload_requested = true;
                        self.reload_pending = true;
                    }
                }
                Request::Register {
                    display,
                    role,
                    capabilities,
                } if connection.bound.is_none() => {
                    registration_capabilities(
                        self.routing.as_ref(),
                        self.rule_baseline.as_deref(),
                        self.lua_source.is_some(),
                        self.recognizers_available(),
                        self.observations,
                        role,
                        &capabilities,
                    )?;
                    let peer = Peer::from_socket(&connection.channel)?;
                    self.verify_agent(peer.pid())?;
                    let client = Compositor::open(
                        &self.policy.runtime_root.join(peer.uid().to_string()),
                        &display,
                        peer.uid(),
                        &self.policy.compositor_executable,
                    )?;
                    let registration = self.logind.register(peer, &client)?;
                    self.verify_agent(connection.channel.peer().pid)?;
                    let binding = registration.binding()?;
                    if !self.policy.allowed_seats.contains(&binding.seat)
                        || role == Role::Agent
                            && self.connections.iter().any(|other| {
                                other.bound.as_ref().is_some_and(|bound| {
                                    bound.role == Role::Agent && bound.seat == binding.seat
                                })
                            })
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "seat is forbidden or already has an agent",
                        ));
                    }
                    self.session = self
                        .session
                        .checked_add(1)
                        .ok_or_else(|| protocol::invalid("session generation exhausted"))?;
                    let fence = Fence {
                        broker: self.epoch,
                        configuration: self.configuration,
                        session: self.session,
                    };
                    let seat = binding.seat.clone();
                    let session = binding.session.clone();
                    let compositor = (binding.compositor_pid, binding.compositor_start);
                    let context = ContextFeed::default();
                    connection.bound = Some(Bound {
                        transport_started: false,
                        client,
                        registration,
                        seat: seat.clone(),
                        role,
                        fence,
                        renewed: now,
                        checked: now,
                        capabilities: capabilities.clone(),
                        observing: false,
                        previous: Grace::default(),
                        context,
                        compositor,
                        context_export: None,
                        rules: rule_control::Rules::new(self.rule_baseline.clone()),
                        recognizers: Default::default(),
                        actions: ActionLedger::default(),
                        available_actions: capabilities
                            .iter()
                            .filter(|capability| **capability == Capability::Commands)
                            .copied()
                            .collect(),
                    });
                    connection.push(Reply::Welcome {
                        fence,
                        seat,
                        session,
                        mode: if self.routing.is_some() {
                            Mode::Route
                        } else {
                            Mode::Observe
                        },
                        capabilities,
                    })?;
                }
                request => {
                    let bound = connection.bound.as_mut().ok_or_else(|| {
                        protocol::invalid("register before requesting control state")
                    })?;
                    let current = request.fence() == Some(bound.fence);
                    let old_heartbeat = bound.previous.permits(&request, now);
                    // An in-flight read or action report may precede a transition
                    // notice. Discard it without acting on stale authority or
                    // closing the agent before that notice can reach it.
                    let old_status = bound.previous.discards(&request, now);
                    if !current && !old_heartbeat && !old_status {
                        return Err(protocol::invalid(
                            "stale broker/configuration/session generation",
                        ));
                    }
                    bound.registration.check(&mut self.logind, &bound.client)?;
                    self.verify_agent(connection.channel.peer().pid)?;
                    bound.checked = now;
                    if current {
                        bound.previous = Grace::default();
                    }
                    let fence = bound.fence;
                    match request {
                        Request::RecognizerReady { .. }
                        | Request::Recognize { .. }
                        | Request::RecognitionCancelled { .. }
                            if !current => {}
                        Request::RecognizerReady { ready, .. } => {
                            self.recognizer_ready(connection, fence, ready, now)?
                        }
                        Request::Recognize { proposal, .. } => {
                            self.prepare_recognition(connection, fence, proposal, now)?
                        }
                        Request::RecognitionCancelled { cancellation, .. } => {
                            if !bound.capabilities.contains(&Capability::CustomRecognizers) {
                                return Err(protocol::invalid("unnegotiated recognizer cleanup"));
                            }
                            bound.recognizers.cleaned(&cancellation)?;
                        }
                        Request::Rules { .. } if !current => {}
                        Request::Rules {
                            fence: incoming,
                            proposal,
                        } => {
                            self.prepare_rules(connection, incoming, proposal, now)?;
                        }
                        Request::LuaSource { .. } if !current => {}
                        Request::ContextRead { .. } if !current => {}
                        Request::LuaSource { offset, .. }
                            if bound.role == Role::Agent
                                && bound.capabilities.contains(&Capability::LuaEvents) =>
                        {
                            let reply = crate::lua::source_transfer::chunk(
                                fence,
                                self.lua_source.as_deref(),
                                offset,
                            )?;
                            connection.push(reply)?;
                        }
                        Request::ContextRead { offset, .. }
                            if bound.role == Role::Agent
                                && bound.capabilities.contains(&Capability::Context) =>
                        {
                            if offset == 0 && !self.refresh_now(connection)? {
                                return Ok(());
                            }
                            let bound = connection.bound.as_mut().unwrap();
                            let guarded = if offset == 0 {
                                bound.context.fresh(monotonic()?)
                            } else {
                                bound
                                    .context_export
                                    .as_ref()
                                    .is_some_and(|export| export.contains_context)
                            };
                            if guarded && !self.guard_observation(connection, now)? {
                                return Ok(());
                            }
                            let now = monotonic()?;
                            if offset == 0 {
                                let bound = connection.bound.as_mut().unwrap();
                                let context = bound
                                    .context
                                    .fresh(now)
                                    .then(|| bound.context.value.clone())
                                    .flatten();
                                let view = crate::context_transfer::View {
                                    fence: bound.fence,
                                    sampled_at: now,
                                    state: bound.context.state(now),
                                    context,
                                    error: bound.context.error.as_ref().map(|error| {
                                        error
                                            .chars()
                                            .filter(|character| !character.is_control())
                                            .take(128)
                                            .collect()
                                    }),
                                };
                                bound.context_export =
                                    Some(crate::context_transfer::Export::new(&view, now)?);
                            }
                            let bound = connection.bound.as_mut().unwrap();
                            let export = bound.context_export.as_ref().ok_or_else(|| {
                                protocol::invalid("context transfer has no initial request")
                            })?;
                            let (bytes, data) = export.chunk(bound.fence, offset, now)?;
                            connection.push(Reply::ContextData {
                                fence,
                                offset,
                                bytes,
                                data,
                            })?;
                        }
                        Request::Heartbeat { .. } => {
                            bound.renewed = now;
                            let revision = bound
                                .context
                                .value
                                .as_ref()
                                .map_or(0, |value| value.snapshot.revision);
                            connection.push(Reply::Accepted { fence, revision })?;
                        }
                        Request::Status { .. } if !current => {}
                        Request::Status { .. } => {
                            let fence = bound.fence;
                            let provider = self
                                .connections
                                .iter()
                                .filter_map(|other| other.bound.as_ref())
                                .find(|other| {
                                    other.role == Role::Agent
                                        && other.compositor == bound.compositor
                                })
                                .unwrap_or(bound);
                            let state = provider.context.state(now);
                            let revision = provider
                                .context
                                .value
                                .as_ref()
                                .map_or(0, |value| value.snapshot.revision);
                            let context_error = provider.context.error.clone();
                            connection.push(Reply::Status {
                                fence,
                                state,
                                revision,
                                pending: connection.outgoing.len(),
                                last_reload_error: self.last_reload_error.clone(),
                                context_error,
                            })?;
                        }
                        Request::Disconnect { .. } => {
                            return Err(io::ErrorKind::UnexpectedEof.into());
                        }
                        Request::Authorize { .. }
                        | Request::Complete { .. }
                        | Request::Availability { .. }
                            if !current => {}
                        Request::Authorize { id, ordinal, .. } => {
                            if !bound
                                .capabilities
                                .iter()
                                .copied()
                                .any(protocol::execution_capability)
                                || self.routing.is_none()
                            {
                                return Err(protocol::invalid(
                                    "action execution was not negotiated",
                                ));
                            }
                            if bound
                                .actions
                                .pending
                                .get(&(id, ordinal))
                                .is_some_and(|offer| offer.cancelled)
                            {
                                connection.push(Reply::ActionCancelled { fence, id, ordinal })?;
                                return connection.flush(monotonic()?);
                            }
                            if !self.refresh_now(connection)?
                                || !self.guard_observation(connection, monotonic()?)?
                            {
                                return Ok(());
                            }
                            let bound = connection.bound.as_mut().unwrap();
                            let now = monotonic()?;
                            let context = bound.context.value.as_ref().ok_or_else(|| {
                                protocol::invalid("action context is unavailable")
                            })?;
                            let offer =
                                bound.actions.pending.get(&(id, ordinal)).ok_or_else(|| {
                                    protocol::invalid("action grant lacks an offer")
                                })?;
                            if offer.cancelled {
                                return connection.flush(now);
                            }
                            if !offer.capabilities_available(
                                &bound.capabilities,
                                &bound.available_actions,
                            ) {
                                bound
                                    .actions
                                    .pending
                                    .get_mut(&(id, ordinal))
                                    .unwrap()
                                    .cancelled = true;
                                connection.push(Reply::ActionCancelled { fence, id, ordinal })?;
                                return connection.flush(monotonic()?);
                            }
                            bound
                                .actions
                                .authorize(id, ordinal, &context.snapshot, now)?;
                            let expires = (now + DECISION_MAX_AGE)
                                .min(context.snapshot.captured_at + DECISION_MAX_AGE);
                            connection.push(Reply::Execute {
                                fence,
                                id,
                                ordinal,
                                expires,
                            })?;
                        }
                        Request::Complete {
                            id,
                            ordinal,
                            result,
                            ..
                        } => {
                            if matches!(
                                result,
                                crate::protocol::Completion::Cancelled
                                    | crate::protocol::Completion::Unavailable
                            ) {
                                bound.actions.complete_unexecuted(id, ordinal)?;
                            } else {
                                bound.actions.complete(id, ordinal)?;
                            }
                            connection.outgoing.retain(|reply| {
                                !matches!(reply,
                                Reply::Execute { id: offered_id, ordinal: offered_ordinal, .. }
                                if *offered_id == id && *offered_ordinal == ordinal)
                            });
                            connection.bytes = connection
                                .outgoing
                                .iter()
                                .map(reply_size)
                                .sum::<io::Result<usize>>()?;
                        }
                        Request::Availability { capabilities, .. } => {
                            if capabilities.iter().any(|capability| {
                                !protocol::execution_capability(*capability)
                                    || !bound.capabilities.contains(capability)
                            }) {
                                return Err(protocol::invalid(
                                    "action availability exceeds negotiated capabilities",
                                ));
                            }
                            bound.available_actions = capabilities;
                        }
                        _ => {
                            return Err(protocol::invalid(
                                "operation requires an unavailable provider capability",
                            ));
                        }
                    }
                }
            }
        }
        if registered {
            self.refresh_context(connection, monotonic()?)?;
        }
        let context_delivery =
            matches!(connection.outgoing.front(), Some(Reply::ContextData { .. }))
                && connection.bound.as_ref().is_some_and(|bound| {
                    bound
                        .context_export
                        .as_ref()
                        .is_some_and(|export| export.contains_context)
                });
        if context_delivery
            || matches!(
                connection.outgoing.front(),
                Some(
                    Reply::Observation { .. }
                        | Reply::Dispatch { .. }
                        | Reply::Execute { .. }
                        | Reply::Input { .. }
                        | Reply::RecognizerMetadata { .. }
                        | Reply::Recognition { .. }
                )
            )
        {
            self.guard_observation(connection, monotonic()?)?;
        }
        // Queued input must still have a live permit at the actual send time,
        // including time spent waiting for the independent lock query above.
        let input_expired = if let Some(Reply::Input { input, .. }) = connection.outgoing.front() {
            let now = monotonic()?;
            let bound = connection.bound.as_ref().unwrap();
            !input.fresh(now)
                || !bound.context.fresh(now)
                || !self.rule_baseline.as_ref().is_some_and(|config| {
                    crate::input_events::permitted_subscription(
                        config,
                        &bound.capabilities,
                        bound.context.value.as_deref(),
                        &bound.seat,
                        bound.fence.session,
                        now,
                    ) && input.source().seat == bound.seat
                        && input
                            .selectors()
                            .iter()
                            .all(|id| config.policy.allowed_devices.contains(id))
                })
        } else {
            false
        };
        if input_expired {
            let bound = connection.bound.as_mut().unwrap();
            bound.context.value = None;
            bound.context.usable = false;
            bound.context.state = State::Unknown;
            bound.context.error = Some("normalized input permit expired before send".into());
            self.advance_session(connection, monotonic()?)?;
        }
        if let Some(Reply::RecognizerMetadata { metadata, .. }) = connection.outgoing.front() {
            let now = monotonic()?;
            let bound = connection.bound.as_ref().unwrap();
            if !recognizers::permitted(self.rule_baseline.as_deref(), bound, now)
                || !bound.recognizers.requested
                || !bound.recognizers.ready
                || !metadata.fresh(now)
            {
                self.advance_session(connection, now)?;
            }
        }
        if let Some(Reply::Recognition { ticket, .. }) = connection.outgoing.front() {
            let now = monotonic()?;
            let bound = connection.bound.as_ref().unwrap();
            if now >= ticket.deadline
                || !recognizers::permitted(self.rule_baseline.as_deref(), bound, now)
                || !bound.recognizers.requested
                || !bound.recognizers.ready
            {
                self.advance_session(connection, now)?;
            }
        }
        connection.flush(monotonic()?)
    }
    fn guard_observation(
        &mut self,
        connection: &mut Connection,
        now: Duration,
    ) -> io::Result<bool> {
        let bound = connection.bound.as_mut().unwrap();
        if !bound.context.fresh_after_sample(now) {
            if !matches!(bound.context.state, State::Locking | State::Locked) {
                bound.context.state = State::Unknown;
            }
            bound.context.usable = false;
            bound.context.value = None;
            bound.context.checked = Duration::ZERO;
            bound.context.error =
                Some("compositor context expired before observation delivery".into());
            self.advance_session(connection, now)?;
            return Ok(false);
        }
        let result = bound.registration.locked(&mut self.logind);
        bound.registration.check(&mut self.logind, &bound.client)?;
        match result {
            Ok(false) if bound.context.fresh(monotonic()?) => return Ok(true),
            Ok(true) => {
                bound.context.state = State::Locked;
                bound.context.usable = false;
                bound.context.value = None;
                bound.context.error = Some("session is locked".into());
            }
            Ok(false) | Err(_) => {
                bound.context.state = State::Unknown;
                bound.context.usable = false;
                bound.context.value = None;
                bound.context.error = Some("session lock state is unavailable".into());
            }
        }
        self.advance_session(connection, monotonic()?)?;
        Ok(false)
    }
    fn advance_session(&mut self, connection: &mut Connection, now: Duration) -> io::Result<()> {
        self.session = self
            .session
            .checked_add(1)
            .ok_or_else(|| protocol::invalid("session generation exhausted"))?;
        self.published = true;
        let bound = connection.bound.as_mut().unwrap();
        bound.previous.remember(bound.fence, now)?;
        bound.fence.session = self.session;
        if let Some(context) = bound.context.value.as_mut() {
            context.snapshot.generation = self.session;
        }
        bound.observing = false;
        bound.actions = ActionLedger::default();
        bound.rules = rule_control::Rules::new(self.rule_baseline.clone());
        bound.recognizers = Default::default();
        bound.context_export = None;
        let fence = bound.fence;
        let state = bound.context.state;
        connection.outgoing.clear();
        connection.bytes = 0;
        connection.push(Reply::SessionChanged { fence, state })
    }
    /// Refreshes an agent's context before it is acted on; whether its session
    /// fence is unchanged afterwards.
    fn refresh_now(&mut self, connection: &mut Connection) -> io::Result<bool> {
        let fence = connection.bound.as_ref().unwrap().fence;
        if connection.bound.as_ref().unwrap().role == Role::Agent {
            self.sample_context(connection, monotonic()?)?;
        }
        Ok(connection
            .bound
            .as_ref()
            .is_some_and(|bound| bound.fence == fence))
    }
    /// Samples an agent's context once a change notification marked it due or
    /// its keepalive passed; only a notified change is published.
    fn refresh_context(&mut self, connection: &mut Connection, now: Duration) -> io::Result<()> {
        let bound = connection.bound.as_mut().unwrap();
        if bound.role != Role::Agent
            || !bound.context.due && now < bound.context.checked + CONTEXT_REFRESH
        {
            return Ok(());
        }
        self.published |= std::mem::take(&mut bound.context.due);
        self.sample_context(connection, now)
    }
    fn sample_context(&mut self, connection: &mut Connection, now: Duration) -> io::Result<()> {
        let bound = connection.bound.as_mut().unwrap();
        bound.registration.check(&mut self.logind, &bound.client)?;
        let binding = bound.registration.binding()?.clone();
        let was_fresh = bound.context.fresh(now);
        let old_usable = bound.context.usable;
        let old_state = bound.context.state;
        let socket = self
            .policy
            .runtime_root
            .join(binding.uid.to_string())
            .join(bound.client.display());
        let result = bound
            .registration
            .locked(&mut self.logind)
            .and_then(|locked| {
                if !bound.capabilities.contains(&Capability::Context) {
                    return Ok((locked, None));
                }
                let until = std::time::Instant::now() + CONTEXT_SAMPLE_TIME;
                let snapshot = bound.context.sampler.sample(
                    &socket,
                    binding.uid,
                    bound.fence.session,
                    binding.kind,
                    locked,
                    until,
                )?;
                Ok((
                    locked,
                    Some(ProvidedContext {
                        binding: binding.clone(),
                        snapshot,
                    }),
                ))
            });
        bound.registration.check(&mut self.logind, &bound.client)?;
        match result {
            Ok((true, _)) => {
                bound.context.value = None;
                bound.context.state = State::Locked;
                bound.context.usable = false;
                bound.context.error = Some("session is locked".into());
            }
            Ok((false, snapshot)) => {
                bound.context.state = snapshot
                    .as_ref()
                    .map_or(State::Unknown, |provided| provided.snapshot.state);
                bound.context.value = snapshot.map(Box::new);
                bound.context.usable = true;
                bound.context.error = None;
            }
            Err(error) => {
                bound.context.value = None;
                bound.context.state = State::Unknown;
                bound.context.usable = false;
                bound.context.error = Some(
                    error
                        .to_string()
                        .chars()
                        .filter(|character| !character.is_control())
                        .take(512)
                        .collect(),
                );
            }
        }
        let checked = monotonic()?;
        bound.context.checked = checked;
        if bound.context.value.as_ref().is_some_and(|value| {
            checked.saturating_sub(value.snapshot.captured_at) > DECISION_MAX_AGE
        }) {
            bound.context.value = None;
            bound.context.state = State::Unknown;
            bound.context.usable = false;
            bound.context.error = Some("compositor snapshot exceeded freshness budget".into());
        }
        let transition = old_state != bound.context.state
            || old_usable != bound.context.usable
            || old_usable && !was_fresh;
        if transition {
            self.advance_session(connection, checked)?;
        }
        Ok(())
    }
    fn verify_agent(&self, pid: u32) -> io::Result<()> {
        let expected = fs::metadata(&self.policy.agent_executable)?;
        let actual = fs::metadata(format!("/proc/{pid}/exe"))?;
        if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "session peer executable differs from broker policy",
            ));
        }
        Ok(())
    }
}
impl AsFd for Broker {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.endpoint.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Action, Direction},
        gestures::{Phase, Point, Space, TouchGesture},
        input::Source,
        rules::{Event, Evidence, Stage},
    };
    #[test]
    fn a_context_sampled_after_the_caller_took_its_time_stays_fresh() {
        let ms = Duration::from_millis;
        let context = ContextFeed {
            usable: true,
            state: State::Desktop,
            checked: ms(10_000),
            ..ContextFeed::default()
        };
        // The caller read its clock just before refreshing the context.
        assert!(!context.fresh(ms(9_999)));
        assert!(context.fresh_after_sample(ms(9_999)));
        assert!(context.fresh_after_sample(ms(10_000) + SNAPSHOT_MAX_AGE));
        assert!(!context.fresh_after_sample(ms(10_001) + SNAPSHOT_MAX_AGE));
        let unusable = ContextFeed {
            usable: false,
            ..context
        };
        assert!(!unusable.fresh_after_sample(ms(9_999)));
    }
    #[test]
    fn action_receipts_require_a_fresh_offer_grant_and_actual_delivery() {
        let fence = Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 1,
        };
        let Reply::Observation { event, .. } = progress(fence, 1, Stage::Begin) else {
            unreachable!()
        };
        let snapshot = crate::context::Snapshot {
            generation: 1,
            revision: 1,
            captured_at: Duration::ZERO,
            state: State::Desktop,
            outputs: vec![],
        };
        let mut actions = ActionLedger::default();
        assert!(actions.authorize(1, 1, &snapshot, Duration::ZERO).is_err());
        actions.offer(&event, &snapshot, Duration::ZERO).unwrap();
        assert!(actions.offer(&event, &snapshot, Duration::ZERO).is_err());
        assert!(actions.complete(1, 1).is_err());
        actions.authorize(1, 1, &snapshot, Duration::ZERO).unwrap();
        assert!(actions.authorize(1, 1, &snapshot, Duration::ZERO).is_err());
        assert!(
            actions.complete(1, 1).is_err(),
            "unsent grant accepted a receipt"
        );
        actions.pending.get_mut(&(1, 1)).unwrap().sent = true;
        actions.complete(1, 1).unwrap();
        assert!(actions.complete(1, 1).is_err());

        for (at, state, generation) in [
            (250, State::Desktop, 1),
            (1000, State::Desktop, 1),
            (1, State::Locked, 1),
            (1, State::Desktop, 2),
        ] {
            let mut actions = ActionLedger::default();
            actions.offer(&event, &snapshot, Duration::ZERO).unwrap();
            let changed = crate::context::Snapshot {
                state,
                generation,
                ..snapshot.clone()
            };
            assert!(
                actions
                    .authorize(1, 1, &changed, Duration::from_millis(at))
                    .is_err()
            );
        }
    }
    #[test]
    fn an_absent_frozen_target_cannot_match_an_absent_current_target() {
        let at = Duration::from_secs(1);
        let view = crate::context_transfer::tests::view(at);
        let fence = view.fence;
        let snapshot = view.context.unwrap().snapshot;
        let Reply::Observation { mut event, .. } = progress(fence, 1, Stage::Begin) else {
            unreachable!()
        };
        let Event::Matched { output, .. } = event.as_mut() else {
            unreachable!()
        };
        *output = Some("missing".into());
        let mut actions = ActionLedger::default();
        actions.offer(&event, &snapshot, at).unwrap();
        assert!(actions.authorize(1, 1, &snapshot, at).is_err());
    }
    #[test]
    fn cancelled_offers_are_scoped_and_unsent_offers_cannot_reappear() {
        let fence = Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 1,
        };
        let Reply::Observation { event, .. } = progress(fence, 1, Stage::Begin) else {
            unreachable!()
        };
        let snapshot = crate::context::Snapshot {
            generation: 1,
            revision: 1,
            captured_at: Duration::ZERO,
            state: State::Desktop,
            outputs: vec![],
        };
        let Event::Matched { source, .. } = event.as_ref() else {
            unreachable!()
        };
        let source = source.clone();
        let mut other = event.clone();
        let Event::Matched {
            id,
            source: other_source,
            ..
        } = other.as_mut()
        else {
            unreachable!()
        };
        *id = 2;
        other_source.device += 1;
        let mut actions = ActionLedger::default();
        actions.offer(&event, &snapshot, Duration::ZERO).unwrap();
        actions.offer(&other, &snapshot, Duration::ZERO).unwrap();
        actions.pending.get_mut(&(1, 1)).unwrap().delivered = true;
        assert_eq!(actions.cancel(&source, Some(1)), vec![((1, 1), true)]);
        assert!(actions.authorize(1, 1, &snapshot, Duration::ZERO).is_err());
        assert!(
            actions.complete(1, 1).is_err(),
            "ungranted cancellation must not become Applied"
        );
        actions.complete_unexecuted(1, 1).unwrap();
        assert!(actions.complete_unexecuted(1, 1).is_err());
        assert!(actions.offer(&event, &snapshot, Duration::ZERO).is_err());
        actions.authorize(2, 1, &snapshot, Duration::ZERO).unwrap();

        let Event::Matched { source, .. } = other.as_ref() else {
            unreachable!()
        };
        assert_eq!(actions.cancel(source, None), vec![((2, 1), false)]);
        assert!(actions.pending.is_empty());
        assert!(
            actions.complete_unexecuted(2, 1).is_err(),
            "undelivered offers have no receipt"
        );
    }

    #[test]
    fn heartbeat_grace_accepts_only_issued_fences_and_never_control_requests() {
        let fence = Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 3,
        };
        let next = Fence {
            configuration: 2,
            session: 5,
            ..fence
        };
        let mut grace = Grace::default();
        grace.remember(fence, Duration::ZERO).unwrap();
        grace.remember(next, Duration::from_millis(500)).unwrap();
        assert!(grace.permits(&Request::Heartbeat { fence }, Duration::from_millis(999)));
        assert!(grace.permits(
            &Request::Heartbeat { fence: next },
            Duration::from_millis(999)
        ));
        assert!(!grace.permits(
            &Request::Heartbeat {
                fence: Fence {
                    session: 4,
                    ..fence
                }
            },
            Duration::ZERO
        ));
        assert!(!grace.permits(&Request::Status { fence }, Duration::ZERO));
        assert!(!grace.permits(&Request::LockRequested { fence }, Duration::ZERO));
        assert!(!grace.permits(&Request::Heartbeat { fence }, Duration::from_secs(1)));
        grace.remember(next, Duration::from_secs(1)).unwrap();
        assert!(!grace.permits(&Request::Heartbeat { fence }, Duration::from_secs(1)));
    }
    #[test]
    fn key_output_is_negotiated_only_beside_context_for_enabled_routed_key_rules() {
        let mut config = crate::config::Config::preset("desktop").unwrap();
        config.mode = crate::config::Mode::Route;
        config.rules.truncate(1);
        config.rules[0].enabled = true;
        config.rules[0].action = Action::Keys {
            keys: vec![crate::config::Key(115)],
        };
        let keys = BTreeSet::from([
            Capability::Observations,
            Capability::Context,
            Capability::SyntheticInput,
        ]);
        let negotiate = |routing: Option<&crate::config::Config>, capabilities| {
            registration_capabilities(routing, None, false, false, true, Role::Agent, capabilities)
        };
        assert!(negotiate(Some(&config), &keys).is_ok());
        assert!(negotiate(None, &keys).is_err());
        let mut contextless = keys.clone();
        contextless.remove(&Capability::Context);
        assert!(negotiate(Some(&config), &contextless).is_err());
        let mut commands = config.clone();
        commands.rules[0].action = Action::Command {
            argv: vec!["true".into()],
        };
        assert!(negotiate(Some(&commands), &keys).is_err());
        config.rules[0].enabled = false;
        assert!(negotiate(Some(&config), &keys).is_err());
    }
    #[test]
    fn grace_discards_in_flight_reads_and_action_reports_but_not_control_requests() {
        let fence = Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 3,
        };
        let unissued = Fence {
            session: 4,
            ..fence
        };
        let mut grace = Grace::default();
        grace.remember(fence, Duration::ZERO).unwrap();
        let reports = |fence| {
            [
                Request::Status { fence },
                Request::Availability {
                    fence,
                    capabilities: [Capability::Commands].into(),
                },
                Request::Authorize {
                    fence,
                    id: 1,
                    ordinal: 1,
                },
                Request::Complete {
                    fence,
                    id: 1,
                    ordinal: 1,
                    result: protocol::Completion::Applied,
                },
            ]
        };
        let late = Duration::from_millis(999);
        for request in reports(fence) {
            assert!(grace.discards(&request, late), "{request:?}");
            assert!(!grace.permits(&request, late), "{request:?}");
            assert!(
                !grace.discards(&request, Duration::from_secs(1)),
                "{request:?}"
            );
        }
        for request in reports(unissued) {
            assert!(!grace.discards(&request, Duration::ZERO), "{request:?}");
        }
        assert!(!grace.discards(&Request::Heartbeat { fence }, Duration::ZERO));
        assert!(!grace.discards(&Request::LockRequested { fence }, Duration::ZERO));
    }
    #[test]
    fn context_state_expires_with_freshness_except_a_reported_lock() {
        let mut feed = ContextFeed::default();
        assert!(!feed.fresh(Duration::ZERO));
        feed.usable = true;
        feed.checked = Duration::from_millis(10);
        feed.state = State::Desktop;
        assert_eq!(feed.state(Duration::from_millis(20)), State::Desktop);
        let expired = feed.checked + SNAPSHOT_MAX_AGE + Duration::from_millis(1);
        assert_eq!(feed.state(expired), State::Unknown);
        assert!(!feed.fresh(Duration::ZERO));
        feed.state = State::Locked;
        feed.usable = false;
        assert!(!feed.fresh(Duration::from_millis(20)));
        assert_eq!(feed.state(expired * 2), State::Locked);
    }
    #[test]
    fn private_context_header_limit_rejects_the_whole_snapshot() {
        let subscription = Subscription {
            seat: "seat0".into(),
            fence: Fence {
                broker: Epoch([1, 2]),
                configuration: 1,
                session: 3,
            },
            context: None,
            capabilities: BTreeSet::new(),
            consumer: None,
        };
        assert!(subscriptions_fit(std::slice::from_ref(&subscription)));
        let mut oversized = subscription;
        oversized.context = Some(Box::new(ProvidedContext {
            binding: crate::context::Binding {
                seat: "seat0".into(),
                session: "verified".into(),
                uid: 1000,
                compositor_pid: 42,
                compositor_start: 123,
                kind: crate::context::SessionKind::User,
            },
            snapshot: crate::context::Snapshot {
                generation: 3,
                revision: 1,
                captured_at: Duration::ZERO,
                state: State::Desktop,
                outputs: vec![],
            },
        }));
        oversized.context.as_mut().unwrap().binding.session = "x".repeat(protocol::MAX_PACKET);
        assert!(!subscriptions_fit(&[oversized]));
    }
    fn progress(fence: Fence, ordinal: u64, stage: Stage) -> Reply {
        let phase = match stage {
            Stage::Begin => Phase::Begin,
            Stage::End => Phase::End,
            _ => Phase::Update,
        };
        Reply::Observation {
            fence,
            event: Box::new(Event::Matched {
                version: 1,
                rule: "continuous".into(),
                id: 1,
                ordinal,
                source: Source {
                    device: 1,
                    generation: 1,
                    seat: "seat0".into(),
                },
                at: Duration::from_millis(ordinal),
                config_generation: fence.configuration,
                session_generation: fence.session,
                stage,
                action: Action::Event {
                    name: "swipe".into(),
                },
                evidence: Evidence::Touch {
                    space: Space::Normalized,
                    gesture: TouchGesture::Pan {
                        fingers: 1,
                        phase,
                        direction: Direction::Right,
                        total: Point {
                            x: ordinal as f64,
                            y: 0.0,
                        },
                        delta: Point { x: 1.0, y: 0.0 },
                        velocity: Point { x: 1.0, y: 0.0 },
                        centroid: Point { x: 0.5, y: 0.5 },
                        swipe: true,
                    },
                },
                output: None,
            }),
        }
    }
    #[test]
    fn queued_progress_coalesces_and_preserves_begin_latest_update_and_end() {
        let (channel, mut receiver) = Channel::pair().unwrap();
        let mut connection = Connection {
            channel,
            opened: Duration::ZERO,
            bound: None,
            outgoing: VecDeque::new(),
            bytes: 0,
            administrator: false,
            waiting_reload: false,
        };
        let fence = Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 3,
        };
        connection.push(progress(fence, 1, Stage::Begin)).unwrap();
        for ordinal in 2..=129 {
            connection
                .push(progress(fence, ordinal, Stage::Update))
                .unwrap();
        }
        connection.push(progress(fence, 130, Stage::End)).unwrap();
        assert_eq!(connection.outgoing.len(), 3);
        for (sequence, ordinal) in [1, 129, 130].into_iter().enumerate() {
            connection.flush(monotonic().unwrap()).unwrap();
            let packet = receiver.receive::<Reply>().unwrap().unwrap();
            assert_eq!(packet.sequence, sequence as u64 + 1);
            assert!(
                matches!(packet.message, Reply::Observation { event, .. } if matches!(event.as_ref(), Event::Matched { ordinal: actual, .. } if *actual == ordinal))
            );
        }
        assert_eq!(connection.bytes, 0);
        assert!(connection.outgoing.is_empty());
    }
}
