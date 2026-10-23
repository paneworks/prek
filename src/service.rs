//! Authenticated, bounded broker control plane. Device descriptors belong to a
//! separate supervised worker; this service performs no desktop actions and
//! writes compositor settings only for its own mirror devices.
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
mod morf_handoff;
mod recognizers;
mod rule_control;
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

pub struct Policy {
    pub runtime_root: PathBuf,
    pub compositor_executable: PathBuf,
    pub agent_executable: PathBuf,
    pub morf_executable: Option<PathBuf>,
    pub allowed_seats: BTreeSet<String>,
}
struct Bound {
    transport_started: bool,
    /// Mirror markers whose Hyprland device settings were applied, or refused.
    mirrors: BTreeMap<String, bool>,
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
    native_shells: Vec<crate::morf::ShellClient>,
    shell_states: Vec<crate::morf::ShellState>,
    shell_rendered: Vec<crate::morf::rendered::OutputContext>,
    shell_discovered: Duration,
    actions: ActionLedger,
    available_actions: BTreeSet<Capability>,
    context_export: Option<crate::context_transfer::Export>,
    rules: rule_control::Rules,
    recognizers: recognizers::Ledger,
}

fn refresh_native_shells(bound: &mut Bound, policy: &Policy, logind: &Logind, now: Duration) {
    let Some(executable) = policy.morf_executable.as_ref().filter(|_| {
        bound.context.fresh(now)
            && bound.context.state == State::Desktop
            && bound
                .context
                .value
                .as_ref()
                .is_some_and(|context| !context.snapshot.secure_overlay)
    }) else {
        bound.native_shells.clear();
        bound.shell_states.clear();
        bound.shell_rendered.clear();
        bound.shell_discovered = Duration::ZERO;
        return;
    };
    let until = std::time::Instant::now() + Duration::from_millis(200);
    let result = (|| {
        let states = if now.saturating_sub(bound.shell_discovered) >= Duration::from_secs(1) {
            bound.native_shells = crate::morf::ShellClient::discover_all(
                &policy
                    .runtime_root
                    .join(bound.registration.binding()?.uid.to_string()),
                bound.registration.binding()?.uid,
                executable,
                logind,
                &bound.client,
            )?;
            bound.shell_discovered = now;
            bound
                .native_shells
                .iter()
                .map(|client| {
                    client
                        .last_state()
                        .cloned()
                        .ok_or_else(|| protocol::invalid("native shell lacks sampled state"))
                })
                .collect::<io::Result<Vec<_>>>()?
        } else {
            bound
                .native_shells
                .iter_mut()
                .map(|client| client.sample_until(logind, &bound.client, until))
                .collect::<io::Result<Vec<_>>>()?
        };
        let mut outputs = BTreeSet::new();
        let snapshot = &bound.context.value.as_ref().unwrap().snapshot;
        let mut visible = Vec::new();
        for state in states {
            if !outputs.insert(state.output.clone()) {
                return Err(protocol::invalid("ambiguous native Morf output providers"));
            }
            if snapshot
                .outputs
                .iter()
                .any(|output| output.geometry.name == state.output)
            {
                visible.push(state);
            }
        }
        let mut rendered = Vec::new();
        let started = monotonic()?;
        let before = bound
            .context
            .sampler
            .sample_layers(&bound.client, &snapshot.outputs, until)
            .ok();
        let mut producers = Vec::new();
        for client in &mut bound.native_shells {
            let Some(state) = client.last_state().filter(|state| visible.contains(state)) else {
                continue;
            };
            let output = state.output.clone();
            if let Some(context) = client.rendered_context_until(logind, &bound.client, until)? {
                producers.push(client.producer());
                rendered.push(crate::morf::rendered::OutputContext {
                    output,
                    context,
                    layout: None,
                });
            }
        }
        if let Some(before) = before
            && let Ok(after) =
                bound
                    .context
                    .sampler
                    .sample_layers(&bound.client, &snapshot.outputs, until)
        {
            let sampled = monotonic()?;
            for (view, producer) in rendered.iter_mut().zip(producers) {
                let output = snapshot
                    .outputs
                    .iter()
                    .find(|output| output.geometry.name == view.output)
                    .unwrap();
                view.layout = crate::morf::rendered::LayoutProjection::from_samples(
                    &view.context,
                    output,
                    &before,
                    &after,
                    producer,
                    (started, sampled),
                )?;
            }
        }
        Ok((visible, rendered))
    })();
    match result {
        Ok((states, rendered)) => {
            bound.shell_states = states;
            bound.shell_rendered = rendered;
        }
        Err(_) => {
            // Optional shell identity loss never fabricates a region, action
            // grant or unlock, and does not inhibit unrelated generic rules.
            bound.native_shells.clear();
            bound.shell_states.clear();
            bound.shell_rendered.clear();
        }
    }
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
    let commands = routing.is_some_and(|config| {
        config.rules.iter().any(|rule| {
            rule.enabled
                && matches!(
                    rule.action,
                    crate::config::Action::Command { .. } | crate::config::Action::Lua { .. }
                )
        })
    });
    let power = routing.is_some_and(|config| {
        config.mode == crate::config::Mode::Route
            && config.policy.allow_power_actions
            && config.rules.iter().any(|rule| {
                rule.enabled
                    && rule.action.requires_power()
                    && protocol::supported_session_action(&rule.action)
            })
    });
    let hyprland = routing.is_some_and(|config| {
        config.mode == crate::config::Mode::Route
            && config.policy.allow_hyprland_actions
            && config.rules.iter().any(|rule| {
                rule.enabled
                    && rule.action.requires_hyprland()
                    && protocol::supported_session_action(&rule.action)
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
                && (capabilities.contains(&Capability::Commands)
                    || capabilities.contains(&Capability::PowerActions)
                    || capabilities.contains(&Capability::HyprlandActions))
            || *capability == Capability::HyprlandActions
                && hyprland
                && capabilities.contains(&Capability::Context)
            || *capability == Capability::PowerActions
                && power
                && capabilities.contains(&Capability::Context)
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
    window: Option<String>,
    native: Option<crate::routing::native::Target>,
    deadline: Duration,
    granted: bool,
    grant_until: Option<Duration>,
    morf: Option<(crate::config::Action, crate::morf::controls::Request)>,
    morf_submitted: bool,
    morf_result_sent: bool,
    sent: bool,
    delivered: bool,
    cancelled: bool,
    required: BTreeSet<Capability>,
    reorder_scope: Option<protocol::ReorderScope>,
    reorder_required: bool,
}
impl Offered {
    fn morf_request(
        &self,
        selected: crate::config::MorfAction,
    ) -> io::Result<crate::morf::controls::Request> {
        let (action, template) = self
            .morf
            .as_ref()
            .ok_or_else(|| protocol::invalid("action has no frozen Morf payload"))?;
        if !match action {
            crate::config::Action::Morf { action } => *action == selected,
            crate::config::Action::Lua { requires, .. } => {
                requires.contains(&crate::config::EffectKind::MorfActions)
            }
            _ => false,
        } {
            return Err(protocol::invalid(
                "Morf selection exceeds its configured offer",
            ));
        }
        let mut request = template.clone();
        request.action = selected;
        if request.phase == crate::morf::controls::Phase::Begin
            && !matches!(
                selected,
                crate::config::MorfAction::Dashboard
                    | crate::config::MorfAction::Notifications
                    | crate::config::MorfAction::QuickSettings
                    | crate::config::MorfAction::WorkspacePreview
            )
        {
            request.phase = crate::morf::controls::Phase::Discrete;
        }
        request.validate()?;
        Ok(request)
    }
    fn target_valid(&self, snapshot: &crate::context::Snapshot, now: Duration) -> bool {
        let output = |snapshot: &crate::context::Snapshot| {
            snapshot
                .outputs
                .iter()
                .find(|output| Some(&output.geometry.name) == self.output.as_ref())
                .cloned()
        };
        let window = |snapshot: &crate::context::Snapshot| {
            snapshot
                .windows
                .iter()
                .find(|window| Some(&window.id) == self.window.as_ref())
                .cloned()
        };
        !self.cancelled
            && now < self.deadline
            && snapshot.captured_at <= now
            && now - snapshot.captured_at < SNAPSHOT_MAX_AGE
            && snapshot.state == State::Desktop
            && !snapshot.secure_overlay
            && snapshot.generation == self.snapshot.generation
            && (self.output.is_none()
                || output(&self.snapshot).is_some_and(|old| Some(old) == output(snapshot)))
            && (self.window.is_none()
                || window(&self.snapshot).is_some_and(|old| Some(old) == window(snapshot)))
            && (self.output.is_none() || snapshot.regions == self.snapshot.regions)
    }
    fn morf_control(
        &self,
        request: &crate::morf::controls::Request,
        snapshot: &crate::context::Snapshot,
        now: Duration,
    ) -> io::Result<Duration> {
        let until = self
            .grant_until
            .filter(|until| now < *until)
            .filter(|_| {
                self.granted && self.sent && self.delivered && self.target_valid(snapshot, now)
            })
            .ok_or_else(|| {
                protocol::invalid("native control has no live delivered execution grant")
            })?;
        if self.morf_request(request.action)? != *request {
            return Err(protocol::invalid(
                "native payload differs from frozen event",
            ));
        }
        Ok(until)
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
    /// A scroll update is dropped while two offers of its gesture are still
    /// pending; later progress carries the same cumulative travel.
    fn backlogged_scroll(&self, event: &crate::rules::Event) -> bool {
        matches!(event, crate::rules::Event::Matched {
            id,
            stage: crate::rules::Stage::Update,
            action: crate::config::Action::Hyprland {
                action: crate::config::WindowAction::ScrollStack
            },
            ..
        } if self.pending.range((*id, 0)..=(*id, u64::MAX)).count() >= 2)
    }
    #[cfg(test)]
    fn offer(
        &mut self,
        fence: Fence,
        event: &crate::rules::Event,
        snapshot: &crate::context::Snapshot,
        now: Duration,
    ) -> io::Result<()> {
        self.offer_scoped(fence, event, snapshot, None, now)
    }
    fn offer_scoped(
        &mut self,
        fence: Fence,
        event: &crate::rules::Event,
        snapshot: &crate::context::Snapshot,
        reorder_scope: Option<protocol::ReorderScope>,
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
                reorder_scope,
                reorder_required:matches!(event,crate::rules::Event::Matched{action,..} if crate::routing::native::required(action)),
                source: source.clone(),
                snapshot: snapshot.clone(),
                output: output.clone(),
                window: window.clone(),
                native: crate::routing::native::event_target(event),
                deadline: now + protocol::MAX_PACKET_AGE,
                granted: false,
                grant_until: None,
                morf_submitted: false,
                morf_result_sent: false,
                morf: match event {
                    crate::rules::Event::Matched { action, .. }
                        if protocol::action_capabilities(action)
                            .contains(&Capability::MorfActions) =>
                    {
                        let selected = match action {
                            crate::config::Action::Morf { action } => *action,
                            _ => crate::config::MorfAction::Dashboard,
                        };
                        crate::morf::controls::Request::from_event(fence, event, selected)
                            .ok()
                            .map(|request| (action.clone(), request))
                    }
                    _ => None,
                },
                sent: false,
                delivered: false,
                cancelled: false,
                required: match event {
                    crate::rules::Event::Matched { action, .. } => {
                        protocol::action_capabilities(action)
                    }
                    _ => unreachable!(),
                },
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
        if offer.granted
            || !offer.target_valid(snapshot, now)
            || offer.reorder_required && offer.reorder_scope.is_none()
        {
            return Err(protocol::invalid(
                "action target or execution authority expired",
            ));
        }
        offer.granted = true;
        offer.grant_until =
            Some((now + SNAPSHOT_MAX_AGE).min(snapshot.captured_at + SNAPSHOT_MAX_AGE));
        Ok(())
    }
    fn retire_native(
        &mut self,
        context: Option<&ProvidedContext>,
    ) -> Vec<(crate::input::Source, u64)> {
        let mut cancelled = Vec::new();
        for ((id, _), offer) in &mut self.pending {
            if let Some(scope) = &offer.reorder_scope {
                let current = context.and_then(|context| {
                    offer
                        .native
                        .as_ref()
                        .and_then(|target| context.native_scope(target))
                });
                if current.as_ref() != Some(scope) {
                    offer.reorder_scope = None;
                }
            }
            if offer.reorder_required && offer.reorder_scope.is_none() && !offer.cancelled {
                cancelled.push((offer.source.clone(), *id));
            }
        }
        cancelled
    }
    fn authorize_native(
        &mut self,
        id: u64,
        ordinal: u64,
        context: &ProvidedContext,
        now: Duration,
    ) -> io::Result<Option<protocol::ReorderScope>> {
        let _ = self.retire_native(Some(context));
        self.authorize(id, ordinal, &context.snapshot, now)?;
        Ok(self.pending[&(id, ordinal)].reorder_scope.clone())
    }
    fn complete(&mut self, id: u64, ordinal: u64) -> io::Result<()> {
        if self.pending.get(&(id, ordinal)).is_none_or(|offer| {
            !offer.granted || !offer.sent || offer.morf_submitted && !offer.morf_result_sent
        }) {
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
        self.refresh_native_actions(now)?;
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
            if let Reply::MorfResult { id, ordinal, .. } = &reply
                && let Some(bound) = self.bound.as_mut()
                && let Some(offer) = bound.actions.pending.get_mut(&(*id, *ordinal))
            {
                offer.morf_result_sent = true;
            }
            self.bytes -= reply_size(&reply)?;
        }
        Ok(())
    }
    fn refresh_native_actions(&mut self, now: Duration) -> io::Result<()> {
        let Some(bound) = self.bound.as_mut() else {
            return Ok(());
        };
        let context = bound
            .context
            .fresh(now)
            .then_some(bound.context.value.as_deref())
            .flatten();
        let cancelled = bound.actions.retire_native(context);
        for reply in &mut self.outgoing {
            let identity = match reply {
                Reply::Dispatch { event, .. } => match event.as_ref() {
                    crate::rules::Event::Matched { id, ordinal, .. } => Some((*id, *ordinal)),
                    _ => None,
                },
                Reply::Execute { id, ordinal, .. } => Some((*id, *ordinal)),
                _ => None,
            };
            if identity.is_some_and(|identity| {
                bound
                    .actions
                    .pending
                    .get(&identity)
                    .is_none_or(|offer| offer.reorder_scope.is_none())
            }) {
                match reply {
                    Reply::Dispatch { reorder_scope, .. }
                    | Reply::Execute { reorder_scope, .. } => *reorder_scope = None,
                    _ => {}
                }
            }
        }
        self.bytes = self
            .outgoing
            .iter()
            .map(reply_size)
            .sum::<io::Result<usize>>()?;
        for (source, id) in cancelled {
            self.cancel_actions(&source, Some(id))?;
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
    input_releases: Option<crate::morf::ownership::Ledger>,
    morf_handoffs: Vec<crate::morf::handoff::Session>,
    morf_pending: Vec<morf_handoff::Pending>,
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
    rule_baseline: Option<std::sync::Arc<crate::config::Config>>,
    rule_handlers: Vec<crate::lua::HandlerRegistration>,
    recognizer_bindings: Vec<crate::lua::RecognizerBinding>,
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
            input_releases: None,
            morf_handoffs: Vec::new(),
            morf_pending: Vec::new(),
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
            inhibited: BTreeSet::new(),
            lock_proofs: BTreeMap::new(),
            completed_providers: BTreeSet::new(),
            routing: None,
            lua_source: None,
            rule_baseline: None,
            rule_handlers: Vec::new(),
            recognizer_bindings: Vec::new(),
        })
    }
    pub fn bind_input_worker(
        &mut self,
        worker: &crate::supervisor::Supervisor,
        channel: &Channel,
    ) -> io::Result<()> {
        if self.input_releases.is_some() {
            return Err(protocol::invalid("input worker already bound"));
        }
        self.input_releases = Some(crate::morf::ownership::Ledger::new(
            worker.worker_identity(),
            channel.peer(),
        )?);
        Ok(())
    }
    /// The handoff owner enrolls before any consumer readiness can be issued.
    /// No public client request can create or complete this descriptor evidence.
    pub fn watch_input_ownership(&mut self, fence: Fence, seat: &str) -> io::Result<()> {
        if !self
            .connections
            .iter()
            .filter_map(|c| c.bound.as_ref())
            .any(|b| {
                b.fence == fence && b.seat == seat && !b.transport_started && b.role == Role::Agent
            })
        {
            return Err(protocol::invalid(
                "ownership must enroll before transport readiness",
            ));
        }
        self.input_releases
            .as_mut()
            .ok_or_else(|| protocol::invalid("input worker is not bound"))?
            .watch(fence, seat)
    }
    pub fn input_released(
        &mut self,
        channel: &Channel,
        worker: crate::supervisor::WorkerIdentity,
        fence: Fence,
        source: &crate::input::Source,
    ) -> io::Result<()> {
        self.input_releases
            .as_mut()
            .ok_or_else(|| protocol::invalid("input worker is not bound"))?
            .released(channel, worker, fence, source)
    }
    pub fn input_worker_exited(&mut self, report: &crate::supervisor::Report) -> io::Result<()> {
        self.input_releases
            .as_mut()
            .ok_or_else(|| protocol::invalid("input worker is not bound"))?
            .exited(report)?;
        self.release_morf_after_worker_exit()
    }
    pub fn take_released_inputs(
        &mut self,
        fence: Fence,
        seat: &str,
    ) -> Option<crate::morf::ownership::ReleasedInputs> {
        self.input_releases.as_mut()?.take(fence, seat)
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
                            && bound
                                .capabilities
                                .iter()
                                .copied()
                                .any(protocol::execution_capability))
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
                        if **capability == Capability::HyprlandActions
                            && !self
                                .routing
                                .as_ref()
                                .is_some_and(|config| config.policy.allow_hyprland_actions)
                        {
                            return false;
                        }
                        if **capability == Capability::PowerActions
                            && !self
                                .routing
                                .as_ref()
                                .is_some_and(|config| config.policy.allow_power_actions)
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
        if let Some(ledger) = self.input_releases.as_mut() {
            ledger.retire_except(
                &subscriptions
                    .iter()
                    .map(|s| (s.fence, s.seat.clone()))
                    .collect::<Vec<_>>(),
            );
        }
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
                self.cancel_morf_source(connection.bound.as_ref().unwrap().fence, source, None);
                self.connections.insert(index, connection);
                return Ok(());
            }
            Report::Observation { event, .. } => {
                if let crate::rules::Event::Cancelled { source, id, .. } = event.as_ref() {
                    connection.cancel_actions(source, Some(*id))?;
                    self.cancel_morf_source(
                        connection.bound.as_ref().unwrap().fence,
                        source,
                        Some(*id),
                    );
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
            Report::Dispatch {
                event,
                reorder_scope,
                ..
            } if bound.observing => {
                if bound.rules.busy() || bound.actions.backlogged_scroll(&event) {
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
                    workspace,
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
                            protocol::execution_capability(*capability)
                                && !bound.available_actions.contains(capability)
                        })
                    || !protocol::supported_session_action(action)
                    || action.requires_hyprland()
                        && (!config.policy.allow_hyprland_actions
                            || !context.hyprland_target_available(
                                action,
                                configured.scope,
                                window.as_deref(),
                                output.as_deref(),
                                *workspace,
                            ))
                    || action.requires_power()
                        && (!config.policy.allow_power_actions
                            || output
                                .as_deref()
                                .is_none_or(|name| !crate::config::valid_power_output(name)))
                    || configured
                        .output
                        .as_ref()
                        .is_some_and(|name| Some(name) != output.as_ref())
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
                        !context.snapshot.windows.iter().any(|window| {
                            window.id == *id
                                && window.mapped
                                && (window.visible
                                    || configured.scope == crate::config::Scope::Everywhere)
                        })
                    })
                {
                    return Err(protocol::invalid(
                        "action is unavailable for this session/context",
                    ));
                }
                let reorder_scope = reorder_scope.filter(|scope| {
                    crate::routing::native::event_target(&event)
                        .and_then(|target| context.native_scope(&target))
                        .as_ref()
                        == Some(scope)
                });
                if crate::routing::native::required(action) && reorder_scope.is_none() {
                    self.connections.insert(index, connection);
                    return Ok(());
                }
                Reply::Dispatch {
                    fence,
                    event: event.clone(),
                    reorder_scope: reorder_scope.clone(),
                }
                .validate()?;
                bound.actions.offer_scoped(
                    bound.fence,
                    &event,
                    &context.snapshot,
                    reorder_scope.clone(),
                    now,
                )?;
                Reply::Dispatch {
                    fence,
                    event,
                    reorder_scope,
                }
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
        let generated = descriptor.is_generated_keyboard();
        let owners = if generated {
            Vec::new()
        } else {
            self.morf_requirements(subscription.fence, &subscription.seat)
        };
        let lease = if generated {
            None
        } else {
            self.morf_lease(subscription.fence, &subscription.seat, now)
        };
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
        if !generated {
            if bound.mirrors.len() >= 256 && !bound.mirrors.contains_key(&descriptor.marker) {
                return Err(protocol::invalid("too many prepared mirror devices"));
            }
            let configured = match bound.mirrors.get(&descriptor.marker) {
                Some(configured) => *configured,
                None => {
                    let result = bound.client.configure_mirror(descriptor);
                    if let Err(error) = &result {
                        eprintln!(
                            "mirror {} keeps no consumer evidence: {error}",
                            descriptor.name
                        );
                    }
                    bound
                        .mirrors
                        .insert(descriptor.marker.clone(), result.is_ok());
                    result.is_ok()
                }
            };
            if !configured {
                return Ok(None);
            }
        }
        let evidence = match bound.client.device_evidence(descriptor, monotonic()?) {
            Ok(evidence) => evidence,
            Err(error) if !generated => {
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
        bound.registration.check(&self.logind, &bound.client)?;
        let admission = (!owners.is_empty()).then_some(crate::morf::admission::Admission {
            owners,
            expires: lease.map(|until| {
                until
                    .min(bound.context.checked + crate::context::SNAPSHOT_MAX_AGE)
                    .min(
                        bound.context.value.as_ref().unwrap().snapshot.captured_at
                            + crate::context::SNAPSHOT_MAX_AGE,
                    )
            }),
        });
        let reply = transport_offer::reply(
            descriptor,
            bound.fence,
            evidence,
            admission,
            self.input_releases.as_mut(),
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
                            .min(bound.renewed + protocol::MAX_PACKET_AGE)
                            .min(if bound.role == Role::Agent {
                                bound.context.checked + CONTEXT_REFRESH
                            } else {
                                Duration::MAX
                            })
                    },
                )
            })
            .chain(
                self.morf_handoffs
                    .iter()
                    .filter_map(|handoff| handoff.deadline()),
            )
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
            self.poll_morf_handoffs(monotonic()?);
            self.collect_morf_controls()?;
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
        self.poll_morf_handoffs(monotonic()?);
        self.collect_morf_controls()?;
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
                    let client = Client::open(
                        &self.policy.runtime_root.join(peer.uid().to_string()),
                        &instance,
                        peer.uid(),
                        &self.policy.compositor_executable,
                    )?;
                    if capabilities.contains(&Capability::HyprlandActions) {
                        client.verify_window_actions()?;
                    }
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
                        transport_started: false,
                        mirrors: BTreeMap::new(),
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
                        native_shells: Vec::new(),
                        shell_states: Vec::new(),
                        shell_rendered: Vec::new(),
                        shell_discovered: Duration::ZERO,
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
                    // An in-flight status read may precede a transition notice.
                    // Discard it without acting on stale authority or closing
                    // the agent before that notice can reach it.
                    let old_status = matches!(&request, Request::Status { fence } | Request::LuaSource { fence, .. } | Request::ContextRead { fence, .. } | Request::Rules {fence,..} | Request::MorfAction {fence,..} | Request::MorfCancel {fence,..} | Request::RecognizerReady {fence,..} | Request::Recognize {fence,..} | Request::RecognitionCancelled {fence,..}
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
                        Request::MorfAction { .. } | Request::MorfCancel { .. } if !current => {}
                        Request::MorfAction {
                            id,
                            ordinal,
                            action,
                            ..
                        } => self.submit_agent_morf(connection, id, ordinal, action)?,
                        Request::MorfCancel { id, ordinal, .. } => {
                            self.cancel_agent_morf(connection, id, ordinal)?
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
                                    morf_rendered: if bound.context.fresh(now)
                                        && bound.context.state(now) == State::Desktop
                                    {
                                        bound
                                            .shell_rendered
                                            .iter()
                                            .filter(|rendered| {
                                                rendered.context.frame.is_none()
                                                    || rendered.context.fresh(now)
                                            })
                                            .cloned()
                                            .collect()
                                    } else {
                                        Vec::new()
                                    },
                                    morf: if bound.context.fresh(now)
                                        && bound.context.state(now) == State::Desktop
                                    {
                                        bound.shell_states.clone()
                                    } else {
                                        Vec::new()
                                    },
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
                            if !self.guard_observation(connection, monotonic()?)? {
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
                            let reorder_scope =
                                bound.actions.authorize_native(id, ordinal, context, now)?;
                            let expires = (now + SNAPSHOT_MAX_AGE)
                                .min(context.snapshot.captured_at + SNAPSHOT_MAX_AGE);
                            connection.push(Reply::Execute {
                                fence,
                                id,
                                ordinal,
                                expires,
                                reorder_scope,
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
                            if bound
                                .available_actions
                                .contains(&Capability::HyprlandActions)
                                && !capabilities.contains(&Capability::HyprlandActions)
                            {
                                bound.context.sampler.retire_native();
                                bound.context.checked = Duration::ZERO;
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
        bound.rules = rule_control::Rules::new(self.rule_baseline.clone());
        bound.recognizers = Default::default();
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
                .sample_with_layouts(&bound.client, bound.fence.session, binding.kind)
                .map(|(snapshot, layouts)| {
                    Some(ProvidedContext {
                        binding: binding.clone(),
                        snapshot,
                        layouts,
                    })
                })
        } else {
            bound
                .context
                .sampler
                .check_lock(&bound.client)
                .map(|_| None)
        };
        bound.registration.check(&self.logind, &bound.client)?;
        let mut checked = monotonic()?;
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
        }
        refresh_native_shells(bound, &self.policy, &self.logind, checked);
        if let Some(value) = bound.context.value.as_mut() {
            crate::morf::rendered::scopes::apply(
                &mut value.snapshot,
                &bound.shell_rendered,
                monotonic()?,
            );
        }
        if bound.context.sampler.inhibited() {
            if self.inhibited.len() >= 4096 && !self.inhibited.contains(&bound.compositor) {
                return Err(protocol::invalid("locked compositor history exhausted"));
            }
            self.inhibited.insert(bound.compositor);
            bound.context.inhibit();
        }
        checked = monotonic()?;
        bound.context.checked = checked;
        if bound.context.value.as_ref().is_some_and(|value| {
            checked.saturating_sub(value.snapshot.captured_at) > SNAPSHOT_MAX_AGE
        }) {
            bound.context.value = None;
            bound.context.state = State::Unknown;
            bound.context.usable = false;
            bound.context.error = Some("compositor snapshot exceeded freshness budget".into());
        }
        let transition = old_state != bound.context.state
            || old_usable != bound.context.usable
            || old_usable && !was_fresh;
        connection.refresh_native_actions(checked)?;
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
    mod native_reorder;
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
        actions
            .offer(fence, &event, &snapshot, Duration::ZERO)
            .unwrap();
        assert!(
            actions
                .offer(fence, &event, &snapshot, Duration::ZERO)
                .is_err()
        );
        assert!(actions.complete(1, 1).is_err());
        actions.authorize(1, 1, &snapshot, Duration::ZERO).unwrap();
        assert!(actions.authorize(1, 1, &snapshot, Duration::ZERO).is_err());
        assert!(
            actions.complete(1, 1).is_err(),
            "unsent grant accepted a receipt"
        );
        actions.pending.get_mut(&(1, 1)).unwrap().sent = true;
        actions.pending.get_mut(&(1, 1)).unwrap().morf_submitted = true;
        assert!(
            actions.complete(1, 1).is_err(),
            "agent acknowledged a native action before its outcome was delivered"
        );
        actions.pending.get_mut(&(1, 1)).unwrap().morf_result_sent = true;
        actions.complete(1, 1).unwrap();
        assert!(actions.complete(1, 1).is_err());

        for (at, state, generation) in [
            (250, State::Desktop, 1),
            (1000, State::Desktop, 1),
            (1, State::Locked, 1),
            (1, State::Desktop, 2),
        ] {
            let mut actions = ActionLedger::default();
            actions
                .offer(fence, &event, &snapshot, Duration::ZERO)
                .unwrap();
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
    fn native_controls_require_delivered_grants_and_exact_frozen_event_and_target() {
        // Explicit context/registration model; this does not establish live
        // root, logind, native controller execution or physical ownership.
        let at = Duration::from_secs(1);
        let view = crate::context_transfer::tests::view(at);
        let snapshot = view.context.unwrap().snapshot;
        let fence = view.fence;
        let Reply::Observation { mut event, .. } = progress(fence, 1, Stage::Begin) else {
            unreachable!()
        };
        let Event::Matched {
            action,
            output,
            evidence,
            at: event_at,
            ..
        } = event.as_mut()
        else {
            unreachable!()
        };
        *action = Action::Morf {
            action: crate::config::MorfAction::Dashboard,
        };
        *output = Some(snapshot.outputs[0].geometry.name.clone());
        *event_at = at;
        let Evidence::Touch { space, .. } = evidence else {
            unreachable!()
        };
        *space = Space::Logical;
        let request = crate::morf::controls::Request::from_event(
            fence,
            &event,
            crate::config::MorfAction::Dashboard,
        )
        .unwrap();
        let mut actions = ActionLedger::default();
        actions.offer(fence, &event, &snapshot, at).unwrap();
        assert!(
            actions.pending[&(1, 1)]
                .morf_control(&request, &snapshot, at)
                .is_err()
        );
        actions.authorize(1, 1, &snapshot, at).unwrap();
        let offer = actions.pending.get_mut(&(1, 1)).unwrap();
        assert!(offer.morf_control(&request, &snapshot, at).is_err());
        offer.sent = true;
        assert!(offer.morf_control(&request, &snapshot, at).is_err());
        offer.delivered = true;
        let until = at + SNAPSHOT_MAX_AGE;
        assert_eq!(offer.morf_control(&request, &snapshot, at).unwrap(), until);
        for field in ["dx", "output", "epoch", "ordinal", "action"] {
            let mut bad = request.clone();
            match field {
                "dx" => bad.dx += 1.,
                "output" => bad.output = "other".into(),
                "epoch" => bad.epoch = "f".repeat(32),
                "ordinal" => bad.ordinal += 1,
                _ => bad.action = crate::config::MorfAction::Notifications,
            }
            assert!(offer.morf_control(&bad, &snapshot, at).is_err(), "{field}");
        }
        let mut fresh = snapshot.clone();
        fresh.captured_at += Duration::from_millis(100);
        assert_eq!(
            offer
                .morf_control(&request, &fresh, fresh.captured_at)
                .unwrap(),
            until
        );
        fresh.captured_at = until;
        assert!(
            offer.morf_control(&request, &fresh, until).is_err(),
            "context refresh extended the original grant"
        );
        for changed in [
            crate::context::Snapshot {
                secure_overlay: true,
                ..snapshot.clone()
            },
            crate::context::Snapshot {
                outputs: vec![],
                ..snapshot.clone()
            },
            crate::context::Snapshot {
                regions: vec![],
                ..snapshot.clone()
            },
        ] {
            assert!(offer.morf_control(&request, &changed, at).is_err());
        }
        offer.cancelled = true;
        assert!(offer.morf_control(&request, &snapshot, at).is_err());

        let Event::Matched { action, .. } = event.as_mut() else {
            unreachable!()
        };
        *action = Action::Lua {
            handler: "shell".into(),
            requires: BTreeSet::from([crate::config::EffectKind::MorfActions]),
        };
        let mut actions = ActionLedger::default();
        actions.offer(fence, &event, &snapshot, at).unwrap();
        actions.authorize(1, 1, &snapshot, at).unwrap();
        let offer = actions.pending.get_mut(&(1, 1)).unwrap();
        offer.sent = true;
        offer.delivered = true;
        let mut selected = request.clone();
        selected.action = crate::config::MorfAction::Notifications;
        assert_eq!(offer.morf_control(&selected, &snapshot, at).unwrap(), until);
        selected.dy += 1.;
        assert!(offer.morf_control(&selected, &snapshot, at).is_err());
    }
    #[test]
    fn native_progress_needs_logical_evidence_and_current_event_generations() {
        let fence = Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 1,
        };
        let Reply::Observation { mut event, .. } = progress(fence, 1, Stage::Begin) else {
            unreachable!()
        };
        let Event::Matched { output, .. } = event.as_mut() else {
            unreachable!()
        };
        *output = Some("TEST-1".into());
        let build = |event: &Event| {
            crate::morf::controls::Request::from_event(
                fence,
                event,
                crate::config::MorfAction::Dashboard,
            )
        };
        assert!(
            build(&event).is_err(),
            "normalized values passed as logical displacement"
        );
        let Event::Matched {
            evidence: Evidence::Touch { space, .. },
            ..
        } = event.as_mut()
        else {
            unreachable!()
        };
        *space = Space::Logical;
        assert!(build(&event).is_ok());
        let Event::Matched {
            config_generation, ..
        } = event.as_mut()
        else {
            unreachable!()
        };
        *config_generation += 1;
        assert!(build(&event).is_err());
    }
    #[test]
    fn an_absent_frozen_target_cannot_match_an_absent_current_target() {
        let at = Duration::from_secs(1);
        let view = crate::context_transfer::tests::view(at);
        let fence = view.fence;
        let snapshot = view.context.unwrap().snapshot;
        for missing_window in [false, true] {
            let Reply::Observation { mut event, .. } = progress(fence, 1, Stage::Begin) else {
                unreachable!()
            };
            let Event::Matched { output, window, .. } = event.as_mut() else {
                unreachable!()
            };
            if missing_window {
                *window = Some("missing".into());
            } else {
                *output = Some("missing".into());
            }
            let mut actions = ActionLedger::default();
            actions.offer(fence, &event, &snapshot, at).unwrap();
            assert!(actions.authorize(1, 1, &snapshot, at).is_err());
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
        actions
            .offer(fence, &event, &snapshot, Duration::ZERO)
            .unwrap();
        actions
            .offer(fence, &other, &snapshot, Duration::ZERO)
            .unwrap();
        actions.pending.get_mut(&(1, 1)).unwrap().delivered = true;
        assert_eq!(actions.cancel(&source, Some(1)), vec![((1, 1), true)]);
        assert!(actions.authorize(1, 1, &snapshot, Duration::ZERO).is_err());
        assert!(
            actions.complete(1, 1).is_err(),
            "ungranted cancellation must not become Applied"
        );
        actions.complete_unexecuted(1, 1).unwrap();
        assert!(actions.complete_unexecuted(1, 1).is_err());
        assert!(
            actions
                .offer(fence, &event, &snapshot, Duration::ZERO)
                .is_err()
        );
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
            layouts: None,
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

    #[test]
    fn native_layout_private_transfer_preserves_the_complete_graph() {
        let view = crate::context_transfer::tests::native_view(Duration::ZERO, 512);
        let subscription = Subscription {
            seat: "seat0".into(),
            fence: view.fence,
            context: view.context.clone(),
            capabilities: BTreeSet::from([Capability::Context]),
            consumer: None,
        };
        assert!(serde_json::to_vec(&subscription).unwrap().len() > protocol::MAX_PACKET);
        assert!(subscriptions_fit(std::slice::from_ref(&subscription)));
        assert_eq!(subscription.context, view.context);
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
