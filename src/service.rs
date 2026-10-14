//! Authenticated, bounded broker control plane. Device descriptors belong to a
//! separate supervised worker; this service performs no desktop actions.
//! Desktop context comes from the bound compositor, never client snapshots.
//! Membership alone never establishes an unlocked desktop.

use crate::{
    context::{SNAPSHOT_MAX_AGE, State},
    hyprland::{Client, ContextSampler},
    ipc::{Channel, Endpoint},
    protocol::{self, Capability, Epoch, Fence, Mode, Reply, Request, Role},
    session::{Logind, Peer, Registration},
    worker::{ProvidedContext, Report, Subscription},
};
use nix::{
    libc,
    poll::{PollFd, PollFlags},
};
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
pub const REFRESH: Duration = Duration::from_millis(250);
pub const CONTEXT_REFRESH: Duration = Duration::from_millis(100);

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
}

struct ContextFeed {
    sampler: ContextSampler,
    value: Option<Box<ProvidedContext>>,
    state: State,
    usable: bool,
    checked: Duration,
    error: Option<String>,
}
impl Default for ContextFeed {
    fn default() -> Self {
        Self {
            sampler: ContextSampler::default(),
            value: None,
            state: State::Unknown,
            usable: false,
            checked: Duration::ZERO,
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
    fn state(&self, now: Duration) -> State {
        if matches!(self.state, State::Locking | State::Locked) || self.fresh(now) {
            self.state
        } else {
            State::Unknown
        }
    }
    fn inhibit(&mut self) {
        if self.state != State::Locked {
            self.state = State::Locking;
        }
        self.usable = false;
        self.value = None;
        self.error =
            Some("compositor lock requires a completed bound-provider unlock transition".into());
    }
}

pub use crate::ipc::monotonic;
fn epoch() -> io::Result<Epoch> {
    let mut bytes = [0u8; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        // SAFETY: initialized bounded output and the kernel random source.
        let count = unsafe {
            libc::getrandom(
                bytes[filled..].as_mut_ptr().cast(),
                bytes.len() - filled,
                libc::GRND_NONBLOCK,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        filled += count as usize;
    }
    let value = Epoch([
        u64::from_ne_bytes(bytes[..8].try_into().unwrap()),
        u64::from_ne_bytes(bytes[8..].try_into().unwrap()),
    ]);
    if value.0 == [0, 0] {
        return Err(protocol::invalid("empty random broker epoch"));
    }
    Ok(value)
}

pub struct Policy {
    pub runtime_root: PathBuf,
    pub compositor_executable: PathBuf,
    pub agent_executable: PathBuf,
    pub morf_executable: Option<PathBuf>,
    pub allowed_seats: BTreeSet<String>,
}
struct Bound {
    client: Client,
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
    native: Option<crate::morf::Client>,
    actions: ActionLedger,
    available_actions: BTreeSet<Capability>,
    context_export: Option<crate::context_transfer::Export>,
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
    window: Option<String>,
    deadline: Duration,
    granted: bool,
    sent: bool,
    delivered: bool,
    cancelled: bool,
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
            window,
            source,
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
                window: window.clone(),
                deadline: now + protocol::MAX_PACKET_AGE,
                granted: false,
                sent: false,
                delivered: false,
                cancelled: false,
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
        let output = |snapshot: &crate::context::Snapshot| {
            snapshot
                .outputs
                .iter()
                .find(|output| Some(&output.geometry.name) == offer.output.as_ref())
                .cloned()
        };
        let window = |snapshot: &crate::context::Snapshot| {
            snapshot
                .windows
                .iter()
                .find(|window| Some(&window.id) == offer.window.as_ref())
                .cloned()
        };
        if offer.cancelled
            || offer.granted
            || now >= offer.deadline
            || snapshot.captured_at > now
            || now.saturating_sub(snapshot.captured_at) >= SNAPSHOT_MAX_AGE
            || snapshot.state != State::Desktop
            || snapshot.secure_overlay
            || snapshot.generation != offer.snapshot.generation
            || offer.output.is_some() && output(snapshot) != output(&offer.snapshot)
            || offer.window.is_some() && window(snapshot) != window(&offer.snapshot)
            || offer.output.is_some() && snapshot.regions != offer.snapshot.regions
        {
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
    #[derive(serde::Serialize)]
    struct Update<'a> {
        revision: u64,
        seats: &'a [Subscription],
    }
    #[derive(serde::Serialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum Command<'a> {
        Subscriptions { update: Update<'a> },
    }
    protocol::encode(&protocol::Packet {
        version: protocol::VERSION,
        sequence: u64::MAX,
        at: Duration::MAX,
        message: Command::Subscriptions {
            update: Update {
                revision: u64::MAX,
                seats,
            },
        },
    })
    .is_ok()
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
    inhibited: BTreeSet<(u32, u64)>,
    lock_proofs: BTreeMap<(u32, u64), crate::morf::Proof>,
    completed_providers: BTreeSet<crate::morf::Proof>,
    routing: Option<crate::config::Config>,
    lua_source: Option<Vec<u8>>,
}
impl Broker {
    /// Context is sampled directly from a verified compositor. Action dispatch
    /// and synthetic input require separate routing/provider capabilities.
    pub fn open(endpoint: Endpoint, library: &std::path::Path, policy: Policy) -> io::Result<Self> {
        if policy.allowed_seats.is_empty()
            || !policy.runtime_root.is_absolute()
            || !policy.agent_executable.is_absolute()
            || !policy.compositor_executable.is_absolute()
            || policy
                .morf_executable
                .as_ref()
                .is_some_and(|path| !path.is_absolute())
        {
            return Err(protocol::invalid(
                "broker requires explicit trusted registration policy",
            ));
        }
        Ok(Self {
            endpoint,
            logind: Logind::open(library)?,
            policy,
            epoch: epoch()?,
            configuration: 1,
            session: 0,
            connections: Vec::new(),
            cursor: 0,
            observations: false,
            reload_requested: false,
            reload_pending: false,
            last_reload_error: None,
            inhibited: BTreeSet::new(),
            lock_proofs: BTreeMap::new(),
            completed_providers: BTreeSet::new(),
            routing: None,
            lua_source: None,
        })
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
        crate::routing::Routing::new(config.clone())?;
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
        self.lua_source = definition
            .map(|definition| {
                definition.validate().map_err(io::Error::other)?;
                let bytes = serde_json::to_vec(definition).map_err(io::Error::other)?;
                if bytes.len() > crate::lua::MAX_SNAPSHOT_BYTES {
                    return Err(protocol::invalid(
                        "approved Lua snapshot exceeds byte limit",
                    ));
                }
                Ok(bytes)
            })
            .transpose()?;
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
            } else if now.saturating_sub(bound.renewed) >= protocol::MAX_PACKET_AGE {
                Err(protocol::invalid("session agent lease expired"))
            } else {
                bound.registration.check(&self.logind, &bound.client)
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
                            && bound.capabilities.contains(&Capability::Commands))
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
                        !matches!(**capability, Capability::Commands | Capability::LuaActions)
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
                        Some("context snapshots exceed private transport packet limit".into());
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
                            && bound.capabilities.contains(&Capability::Commands))
                    && (bound.context.fresh(now)
                        || matches!(
                            &report,
                            Report::ContextLost { .. } | Report::SourceInvalidated { .. }
                        ))
            })
        }) else {
            return Ok(());
        };
        let mut connection = self.connections.remove(index);
        let bound = connection.bound.as_mut().unwrap();
        let verification = if now.saturating_sub(bound.renewed) >= protocol::MAX_PACKET_AGE {
            Err(protocol::invalid("session agent lease expired"))
        } else {
            bound
                .registration
                .check(&self.logind, &bound.client)
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
            Report::Observation { .. } | Report::Dispatch { .. }
        ) && !self.guard_observation(&mut connection, now)?
        {
            self.connections.insert(index, connection);
            return Ok(());
        }
        let bound = connection.bound.as_mut().unwrap();
        let fence = bound.fence;
        if matches!(report, Report::ContextLost { .. }) {
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
                if !bound.available_actions.contains(&Capability::Commands)
                    && !bound.available_actions.contains(&Capability::LuaActions)
                    || bound.actions.pending.len() >= crate::actions::MAX_RUNNING
                {
                    self.advance_session(&mut connection, now)?;
                    self.connections.insert(index, connection);
                    return Ok(());
                }
                let config = self
                    .routing
                    .as_ref()
                    .ok_or_else(|| protocol::invalid("observe broker cannot dispatch actions"))?;
                let crate::rules::Event::Matched {
                    rule,
                    source,
                    at,
                    action,
                    window,
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
                        configured.id == *rule && configured.enabled && configured.action == *action
                    })
                    .ok_or_else(|| {
                        protocol::invalid("worker action differs from root configuration")
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
                            matches!(capability, Capability::Commands | Capability::LuaActions)
                                && !bound.available_actions.contains(capability)
                        })
                    || !matches!(
                        action,
                        crate::config::Action::Command { .. } | crate::config::Action::Lua { .. }
                    )
                    || source.seat != bound.seat
                    || source.generation == 0
                    || *at > now
                    || now - *at > protocol::MAX_PACKET_AGE
                    || context.snapshot.state != State::Desktop
                    || context.snapshot.secure_overlay
                    || !configured
                        .contexts
                        .contains(&crate::config::Context::Desktop)
                    || configured.scope != crate::config::Scope::Everywhere && output.is_none()
                    || output.as_ref().is_some_and(|name| {
                        !context
                            .snapshot
                            .outputs
                            .iter()
                            .any(|output| output.geometry.name == *name)
                    })
                    || window.as_ref().is_some_and(|id| {
                        !context
                            .snapshot
                            .windows
                            .iter()
                            .any(|window| window.id == *id && window.visible)
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
        bound.registration.check(&self.logind, &bound.client)?;
        let Some(evidence) = bound.client.device_evidence(descriptor, monotonic()?)? else {
            return Ok(None);
        };
        bound.registration.check(&self.logind, &bound.client)?;
        Ok(Some(crate::configuration::Command::Transport {
            fence: bound.fence,
            source: descriptor.source.clone(),
            evidence,
        }))
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
                            .min(bound.renewed + protocol::MAX_PACKET_AGE)
                            .min(if bound.role == Role::Agent {
                                bound.context.checked + CONTEXT_REFRESH
                            } else {
                                Duration::MAX
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
        fds
    }
    /// One incoming message/registration per step prevents a subscriber from
    /// monopolizing supervision. All sends are nonblocking and bounded.
    pub fn step(&mut self, now: Duration) -> io::Result<()> {
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
            if now.saturating_sub(bound.renewed) >= protocol::MAX_PACKET_AGE {
                return Err(protocol::invalid("session agent lease expired"));
            }
            if now < bound.checked {
                return Err(protocol::invalid("session timeline moved backward"));
            }
            if now - bound.checked >= REFRESH {
                bound.registration.check(&self.logind, &bound.client)?;
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
                    instance,
                    role,
                    capabilities,
                } if connection.bound.is_none() => {
                    let commands = self.routing.as_ref().is_some_and(|config| {
                        config.rules.iter().any(|rule| {
                            rule.enabled
                                && matches!(
                                    rule.action,
                                    crate::config::Action::Command { .. }
                                        | crate::config::Action::Lua { .. }
                                )
                        })
                    });
                    if capabilities.iter().any(|capability| {
                        !(matches!(capability, Capability::Observations | Capability::Context)
                            || *capability == Capability::LuaEvents
                                && self.lua_source.is_some()
                                && capabilities.contains(&Capability::Observations)
                                && capabilities.contains(&Capability::Context)
                            || *capability == Capability::LuaActions
                                && self.routing.is_some()
                                && self.lua_source.is_some()
                                && capabilities.contains(&Capability::LuaEvents)
                                && capabilities.contains(&Capability::Commands)
                            || *capability == Capability::Commands
                                && commands
                                && capabilities.contains(&Capability::Context))
                    }) || !capabilities.is_empty() && (!self.observations || role != Role::Agent)
                    {
                        return Err(protocol::invalid(
                            "requested capabilities are not implemented by this broker",
                        ));
                    }
                    let peer = Peer::from_socket(&connection.channel)?;
                    self.verify_agent(peer.pid())?;
                    let client = Client::open(
                        &self.policy.runtime_root.join(peer.uid().to_string()),
                        &instance,
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
                    let mut context = ContextFeed::default();
                    if self.inhibited.contains(&compositor) {
                        context.inhibit();
                    }
                    connection.bound = Some(Bound {
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
                        native: None,
                        context_export: None,
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
                    // An in-flight status read may precede a transition notice.
                    // Discard it without acting on stale authority or closing
                    // the agent before that notice can reach it.
                    let old_status = matches!(&request, Request::Status { fence } | Request::LuaSource { fence, .. } | Request::ContextRead { fence, .. }
                        if now < bound.previous.expires && bound.previous.fences.contains(fence));
                    if !current && !old_heartbeat && !old_status {
                        return Err(protocol::invalid(
                            "stale broker/configuration/session generation",
                        ));
                    }
                    bound.registration.check(&self.logind, &bound.client)?;
                    self.verify_agent(connection.channel.peer().pid)?;
                    bound.checked = now;
                    if current {
                        bound.previous = Grace::default();
                    }
                    let fence = bound.fence;
                    match request {
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
                            let guarded = if offset == 0 {
                                bound.context.fresh(now)
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
                        Request::Authorize { id, ordinal, .. } => {
                            if !bound.capabilities.contains(&Capability::Commands)
                                || self.routing.is_none()
                            {
                                return Err(protocol::invalid(
                                    "command execution was not negotiated",
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
                            if !self.guard_observation(connection, monotonic()?)? {
                                return Ok(());
                            }
                            let bound = connection.bound.as_mut().unwrap();
                            let now = monotonic()?;
                            let context = bound.context.value.as_ref().ok_or_else(|| {
                                protocol::invalid("command context is unavailable")
                            })?;
                            bound
                                .actions
                                .authorize(id, ordinal, &context.snapshot, now)?;
                            let expires = (now + SNAPSHOT_MAX_AGE)
                                .min(context.snapshot.captured_at + SNAPSHOT_MAX_AGE);
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
                                !matches!(
                                    *capability,
                                    Capability::Commands | Capability::LuaActions
                                ) || !bound.capabilities.contains(capability)
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
                Some(Reply::Observation { .. } | Reply::Dispatch { .. } | Reply::Execute { .. })
            )
        {
            self.guard_observation(connection, monotonic()?)?;
        }
        connection.flush(monotonic()?)
    }
    fn guard_observation(
        &mut self,
        connection: &mut Connection,
        now: Duration,
    ) -> io::Result<bool> {
        let bound = connection.bound.as_mut().unwrap();
        if !bound.context.fresh(now) {
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
        let result = bound.context.sampler.check_lock(&bound.client);
        bound.registration.check(&self.logind, &bound.client)?;
        match result {
            Ok(false) if bound.context.fresh(monotonic()?) => return Ok(true),
            Ok(true) => {
                if self.inhibited.len() >= 4096 && !self.inhibited.contains(&bound.compositor) {
                    return Err(protocol::invalid("locked compositor history exhausted"));
                }
                self.inhibited.insert(bound.compositor);
                bound.context.inhibit();
            }
            Ok(false) | Err(_) => {
                bound.context.state = State::Unknown;
                bound.context.usable = false;
                bound.context.value = None;
                bound.context.error = Some("compositor lock state is unavailable".into());
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
        let bound = connection.bound.as_mut().unwrap();
        bound.previous.remember(bound.fence, now)?;
        bound.fence.session = self.session;
        if let Some(context) = bound.context.value.as_mut() {
            context.snapshot.generation = self.session;
        }
        bound.observing = false;
        bound.actions = ActionLedger::default();
        bound.context_export = None;
        let fence = bound.fence;
        let state = bound.context.state;
        connection.outgoing.clear();
        connection.bytes = 0;
        connection.push(Reply::SessionChanged { fence, state })
    }
    fn refresh_context(&mut self, connection: &mut Connection, now: Duration) -> io::Result<()> {
        let bound = connection.bound.as_mut().unwrap();
        if bound.role != Role::Agent || now < bound.context.checked + CONTEXT_REFRESH {
            return Ok(());
        }
        bound.registration.check(&self.logind, &bound.client)?;
        let binding = bound.registration.binding()?.clone();
        let was_fresh = bound.context.fresh(now);
        let old_usable = bound.context.usable;
        let old_state = bound.context.state;
        if let Some(executable) = &self.policy.morf_executable {
            if bound.native.is_none() {
                match crate::morf::Client::discover(
                    &self.policy.runtime_root.join(binding.uid.to_string()),
                    binding.uid,
                    executable,
                    &self.logind,
                    &bound.client,
                ) {
                    Ok(client) => bound.native = client,
                    Err(error) => {
                        bound.context.inhibit();
                        bound.context.error = Some(error.to_string());
                    }
                }
            }
            if let Some(native) = bound.native.as_mut() {
                match native.sample(&self.logind, &bound.client) {
                    Ok(state) => {
                        let proof = native.proof(&state);
                        match state.phase {
                            crate::morf::Phase::Pending => {
                                self.lock_proofs.insert(bound.compositor, proof);
                                bound.context.inhibit();
                            }
                            crate::morf::Phase::Locked => {
                                if bound.client.lock_state()? {
                                    self.lock_proofs.insert(bound.compositor, proof);
                                    bound.context.state = State::Locked;
                                }
                                bound.context.inhibit();
                            }
                            crate::morf::Phase::UnlockDispatched => {
                                if self.completed_providers.contains(&proof) {
                                    let _ = native.release(&state);
                                    bound.native = None;
                                } else if self.lock_proofs.get(&bound.compositor) == Some(&proof)
                                    && !bound.client.lock_state()?
                                {
                                    // The same live native producer observed its
                                    // lock acknowledgement and flushed unlock;
                                    // actual compositor unlock is independently
                                    // confirmed before retiring the provider.
                                    bound.registration.check(&self.logind, &bound.client)?;
                                    self.inhibited.remove(&bound.compositor);
                                    self.lock_proofs.remove(&bound.compositor);
                                    if self.completed_providers.len() >= 4096 {
                                        return Err(protocol::invalid(
                                            "native provider completion history exhausted",
                                        ));
                                    }
                                    self.completed_providers.insert(proof);
                                    bound.context.sampler.provider_unlock_completed();
                                    bound.context.state = State::Unknown;
                                    bound.context.usable = false;
                                    bound.context.value = None;
                                    let _ = native.release(&state);
                                    bound.native = None;
                                } else {
                                    bound.context.inhibit();
                                }
                            }
                        }
                    }
                    Err(error) => {
                        bound.native = None;
                        bound.context.state = State::Locking;
                        bound.context.inhibit();
                        bound.context.error = Some(error.to_string());
                    }
                }
            }
        }
        let result = if matches!(bound.context.state, State::Locking | State::Locked) {
            Ok(None)
        } else if bound.capabilities.contains(&Capability::Context) {
            bound
                .context
                .sampler
                .sample(&bound.client, bound.fence.session, binding.kind)
                .map(Some)
        } else {
            bound
                .context
                .sampler
                .check_lock(&bound.client)
                .map(|_| None)
        };
        bound.registration.check(&self.logind, &bound.client)?;
        let checked = monotonic()?;
        bound.context.checked = checked;
        if matches!(bound.context.state, State::Locking | State::Locked)
            || bound.context.sampler.inhibited()
        {
            // Persist inhibition across agent disconnect/re-registration for
            // the same compositor lifetime. No caller can clear this latch.
            if self.inhibited.len() >= 4096 && !self.inhibited.contains(&bound.compositor) {
                return Err(protocol::invalid("locked compositor history exhausted"));
            }
            self.inhibited.insert(bound.compositor);
            bound.context.inhibit();
        } else {
            match result {
                Ok(snapshot) => {
                    bound.context.state = snapshot
                        .as_ref()
                        .map_or(State::Unknown, |snapshot| snapshot.state);
                    bound.context.value =
                        snapshot.map(|snapshot| Box::new(ProvidedContext { binding, snapshot }));
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
        }
        if bound.context.value.as_ref().is_some_and(|value| {
            checked.saturating_sub(value.snapshot.captured_at) > SNAPSHOT_MAX_AGE
        }) {
            bound.context.value = None;
            bound.context.state = State::Unknown;
            bound.context.usable = false;
            bound.context.error = Some("compositor snapshot exceeded freshness budget".into());
        }
        if old_state != bound.context.state
            || old_usable != bound.context.usable
            || old_usable && !was_fresh
        {
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
            windows: vec![],
            regions: vec![],
            secure_overlay: false,
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
            windows: vec![],
            regions: vec![],
            secure_overlay: false,
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
    fn lock_inhibition_outlives_geometry_and_a_plain_lease_refresh() {
        let mut feed = ContextFeed::default();
        assert!(!feed.fresh(Duration::ZERO));
        feed.usable = true;
        feed.checked = Duration::from_millis(10);
        feed.state = State::Desktop;
        assert_eq!(feed.state(Duration::from_millis(20)), State::Desktop);
        assert_eq!(feed.state(Duration::from_millis(261)), State::Unknown);
        assert!(!feed.fresh(Duration::ZERO));
        feed.inhibit();
        feed.checked = Duration::from_secs(2);
        assert!(!feed.fresh(Duration::from_secs(2)));
        assert_eq!(feed.state(Duration::from_secs(3)), State::Locking);
        assert!(feed.value.is_none());
    }
    #[test]
    fn private_context_packet_limit_rejects_the_whole_snapshot() {
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
                windows: vec![],
                regions: vec![],
                secure_overlay: false,
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
                window: None,
                output: None,
                workspace: None,
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
