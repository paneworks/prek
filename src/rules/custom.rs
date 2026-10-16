//! Native eligibility and lifecycle for asynchronous recognizer proposals.
//! No Lua executes on this coordinator's input thread.
use super::*;
use crate::{
    input_events::Event as Input,
    protocol::Fence,
    recognition::{self, Gate, InputKind, Proposal, Retirement, Ticket},
    router::Ownership,
};
use std::collections::VecDeque;

pub(super) struct State {
    pub fence: Fence,
    pub gate: Gate,
    pub approved: Vec<recognition::Definition>,
    ready: bool,
    ready_at: Duration,
    live: BTreeSet<u64>,
    offers: VecDeque<Offer>,
    bytes: usize,
}
struct Offer {
    candidate: u64,
    input: Option<Input>,
    at: Duration,
    bytes: usize,
}
pub(super) fn definitions(config: &Config) -> Vec<recognition::Definition> {
    config.recognizer_definitions()
}
impl State {
    pub(super) fn ready(&self) -> bool {
        self.ready
    }
    pub fn new(config: &Config, fence: Option<Fence>) -> io::Result<Option<Self>> {
        let definitions = definitions(config);
        let Some(fence) = fence else {
            if !config.rules.iter().any(|rule| {
                rule.enabled
                    && config.mode_active(rule)
                    && rule.trigger.recognizers().next().is_some()
            }) {
                return Ok(None);
            }
            return Err(invalid(
                "custom recognizers require a trusted asynchronous ticket coordinator",
            ));
        };
        Ok(Some(Self {
            fence,
            gate: Gate::new(fence, definitions.clone())?,
            approved: definitions,
            ready: true,
            ready_at: Duration::ZERO,
            live: BTreeSet::new(),
            offers: VecDeque::new(),
            bytes: 0,
        }))
    }
    fn offer(&mut self, candidate: u64, input: Option<Input>, at: Duration) -> io::Result<()> {
        let bytes = input.as_ref().map_or(Ok(128), |input| {
            recognition::encoded_size(input, recognition::MAX_FRAME_BYTES - 1024)
                .map(|size| size + 128)
        })?;
        if self.offers.len() >= recognition::MAX_PENDING
            || self.bytes + bytes > recognition::MAX_BYTES
        {
            return Err(invalid("custom recognizer offer queue full"));
        }
        self.offers.push_back(Offer {
            candidate,
            input,
            at,
            bytes,
        });
        self.bytes += bytes;
        Ok(())
    }
}
pub(super) struct FrameContext<'a> {
    pub config: &'a Config,
    pub authority: &'a Authority,
    pub keys: &'a Keys,
    pub custom: &'a mut State,
    pub next_id: &'a mut u64,
    pub events: &'a mut Vec<Event>,
}
pub(super) fn frame(
    worker: &mut Worker,
    state: &DeviceState,
    frame: &NormalizedFrame,
    context: FrameContext<'_>,
) {
    if !context.custom.ready || frame.raw.at < context.custom.ready_at {
        if let Recognizer::Custom { blocked, .. } = &mut worker.recognizer {
            if !state.keys.is_empty() || !state.contacts.is_empty() {
                blocked.insert(state.device.source.clone());
            } else {
                blocked.remove(&state.device.source);
            }
        }
        return;
    }
    let Trigger::Custom {
        recognizer,
        timeout,
        wake,
    } = &worker.rule.trigger
    else {
        unreachable!()
    };
    let definition = recognizer.clone();
    let (timeout, wake) = (timeout.duration(), wake.map(|wake| wake.duration()));
    let source = &state.device.source;
    let at = frame.raw.at;
    let Recognizer::Custom { blocked, .. } = &mut worker.recognizer else {
        unreachable!()
    };
    if blocked.contains(source) {
        if state.keys.is_empty() && state.contacts.is_empty() {
            blocked.remove(source);
        }
        return;
    }
    let Some(input) = Input::frame(context.config, &state.device, frame.clone())
        .filter(|input| definition.interested(input))
    else {
        return;
    };
    let input = match definition.project(&input, true) {
        Ok(input) => input,
        Err(_) => {
            cancel_candidate(
                worker,
                source,
                at,
                "invalid custom input projection",
                context.events,
            );
            return;
        }
    };
    let mut starts = frame.changes.iter().any(|change| match change {
        Change::Key {
            state: KeyState::Press,
            ..
        } => definition.inputs.contains(&InputKind::Key),
        Change::Relative { value, .. } => {
            *value != 0 && definition.inputs.contains(&InputKind::Relative)
        }
        Change::Wheel { steps, .. } => {
            *steps != 0.0 && definition.inputs.contains(&InputKind::Wheel)
        }
        Change::Absolute { .. } => definition.inputs.contains(&InputKind::Absolute),
        Change::Switch { on: true, .. } => definition.inputs.contains(&InputKind::Switch),
        _ => false,
    }) || definition.inputs.contains(&InputKind::Touch)
        && frame
            .contacts
            .iter()
            .any(|contact| contact.fresh && frame.contacts_started.contains(&contact.id));
    if definition.continuous && worker.rule.delivery != crate::config::Delivery::Observe {
        starts = definition.inputs.contains(&InputKind::Key)
            && frame.changes.iter().any(|change| {
                matches!(
                    change,
                    Change::Key {
                        state: KeyState::Press,
                        ..
                    }
                )
            })
            || definition.inputs.contains(&InputKind::Touch)
                && frame
                    .contacts
                    .iter()
                    .any(|contact| contact.fresh && frame.contacts_started.contains(&contact.id));
    }
    if matches!(&worker.recognizer, Recognizer::Custom { closed, .. } if closed.contains(source))
        && starts
    {
        if let Some(old) = worker.candidates.get(source) {
            context
                .custom
                .gate
                .retire_with(old.id, Retirement::Cancelled);
            context.custom.live.remove(&old.id);
        }
        cancel_candidate(
            worker,
            source,
            at,
            "new physical group superseded custom recognition",
            context.events,
        );
        if let Recognizer::Custom { blocked, .. } = &mut worker.recognizer {
            blocked.remove(source);
        }
    }
    if !worker.candidates.contains_key(source) {
        if !starts
            || context.custom.live.len() + context.custom.gate.retiring()
                >= recognition::MAX_PENDING
            || !context.authority.fresh(at).is_ok_and(|snapshot| {
                snapshot.state == crate::context::State::Desktop && !snapshot.secure_overlay
            })
            || context.authority.generation() != context.custom.fence.session
        {
            return;
        }
        let positions = if definition.inputs.contains(&InputKind::Touch) {
            match state
                .mapping
                .as_ref()
                .map(|mapping| {
                    frame
                        .contacts
                        .iter()
                        .map(|contact| mapping.map(contact.x, contact.y))
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()
            {
                Ok(positions) => positions.unwrap_or_default(),
                Err(_) => return,
            }
        } else {
            Vec::new()
        };
        start_candidate(
            worker,
            state,
            context.authority,
            context.keys,
            (at, &positions),
            context.next_id,
            context.events,
        );
        if let Some(candidate) = worker.candidates.get_mut(source) {
            candidate.expires = at.checked_add(timeout);
            context.custom.live.insert(candidate.id);
            if let Recognizer::Custom { wakes, .. } = &mut worker.recognizer
                && let Some(wake) = wake
            {
                wakes.insert(source.clone(), at.saturating_add(wake));
            }
        }
    } else if let Some(candidate) = worker.candidates.get_mut(source) {
        // Freeze scope against every joined contact before exposing the frame.
        if let Some(permit) = candidate.permit.as_mut() {
            for contact in frame
                .contacts
                .iter()
                .filter(|contact| frame.contacts_started.contains(&contact.id))
            {
                if candidate.contacts.contains(&contact.id) {
                    continue;
                }
                let position = state
                    .mapping
                    .as_ref()
                    .and_then(|mapping| mapping.map(contact.x, contact.y).ok());
                if position.is_none_or(|position| {
                    context
                        .authority
                        .join(permit, &worker.rule, input_context(state, at), position)
                        .is_err()
                }) {
                    cancel_candidate(
                        worker,
                        source,
                        at,
                        "custom contact left frozen scope",
                        context.events,
                    );
                    return;
                }
                candidate.contacts.insert(contact.id);
            }
        }
    }
    if worker.rule.delivery != crate::config::Delivery::Observe
        && state.keys.is_empty()
        && state.contacts.is_empty()
        && let Recognizer::Custom { closed, .. } = &mut worker.recognizer
    {
        closed.insert(source.clone());
    }
    if let Some(candidate) = worker.candidates.get(source)
        && context.custom.offer(candidate.id, Some(input), at).is_err()
    {
        cancel_candidate(
            worker,
            source,
            at,
            "custom recognition queue exhausted",
            context.events,
        );
    }
}
fn input_context(state: &DeviceState, at: Duration) -> InputContext<'_> {
    super::input(state, at)
}
impl Engine {
    pub(crate) fn retire_routing_recognition(&mut self, source: &Source) {
        let at = self.input_at();
        let ids: Vec<_> = self
            .workers
            .iter()
            .filter(|worker| matches!(worker.rule.trigger, Trigger::Custom { .. }))
            .filter_map(|worker| worker.candidates.get(source).map(|candidate| candidate.id))
            .collect();
        for id in ids {
            self.custom_cancel(id, at, "routing transport retired");
        }
        self.custom_sync(at, Retirement::SourceLost);
    }
    pub(crate) fn recognition_ready(&mut self, ready: bool, at: Duration) -> io::Result<()> {
        let Some(custom) = self.custom.as_ref() else {
            return Ok(());
        };
        if custom.ready == ready {
            return Ok(());
        }
        if !ready {
            let ids: Vec<_> = self
                .workers
                .iter()
                .filter(|worker| matches!(worker.rule.trigger, Trigger::Custom { .. }))
                .flat_map(|worker| worker.candidates.values().map(|candidate| candidate.id))
                .collect();
            for id in ids {
                self.custom_cancel(id, at, "recognizer helper is unavailable");
            }
            self.custom_sync(at, Retirement::Cancelled);
        } else {
            for worker in &mut self.workers {
                if matches!(worker.rule.trigger, Trigger::Custom { .. }) {
                    for state in self.devices.values() {
                        worker.register(state, &self.config)?;
                    }
                }
            }
        }
        self.custom.as_mut().unwrap().ready = ready;
        if ready {
            self.custom.as_mut().unwrap().ready_at = at;
        }
        Ok(())
    }
    fn custom_intent(&self, id: u64, at: Duration) -> Option<RouteIntent> {
        let (worker, candidate) = self.workers.iter().find_map(|worker| {
            worker
                .candidates
                .values()
                .find(|candidate| candidate.id == id)
                .map(|candidate| (worker, candidate))
        })?;
        let custom = self.custom.as_ref()?;
        let state = self.devices.get(&candidate.source.device)?;
        let intent = RouteIntent {
            rule: worker.rule.clone(),
            candidate: candidate.clone(),
        };
        (matches!(worker.rule.trigger, Trigger::Custom { .. })
            && candidate.session_generation == custom.fence.session
            && selected(&self.config, &worker.rule, &state.device)
            && (!worker.rule.keys_throughout
                || conditions(&worker.rule, &self.conditions, &candidate.source.seat))
            && self.authority.fresh(at).is_ok_and(|snapshot| {
                snapshot.state == crate::context::State::Desktop && !snapshot.secure_overlay
            })
            && self.validate_route(&intent, at).is_ok())
        .then_some(intent)
    }
    fn custom_cancel(&mut self, id: u64, at: Duration, reason: &str) {
        if let Some(worker) = self.workers.iter_mut().find(|worker| {
            worker
                .candidates
                .values()
                .any(|candidate| candidate.id == id)
        }) {
            let source = worker
                .candidates
                .values()
                .find(|candidate| candidate.id == id)
                .unwrap()
                .source
                .clone();
            cancel_candidate(worker, &source, at, reason, &mut self.events);
        }
    }
    pub(super) fn custom_sync(&mut self, at: Duration, reason: Retirement) {
        let Some(custom) = self.custom.as_mut() else {
            return;
        };
        let live: BTreeSet<_> = self
            .workers
            .iter()
            .filter(|worker| matches!(worker.rule.trigger, Trigger::Custom { .. }))
            .flat_map(|worker| worker.candidates.values().map(|candidate| candidate.id))
            .collect();
        for id in custom.gate.candidates() {
            if !live.contains(&id) {
                custom.gate.retire_with(
                    id,
                    if self.authority.fresh(at).is_err() {
                        Retirement::ContextLost
                    } else {
                        reason
                    },
                );
            }
        }
        custom.live = live;
        custom
            .offers
            .retain(|offer| custom.live.contains(&offer.candidate));
        custom.bytes = custom.offers.iter().map(|offer| offer.bytes).sum();
        for worker in &mut self.workers {
            if let Recognizer::Custom {
                blocked,
                wakes,
                closed,
            } = &mut worker.recognizer
            {
                closed.retain(|source| worker.candidates.contains_key(source));
                blocked.retain(|source| {
                    self.devices.get(&source.device).is_some_and(|state| {
                        state.device.source == *source
                            && (!state.keys.is_empty() || !state.contacts.is_empty())
                    })
                });
                wakes.retain(|source, _| worker.candidates.contains_key(source));
            }
        }
    }
    pub(super) fn custom_advance(&mut self, at: Duration) {
        let overdue = self
            .custom
            .as_ref()
            .map_or_else(Vec::new, |custom| custom.gate.overdue(at));
        for id in overdue {
            self.custom_cancel(id, at, "custom recognizer response expired");
        }
        self.custom_sync(at, Retirement::Expired);
        let Some(custom) = self.custom.as_mut() else {
            return;
        };
        custom.gate.expire(at);
        if !custom.ready {
            return;
        }
        for worker in &mut self.workers {
            let Trigger::Custom {
                wake: Some(interval),
                ..
            } = &worker.rule.trigger
            else {
                continue;
            };
            let Recognizer::Custom { wakes, closed, .. } = &mut worker.recognizer else {
                continue;
            };
            for (source, wake) in wakes {
                let id = worker.candidates[source].id;
                if closed.contains(source) {
                    continue;
                }
                if *wake <= at
                    && !custom.gate.waiting(id)
                    && !custom
                        .offers
                        .iter()
                        .any(|offer| offer.candidate == id && offer.input.is_none())
                    && custom.offer(id, None, at).is_ok()
                {
                    *wake = at.saturating_add(interval.duration());
                }
            }
        }
    }
    pub(super) fn custom_deadline(&self) -> Option<Duration> {
        let custom = self.custom.as_ref()?;
        custom
            .gate
            .deadline()
            .into_iter()
            .chain(self.workers.iter().filter_map(|worker| {
                let Recognizer::Custom { wakes, closed, .. } = &worker.recognizer else {
                    return None;
                };
                wakes
                    .iter()
                    .filter(|(source, _)| {
                        !closed.contains(*source)
                            && !custom.gate.waiting(worker.candidates[*source].id)
                            && !custom
                                .offers
                                .iter()
                                .any(|offer| offer.candidate == worker.candidates[*source].id)
                    })
                    .map(|(_, at)| *at)
                    .min()
            }))
            .min()
    }
    pub fn recognition_requests(&mut self, at: Duration) -> io::Result<Vec<Ticket>> {
        self.recognition_requests_with(at, |_, _, _| (None, None))
    }
    pub(crate) fn recognition_requests_with(
        &mut self,
        at: Duration,
        ownership: impl Fn(&Source, &Rule, u64) -> (Option<Ownership>, Option<u64>),
    ) -> io::Result<Vec<Ticket>> {
        self.advance(at)?;
        let Some(custom) = self.custom.as_mut() else {
            return Ok(Vec::new());
        };
        let offers = std::mem::take(&mut custom.offers);
        custom.bytes = 0;
        let mut tickets = Vec::new();
        for offer in offers {
            let Some(intent) = self.custom_intent(offer.candidate, at) else {
                self.custom_cancel(offer.candidate, at, "custom eligibility changed");
                continue;
            };
            let (ownership, winner) = ownership(intent.source(), &intent.rule, intent.id());
            let current = self.custom_candidate(&intent, ownership, winner)?;
            let Trigger::Custom { recognizer, .. } = &intent.rule.trigger else {
                unreachable!()
            };
            if at
                >= offer
                    .at
                    .saturating_add(recognizer.reply_within.duration())
                    .min(current.expires)
            {
                self.custom_cancel(offer.candidate, at, "queued custom request expired");
                self.custom
                    .as_mut()
                    .unwrap()
                    .gate
                    .retire_with(offer.candidate, Retirement::Expired);
                continue;
            }
            let gate = &mut self.custom.as_mut().unwrap().gate;
            let result = match offer.input {
                Some(input) => gate.offer(&recognizer.name, &current, &input, at),
                None => gate.offer_timer_at(&recognizer.name, &current, offer.at, at),
            };
            match result {
                Ok(ticket) => tickets.push(ticket),
                Err(_) => self.custom_cancel(offer.candidate, at, "custom ticket refused"),
            }
        }
        self.custom_sync(at, Retirement::Cancelled);
        tickets.retain(|ticket| {
            self.custom
                .as_ref()
                .unwrap()
                .gate
                .candidate(ticket.id)
                .is_some()
        });
        Ok(tickets)
    }
    fn custom_candidate(
        &self,
        intent: &RouteIntent,
        ownership: Option<Ownership>,
        winner: Option<u64>,
    ) -> io::Result<recognition::Candidate> {
        Ok(recognition::Candidate {
            id: intent.id(),
            revision: self.rule_revision,
            rule: intent.rule.id.clone(),
            source: intent.source().clone(),
            ownership,
            winner,
            expires: intent
                .candidate
                .expires
                .ok_or_else(|| invalid("custom candidate has no expiry"))?,
        })
    }
    pub fn recognition_response(&mut self, proposal: Proposal, at: Duration) -> io::Result<()> {
        self.recognition_response_with(proposal, at, |_, _, _| (None, None))
    }
    pub(crate) fn recognition_response_with(
        &mut self,
        proposal: Proposal,
        at: Duration,
        ownership: impl Fn(&Source, &Rule, u64) -> (Option<Ownership>, Option<u64>),
    ) -> io::Result<()> {
        self.recognition_response_clocked(proposal, at, at, ownership)
    }
    /// The live reader drains input to its watermark before admitting replies.
    /// Wall-clock validation must not advance input timers past unread frames.
    pub(crate) fn recognition_response_after_input(
        &mut self,
        proposal: Proposal,
        input_at: Duration,
        now: Duration,
    ) -> io::Result<()> {
        self.recognition_response_clocked(proposal, input_at, now, |_, _, _| (None, None))
    }
    pub(crate) fn recognition_response_clocked(
        &mut self,
        proposal: Proposal,
        input_at: Duration,
        at: Duration,
        ownership: impl Fn(&Source, &Rule, u64) -> (Option<Ownership>, Option<u64>),
    ) -> io::Result<()> {
        if input_at > at || self.last_at.is_some_and(|last| input_at < last) {
            return Err(invalid("custom reply clock regressed"));
        }
        self.advance(input_at)?;
        let id = self
            .custom
            .as_ref()
            .and_then(|custom| custom.gate.candidate(proposal.ticket))
            .ok_or_else(|| invalid("unknown custom recognition ticket"))?;
        let intent = self
            .custom_intent(id, at)
            .ok_or_else(|| invalid("custom recognition is no longer eligible"))?;
        let (ownership, winner) = ownership(intent.source(), &intent.rule, intent.id());
        let candidate = self.custom_candidate(&intent, ownership, winner)?;
        let validation = self
            .custom
            .as_mut()
            .unwrap()
            .gate
            .validate(proposal, &candidate, at);
        let Some(validated) = (match validation {
            Ok(value) => value,
            Err(error) => {
                self.custom_cancel(id, at, "custom proposal failed native validation");
                self.custom_sync(at, Retirement::Cancelled);
                return Err(error);
            }
        }) else {
            return Ok(());
        };
        let phase = match validated.phase {
            recognition::Phase::Pending => unreachable!(),
            recognition::Phase::Discrete => Stage::Discrete,
            recognition::Phase::Begin => Stage::Begin,
            recognition::Phase::Update => Stage::Update,
            recognition::Phase::End => Stage::End,
            recognition::Phase::Cancel => Stage::Cancel,
        };
        if matches!(phase, Stage::Begin | Stage::Update)
            && self.workers.iter().find(|worker| worker.rule.id == intent.rule.id).is_some_and(|worker| matches!(&worker.recognizer, Recognizer::Custom { closed, .. } if closed.contains(intent.source()))) {
            self.custom_cancel(id, at, "custom progress arrived after physical completion");
            self.custom_sync(at, Retirement::Cancelled);
            return Err(invalid("custom progress arrived after physical completion"));
        }
        let worker = self
            .workers
            .iter_mut()
            .find(|worker| worker.rule.id == intent.rule.id)
            .unwrap();
        if worker.candidates[intent.source()].ordinal.checked_add(1) != Some(validated.ordinal) {
            return Err(invalid("custom ordinal diverged from native rule"));
        }
        report_signals(
            worker,
            vec![Signal::Custom {
                source: intent.source().clone(),
                stage: phase,
                data: validated.data,
            }],
            self.generation,
            validated.observed_at,
            &mut self.events,
        );
        if let Some(trace) = worker.matches.last_mut()
            && matches!(&trace.event, Event::Matched { id, ordinal, .. } if *id == intent.id() && *ordinal == validated.ordinal)
        {
            trace.expires = Some(validated.deadline);
        }
        if matches!(phase, Stage::Discrete | Stage::End | Stage::Cancel) {
            worker.candidates.remove(intent.source());
            if let Recognizer::Custom {
                blocked,
                wakes,
                closed,
            } = &mut worker.recognizer
            {
                blocked.insert(intent.source().clone());
                wakes.remove(intent.source());
                closed.remove(intent.source());
            }
            self.custom.as_mut().unwrap().gate.retire_with(
                id,
                if phase == Stage::Cancel {
                    Retirement::Cancelled
                } else {
                    Retirement::Completed
                },
            );
        }
        self.custom_sync(at, Retirement::Cancelled);
        Ok(())
    }
    pub fn take_recognition_cancellations(&mut self) -> Vec<recognition::Cancellation> {
        self.custom
            .as_mut()
            .map_or_else(Vec::new, |custom| custom.gate.take_cancellations())
    }
    pub(crate) fn cancel_custom(&mut self, id: u64, at: Duration, reason: &str) {
        self.custom_cancel(id, at, reason);
        self.custom_sync(at, Retirement::Cancelled);
    }
}
