//! Bounded per-device ownership and scheduled raw-frame delivery.
//!
//! This is a deterministic transport model, not an evdev/uinput backend. A
//! trusted policy/context layer chooses candidates; recognizers must claim
//! before passthrough commits. The caller writes each due frame and then
//! acknowledges it. Failed writes require descriptor teardown and recovery.

use crate::{
    input::{
        ABS_MT_SLOT, ABS_MT_TRACKING_ID, Device, EV_ABS, EV_KEY, EV_REL, EV_SW, EV_SYN, Frame,
        RawEvent, SYN_REPORT, Source,
    },
    transport::Ready,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::Duration,
};
mod chain;
mod keyboard;
use chain::Retention;
pub use chain::{ChainAdmission, ChainToken, ClaimedChain};
pub use keyboard::{ConsumedKey, InputSpan, KeyClaim, KeyProjection};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Policy {
    Observe,
    /// Only a trusted preflight may dedicate a verified noninteractive scope.
    Reserve,
    Buffer {
        join: Duration,
        decision: Duration,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub id: u64,
    pub authorization: u64,
    pub policy: Policy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ownership {
    Forwarded,
    Pending,
    Claimed,
    Draining,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub frames: usize,
    pub events: usize,
    pub offset: Duration,
    pub scheduler_slack: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            frames: 1024,
            events: 32768,
            offset: Duration::from_millis(300),
            scheduler_slack: Duration::from_millis(20),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scheduled {
    pub ticket: u64,
    pub due: Duration,
    pub frame: Frame,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    StaleSource,
    InvalidFrame,
    BackwardTime,
    Overflow,
    MissedDelivery,
    Stopped,
    InvalidCandidate,
    Unauthorized,
    TooLate,
    WrongTicket,
    UnsupportedProjection,
}

#[derive(Clone, Debug, Default)]
struct State {
    keys: BTreeSet<u16>,
    contacts: BTreeMap<u16, i32>,
    slot: u16,
}

impl State {
    fn neutral(&self) -> bool {
        self.keys.is_empty() && self.contacts.is_empty()
    }

    fn apply(&mut self, frame: &Frame, device: &Device) -> Result<bool, Fault> {
        let mut starts = false;
        for event in &frame.events {
            if event.kind == EV_SYN {
                continue;
            }
            if !device.has(event.kind, event.code) {
                return Err(Fault::InvalidFrame);
            }
            match event.kind {
                EV_KEY => match event.value {
                    0 => {
                        self.keys.remove(&event.code);
                    }
                    1 => {
                        starts |= self.keys.insert(event.code);
                    }
                    2 if self.keys.contains(&event.code) => {}
                    _ => return Err(Fault::InvalidFrame),
                },
                EV_ABS if event.code == ABS_MT_SLOT => {
                    let slot = u16::try_from(event.value).map_err(|_| Fault::InvalidFrame)?;
                    let axis = device.axes.get(&ABS_MT_SLOT).ok_or(Fault::InvalidFrame)?;
                    if !(axis.minimum..=axis.maximum).contains(&event.value) {
                        return Err(Fault::InvalidFrame);
                    }
                    self.slot = slot;
                }
                EV_ABS if event.code == ABS_MT_TRACKING_ID => match event.value {
                    -1 => {
                        self.contacts.remove(&self.slot);
                    }
                    value if value >= 0 => {
                        starts |= self.contacts.insert(self.slot, value).is_none();
                    }
                    _ => return Err(Fault::InvalidFrame),
                },
                _ => {}
            }
        }
        Ok(starts)
    }

    fn releases(&self) -> Vec<RawEvent> {
        let mut events = Vec::new();
        // BTN_TOUCH comes first for the kernel's legacy mouse emulation path.
        if self.keys.contains(&0x14a) {
            events.push(RawEvent {
                kind: EV_KEY,
                code: 0x14a,
                value: 0,
            });
        }
        events.extend(
            self.keys
                .iter()
                .filter(|key| **key != 0x14a)
                .map(|key| RawEvent {
                    kind: EV_KEY,
                    code: *key,
                    value: 0,
                }),
        );
        for slot in self.contacts.keys() {
            events.push(RawEvent {
                kind: EV_ABS,
                code: ABS_MT_SLOT,
                value: i32::from(*slot),
            });
            events.push(RawEvent {
                kind: EV_ABS,
                code: ABS_MT_TRACKING_ID,
                value: -1,
            });
        }
        if !events.is_empty() {
            events.push(RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            });
        }
        events
    }
}

#[derive(Debug)]
struct Group {
    candidate: Option<Candidate>,
    first: Duration,
    ownership: Ownership,
    pending: Vec<Frame>,
    preview: Option<crate::input::AbsoluteState>,
    completion: bool,
    chain: bool,
    first_segment: u64,
    last_segment: u64,
    last_first: Duration,
}

/// All descriptor lifetime operations remain the backend's responsibility.
/// Teardown is mandatory even if neutral events cannot be written (including
/// partial/uncertain writes); returning this value is not proof of recovery.
#[derive(Debug)]
pub struct Recovery {
    pub source: Source,
    pub neutral_events: Vec<RawEvent>,
    pub destroy_virtual_and_release_grab: bool,
}

#[derive(Debug)]
pub struct Router {
    device: Device,
    authorization: u64,
    limits: Limits,
    physical: State,
    delivered: State,
    physical_absolute: crate::input::AbsoluteState,
    planned_absolute: crate::input::AbsoluteState,
    group: Option<Group>,
    queue: VecDeque<Scheduled>,
    offset: Duration,
    last_at: Duration,
    delivery_at: Duration,
    last_sequence: Option<u64>,
    next_ticket: u64,
    last_candidate: u64,
    next_segment: u64,
    stopped: bool,
    recovered: bool,
    keyboard: Option<keyboard::Ledger>,
    positions: BTreeMap<u64, InputSpan>,
}

impl Router {
    pub fn new(
        device: Device,
        ready: &Ready,
        authorization: u64,
        limits: Limits,
        at: Duration,
    ) -> Result<Self, Fault> {
        if !ready.matches(&device)
            || !ready.valid_at(at)
            || authorization == 0
            || limits.frames == 0
            || limits.frames > 4096
            || limits.events < 2
            || limits.events > 262144
            || limits.offset.is_zero()
            || limits.offset > Duration::from_secs(5)
            || limits.scheduler_slack > Duration::from_millis(100)
        {
            return Err(Fault::InvalidCandidate);
        }
        Ok(Self {
            device,
            authorization,
            limits,
            physical: State::default(),
            delivered: State::default(),
            physical_absolute: crate::input::AbsoluteState::default(),
            planned_absolute: crate::input::AbsoluteState::default(),
            group: None,
            queue: VecDeque::new(),
            offset: Duration::ZERO,
            last_at: at,
            delivery_at: at,
            last_sequence: None,
            next_ticket: 0,
            last_candidate: 0,
            next_segment: 0,
            stopped: false,
            recovered: false,
            keyboard: None,
            positions: BTreeMap::new(),
        })
    }

    pub fn ownership(&self) -> Option<Ownership> {
        self.group.as_ref().map(|group| group.ownership)
    }
    pub fn physical_neutral(&self) -> bool {
        self.physical.neutral()
    }
    pub(crate) fn completion_pending(&self) -> bool {
        self.group.as_ref().is_some_and(|group| group.completion)
    }
    pub(crate) fn release_completion(&mut self) {
        if let Some(group) = &mut self.group {
            group.completion = false;
            group.chain = false;
        }
        if self.physical.neutral() && self.ownership() != Some(Ownership::Pending) {
            self.group = None;
        }
        self.reset_epoch();
    }
    pub fn queued_frames(&self) -> usize {
        self.queue.len()
    }
    pub fn delivery_offset(&self) -> Duration {
        self.offset
    }

    pub fn next_deadline(&self) -> Option<Duration> {
        let decision = self.group.as_ref().and_then(|group| {
            if group.ownership != Ownership::Pending {
                return None;
            }
            match group.candidate?.policy {
                Policy::Buffer { decision, .. } => group.first.checked_add(decision),
                _ => None,
            }
        });
        self.queue
            .front()
            .map(|entry| entry.due)
            .into_iter()
            .chain(decision)
            .min()
    }

    pub fn seed_absolute(&mut self, state: &crate::input::AbsoluteState) -> Result<(), Fault> {
        if self.last_sequence.is_some()
            || self.group.is_some()
            || !self.queue.is_empty()
            || self.stopped
        {
            return Err(Fault::Stopped);
        }
        state
            .validate(&self.device.axes)
            .map_err(|_| Fault::InvalidFrame)?;
        self.physical.slot = state.selected_slot();
        self.delivered.slot = state.selected_slot();
        self.physical_absolute = state.clone();
        self.planned_absolute = state.clone();
        Ok(())
    }

    fn fail<T>(&mut self, fault: Fault) -> Result<T, Fault> {
        self.stopped = true;
        Err(fault)
    }

    fn storage(&self) -> (usize, usize) {
        let pending = self
            .group
            .as_ref()
            .map(|group| group.pending.as_slice())
            .unwrap_or_default();
        (
            self.queue.len() + pending.len(),
            self.queue
                .iter()
                .map(|entry| entry.frame.events.len())
                .sum::<usize>()
                + pending
                    .iter()
                    .map(|frame| frame.events.len())
                    .sum::<usize>(),
        )
    }

    fn capacity(&mut self, count: usize) -> Result<(), Fault> {
        let (frames, events) = self.storage();
        if frames >= self.limits.frames || events.saturating_add(count) > self.limits.events {
            self.fail(Fault::Overflow)
        } else {
            Ok(())
        }
    }

    fn schedule(&mut self, frame: Frame) -> Result<(), Fault> {
        self.capacity(frame.events.len())?;
        let Some(due) = frame.at.checked_add(self.offset) else {
            return self.fail(Fault::Overflow);
        };
        if self.queue.back().is_some_and(|previous| previous.due > due) {
            return self.fail(Fault::BackwardTime);
        }
        let Some(ticket) = self.next_ticket.checked_add(1) else {
            return self.fail(Fault::Overflow);
        };
        self.next_ticket = ticket;
        for event in &frame.events {
            self.planned_absolute.update(*event);
        }
        self.queue.push_back(Scheduled { ticket, due, frame });
        Ok(())
    }

    fn reset_epoch(&mut self) {
        if self.physical.neutral()
            && self.delivered.neutral()
            && self.queue.is_empty()
            && self.group.is_none()
        {
            self.offset = Duration::ZERO;
        }
    }

    fn forward(&mut self, at: Duration) -> Result<(), Fault> {
        let Some(group) = &mut self.group else {
            return Err(Fault::WrongTicket);
        };
        if group.ownership != Ownership::Pending {
            return Err(Fault::TooLate);
        }
        let delay = at.saturating_sub(group.first);
        if delay > self.limits.offset {
            return self.fail(Fault::MissedDelivery);
        }
        self.offset = self.offset.max(delay);
        group.ownership = Ownership::Forwarded;
        group.completion = false;
        group.chain = false;
        group.preview = None;
        let pending = std::mem::take(&mut group.pending);
        for frame in pending {
            self.schedule(frame)?;
        }
        if self.physical.neutral() {
            self.group = None;
        }
        Ok(())
    }

    /// Advance the scheduler to a safe input watermark: callers must drain
    /// older physical frames first. Decision timeouts commit ordinary delivery.
    pub fn advance(&mut self, at: Duration) -> Result<(), Fault> {
        if self.stopped {
            return Err(Fault::Stopped);
        }
        if at < self.last_at {
            return self.fail(Fault::BackwardTime);
        }
        if let Some(group) = &self.group
            && group.ownership == Ownership::Pending
            && let Some(Candidate {
                policy: Policy::Buffer { decision, .. },
                ..
            }) = group.candidate
        {
            let Some(deadline) = group.first.checked_add(decision) else {
                return self.fail(Fault::Overflow);
            };
            if at >= deadline {
                self.forward(deadline)?;
            }
        }
        self.last_at = at;
        if self
            .queue
            .front()
            .is_some_and(|first| at.saturating_sub(first.due) > self.limits.scheduler_slack)
        {
            return self.fail(Fault::MissedDelivery);
        }
        self.reset_epoch();
        Ok(())
    }

    pub fn push(
        &mut self,
        frame: Frame,
        candidate: Option<Candidate>,
        at: Duration,
    ) -> Result<(), Fault> {
        self.push_with_completion(frame, candidate, at, false)
    }
    pub(crate) fn push_with_completion(
        &mut self,
        frame: Frame,
        candidate: Option<Candidate>,
        at: Duration,
        completion: bool,
    ) -> Result<(), Fault> {
        self.push_inner(frame, candidate, at, Retention::Single(completion))
    }
    fn push_inner(
        &mut self,
        frame: Frame,
        candidate: Option<Candidate>,
        at: Duration,
        retention: Retention<'_>,
    ) -> Result<(), Fault> {
        // A delayed old reader must not stop a newly connected stream.
        if frame.source != self.device.source {
            return Err(Fault::StaleSource);
        }
        if self.stopped {
            return Err(Fault::Stopped);
        }
        if self.keyboard.is_some() {
            return self.fail(Fault::InvalidCandidate);
        }
        if frame.at > at
            || frame.at < self.last_at
            || self.last_sequence.is_some_and(|seq| frame.sequence <= seq)
        {
            return self.fail(Fault::BackwardTime);
        }
        if frame.events.is_empty()
            || frame.events.len() > 4096
            || frame
                .events
                .last()
                .is_none_or(|event| event.kind != EV_SYN || event.code != SYN_REPORT)
            || frame.events[..frame.events.len() - 1]
                .iter()
                .any(|event| event.kind == EV_SYN)
        {
            return self.fail(Fault::InvalidFrame);
        }
        let mut physical = self.physical.clone();
        let starts = match physical.apply(&frame, &self.device) {
            Ok(starts) => starts,
            Err(error) => return self.fail(error),
        };
        let begins = starts
            || (candidate.is_some() || matches!(retention, Retention::Continue(_)))
                && frame.events.iter().any(|event| {
                    matches!(event.kind, crate::input::EV_REL | crate::input::EV_SW)
                        && event.value != 0
                });
        let completed = self.physical.neutral()
            && self.completion_pending()
            && (begins
                || frame
                    .events
                    .iter()
                    .any(|event| event.kind == EV_REL && event.value != 0 || event.kind == EV_SW));
        match retention {
            Retention::Start => {
                if candidate.is_none_or(|candidate| candidate.policy == Policy::Observe)
                    || self.group.as_ref().is_some_and(|group| group.chain)
                {
                    return Err(Fault::InvalidCandidate);
                }
            }
            Retention::Continue(token) => {
                self.continuation(token)?;
                if candidate.is_some() || !self.physical.neutral() || !begins {
                    return Err(Fault::InvalidCandidate);
                }
            }
            Retention::Single(_) => {}
        }
        // Reject caller mistakes before mutating physical state or decisions.
        if let Some(value) = candidate
            && (!begins
                || !completed
                    && self.group.as_ref().is_some_and(|group| {
                        !self.physical.neutral() || group.ownership != Ownership::Pending
                    })
                || value.id <= self.last_candidate
                || value.authorization != self.authorization
                || matches!(value.policy, Policy::Buffer { join, decision }
                    if join > decision || decision.is_zero() || decision > self.limits.offset))
        {
            return Err(Fault::InvalidCandidate);
        }
        self.advance(frame.at)?;
        let joining = matches!(retention,Retention::Continue(token)
            if self.chain_ownership(token,frame.at).is_some());
        if completed && !joining {
            self.release_completion();
        }
        // A completed pending tap may be followed by another group before its
        // decision deadline. Commit that tap before accepting the new start.
        if self.physical.neutral()
            && !joining
            && self
                .group
                .as_ref()
                .is_some_and(|group| group.ownership == Ownership::Pending)
        {
            self.forward(frame.at)?;
        }
        if joining {
            let group = self.group.as_mut().unwrap();
            if group.last_segment - group.first_segment + 1 >= self.limits.frames as u64 {
                return self.fail(Fault::Overflow);
            }
            let Some(serial) = self.next_segment.checked_add(1) else {
                return self.fail(Fault::Overflow);
            };
            self.next_segment = serial;
            group.last_segment = serial;
            group.last_first = frame.at;
        }
        if self.group.is_none() && begins {
            if let Some(value) = candidate {
                self.last_candidate = value.id;
            }
            let ownership = match candidate.map(|value| value.policy) {
                Some(Policy::Buffer { .. }) => Ownership::Pending,
                Some(Policy::Reserve) => Ownership::Claimed,
                Some(Policy::Observe) | None => Ownership::Forwarded,
            };
            let Some(serial) = self.next_segment.checked_add(1) else {
                return self.fail(Fault::Overflow);
            };
            self.next_segment = serial;
            self.group = Some(Group {
                candidate,
                first: frame.at,
                ownership,
                pending: Vec::new(),
                preview: (ownership == Ownership::Pending).then(|| self.planned_absolute.clone()),
                completion: retention.retains() && candidate.is_some(),
                chain: matches!(retention, Retention::Start),
                first_segment: serial,
                last_segment: serial,
                last_first: frame.at,
            });
        } else if candidate.is_some() {
            return Err(Fault::InvalidCandidate);
        }
        let mut frame = frame;
        let ownership = self.ownership();
        let repairs = if !matches!(ownership, Some(Ownership::Claimed | Ownership::Draining)) {
            let preview = if ownership == Some(Ownership::Pending) {
                self.group.as_ref().unwrap().preview.as_ref().unwrap()
            } else {
                &self.planned_absolute
            };
            let repairs = self.physical_absolute.restore_for(preview, &frame);
            if frame.events.len().saturating_add(repairs.len()) > 4096 {
                return self.fail(Fault::Overflow);
            }
            repairs
        } else {
            Vec::new()
        };
        // Track the physical delta, not the inserted virtual-state repair.
        for event in &frame.events {
            self.physical_absolute.update(*event);
        }
        let insert_at = frame
            .events
            .iter()
            .position(|event| event.kind == EV_ABS)
            .unwrap_or(frame.events.len() - 1);
        frame.events.splice(insert_at..insert_at, repairs);
        self.physical = physical;
        self.last_sequence = Some(frame.sequence);
        match self.ownership() {
            Some(Ownership::Pending) => {
                self.capacity(frame.events.len())?;
                let preview = self.group.as_mut().unwrap().preview.as_mut().unwrap();
                for event in &frame.events {
                    preview.update(*event);
                }
                self.group.as_mut().unwrap().pending.push(frame);
            }
            Some(Ownership::Claimed | Ownership::Draining) => {}
            Some(Ownership::Forwarded) | None => self.schedule(frame)?,
        }
        if self.physical.neutral()
            && self.ownership() != Some(Ownership::Pending)
            && !self.completion_pending()
        {
            self.group = None;
        }
        self.advance(at)
    }

    fn authorized(&self, id: u64, authorization: u64) -> Result<(), Fault> {
        if self.stopped {
            return Err(Fault::Stopped);
        }
        if authorization != self.authorization {
            return Err(Fault::Unauthorized);
        }
        if self
            .group
            .as_ref()
            .and_then(|group| group.candidate)
            .is_none_or(|candidate| candidate.id != id)
        {
            return Err(Fault::WrongTicket);
        }
        Ok(())
    }

    pub fn claim(&mut self, id: u64, authorization: u64, at: Duration) -> Result<(), Fault> {
        self.claim_inner(id, authorization, at, false)
    }
    /// A fully recognized, already-neutral interaction cannot admit another
    /// contact. The trusted recognizer may resolve it before the join deadline.
    pub fn claim_finished(
        &mut self,
        id: u64,
        authorization: u64,
        at: Duration,
    ) -> Result<(), Fault> {
        if !self.physical.neutral() {
            return Err(Fault::InvalidCandidate);
        }
        self.claim_inner(id, authorization, at, true)
    }
    fn claim_inner(
        &mut self,
        id: u64,
        authorization: u64,
        at: Duration,
        finished: bool,
    ) -> Result<(), Fault> {
        self.authorized(id, authorization)?;
        self.advance(at)?;
        let group = self.group.as_mut().ok_or(Fault::TooLate)?;
        if group.ownership != Ownership::Pending {
            return Err(Fault::TooLate);
        }
        if let Some(Candidate {
            policy: Policy::Buffer { join, .. },
            ..
        }) = group.candidate
            && !finished
            && at.saturating_sub(group.last_first) < join
        {
            return Err(Fault::TooLate);
        }
        group.pending.clear();
        group.preview = None;
        group.ownership = Ownership::Claimed;
        if self.physical.neutral() && !group.completion {
            self.group = None;
        }
        self.reset_epoch();
        Ok(())
    }

    pub fn claimable(&self, id: u64, authorization: u64, at: Duration) -> Result<(), Fault> {
        self.authorized(id, authorization)?;
        let group = self.group.as_ref().ok_or(Fault::TooLate)?;
        if at < self.last_at {
            return Err(Fault::BackwardTime);
        }
        if group.ownership != Ownership::Pending {
            return Err(Fault::TooLate);
        }
        if let Some(Candidate {
            policy: Policy::Buffer { join, decision },
            ..
        }) = group.candidate
            && (at >= group.first + decision
                || !self.physical.neutral() && at < group.last_first + join)
        {
            return Err(Fault::TooLate);
        }
        Ok(())
    }
    /// A rejected pre-claim candidate returns to its ordinary path. Once
    /// claimed, cancellation consumes the remaining releases through neutral.
    pub fn cancel(&mut self, id: u64, authorization: u64, at: Duration) -> Result<(), Fault> {
        self.authorized(id, authorization)?;
        self.advance(at)?;
        self.cancel_at(at)
    }
    pub(crate) fn cancel_after_input(
        &mut self,
        id: u64,
        authorization: u64,
        input_at: Duration,
        now: Duration,
    ) -> Result<(), Fault> {
        self.authorized(id, authorization)?;
        if input_at > now || now < self.delivery_at {
            return self.fail(Fault::BackwardTime);
        }
        self.advance(input_at)?;
        let mut release_at = now;
        if self.ownership() == Some(Ownership::Pending) {
            let group = self.group.as_ref().unwrap();
            if let Some(Candidate {
                policy: Policy::Buffer { decision, .. },
                ..
            }) = group.candidate
            {
                let Some(deadline) = group.first.checked_add(decision) else {
                    return self.fail(Fault::Overflow);
                };
                release_at = release_at.min(deadline);
            }
        }
        self.cancel_at(release_at)?;
        self.delivery_clock(now)
    }
    fn cancel_at(&mut self, at: Duration) -> Result<(), Fault> {
        match self.ownership() {
            Some(Ownership::Pending) => self.forward(at)?,
            Some(Ownership::Claimed) => {
                self.group.as_mut().unwrap().ownership = Ownership::Draining;
                self.group.as_mut().unwrap().completion = false;
                self.group.as_mut().unwrap().chain = false;
            }
            _ => return Err(Fault::TooLate),
        }
        if self.physical.neutral() {
            self.group = None;
        }
        self.reset_epoch();
        Ok(())
    }

    pub fn due(&mut self, at: Duration) -> Result<Option<&Scheduled>, Fault> {
        self.advance(at)?;
        self.due_after_input(at)
    }

    pub(crate) fn prefix_delivered(&self, sequence: u64, inclusive: bool) -> bool {
        let before =
            |frame: &Frame| frame.sequence < sequence || inclusive && frame.sequence == sequence;
        !self.queue.iter().any(|entry| {
            self.positions.get(&entry.ticket).map_or_else(
                || before(&entry.frame),
                |span| span.sequence < sequence || inclusive && span.sequence == sequence,
            )
        }) && !self.group.as_ref().is_some_and(|group| {
            group.ownership == Ownership::Pending && group.pending.iter().any(before)
        })
    }
    pub(crate) fn due_after_input(&mut self, at: Duration) -> Result<Option<&Scheduled>, Fault> {
        self.delivery_clock(at)?;
        Ok(self.queue.front().filter(|entry| entry.due <= at))
    }
    fn delivery_clock(&mut self, at: Duration) -> Result<(), Fault> {
        if self.stopped {
            return Err(Fault::Stopped);
        }
        if at < self.last_at || at < self.delivery_at {
            return self.fail(Fault::BackwardTime);
        }
        if self
            .queue
            .front()
            .is_some_and(|first| at.saturating_sub(first.due) > self.limits.scheduler_slack)
        {
            return self.fail(Fault::MissedDelivery);
        }
        self.delivery_at = at;
        Ok(())
    }

    /// Call only after an entire frame was successfully written. An uncertain
    /// partial write is a teardown/recovery event, never an automatic retry.
    pub fn delivered(&mut self, ticket: u64, at: Duration) -> Result<(), Fault> {
        self.advance(at)?;
        self.delivered_after_input(ticket, at)
    }
    pub(crate) fn delivered_after_input(&mut self, ticket: u64, at: Duration) -> Result<(), Fault> {
        self.delivery_clock(at)?;
        let first = self.queue.front().ok_or(Fault::WrongTicket)?;
        if first.ticket != ticket || at < first.due {
            return Err(Fault::WrongTicket);
        }
        let mut state = self.delivered.clone();
        if let Err(error) = state.apply(&first.frame, &self.device) {
            return self.fail(error);
        }
        self.delivered = state;
        self.positions.remove(&ticket);
        self.queue.pop_front();
        self.reset_epoch();
        Ok(())
    }

    /// A session handoff must not replay buffered authentication input into
    /// another session. Backend teardown is required before reinitialization.
    pub fn revoke(&mut self) {
        self.stopped = true;
    }

    pub fn recover(&mut self) -> Option<Recovery> {
        if self.recovered {
            return None;
        }
        self.recovered = true;
        self.stopped = true;
        self.group = None;
        self.queue.clear();
        self.positions.clear();
        Some(Recovery {
            source: self.device.source.clone(),
            neutral_events: self.delivered.releases(),
            destroy_virtual_and_release_grab: true,
        })
    }
}

#[cfg(test)]
mod tests {
    mod chain;
    use super::*;
    use crate::{
        input::{ABS_MT_POSITION_X, ABS_MT_POSITION_Y},
        transport::{Consumer, ConsumerEvidence, Descriptor, KernelIdentity, Readiness},
    };

    fn ms(time: u64) -> Duration {
        Duration::from_millis(time)
    }
    fn candidate(id: u64) -> Candidate {
        Candidate {
            id,
            authorization: 1,
            policy: Policy::Buffer {
                join: ms(120),
                decision: ms(300),
            },
        }
    }

    fn router(limits: Limits) -> Router {
        let device = crate::transport::tests::touch();
        router_for(device, limits)
    }

    fn router_for(device: Device, limits: Limits) -> Router {
        let descriptor = Descriptor::mirror(
            &device,
            KernelIdentity {
                bus: 3,
                vendor: 1,
                product: 2,
                version: 1,
            },
            None,
            &BTreeMap::new(),
            BTreeMap::new(),
        )
        .unwrap();
        let consumer = Consumer::IsolatedReceiver { pid: 1, start: 1 };
        let mut gate =
            Readiness::new(descriptor.clone(), consumer.clone(), Duration::ZERO).unwrap();
        gate.metadata(&descriptor, Duration::ZERO).unwrap();
        gate.consumer(
            &ConsumerEvidence {
                consumer,
                marker: descriptor.marker.clone(),
                name: descriptor.name.clone(),
                class: descriptor.class,
                seat: "seat0".into(),
                settings: BTreeMap::new(),
                at: Duration::ZERO,
            },
            Duration::ZERO,
        )
        .unwrap();
        gate.neutral(true, Duration::ZERO).unwrap();
        let ready = gate.finish(Duration::ZERO).unwrap();
        Router::new(device, &ready, 1, limits, Duration::ZERO).unwrap()
    }

    fn frame(seq: u64, at: u64, events: &[(u16, u16, i32)]) -> Frame {
        let mut events: Vec<_> = events
            .iter()
            .map(|&(kind, code, value)| RawEvent { kind, code, value })
            .collect();
        events.push(RawEvent {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        });
        Frame {
            source: crate::transport::tests::touch().source,
            at: ms(at),
            sequence: seq,
            events,
        }
    }
    fn down(seq: u64, at: u64, slot: i32, id: i32) -> Frame {
        frame(
            seq,
            at,
            &[
                (EV_ABS, ABS_MT_SLOT, slot),
                (EV_ABS, ABS_MT_TRACKING_ID, id),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_ABS, ABS_MT_POSITION_Y, 200),
            ],
        )
    }
    fn up(seq: u64, at: u64, slot: i32) -> Frame {
        frame(
            seq,
            at,
            &[
                (EV_ABS, ABS_MT_SLOT, slot),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
            ],
        )
    }
    fn deliver(router: &mut Router, at: u64) -> Option<Frame> {
        let entry = router.due(ms(at)).unwrap()?.clone();
        router.delivered(entry.ticket, ms(at)).unwrap();
        Some(entry.frame)
    }

    #[test]
    fn completed_tap_keeps_down_up_interval_and_delivers_exactly_once() {
        let mut router = router(Limits::default());
        let start = down(1, 0, 0, 10);
        let end = up(2, 40, 0);
        router
            .push(start.clone(), Some(candidate(1)), ms(0))
            .unwrap();
        router.push(end.clone(), None, ms(40)).unwrap();
        assert!(router.due(ms(40)).unwrap().is_none());
        router.cancel(1, 1, ms(40)).unwrap();
        assert_eq!(deliver(&mut router, 40), Some(start));
        assert!(deliver(&mut router, 79).is_none());
        assert_eq!(deliver(&mut router, 80), Some(end));
        assert!(deliver(&mut router, 80).is_none());
        assert_eq!(router.delivery_offset(), Duration::ZERO);
    }

    #[test]
    fn once_forwarded_late_fingers_cannot_steal_the_sequence() {
        let mut router = router(Limits::default());
        router
            .push(down(1, 0, 0, 10), Some(candidate(1)), ms(0))
            .unwrap();
        router.cancel(1, 1, ms(120)).unwrap();
        deliver(&mut router, 120).unwrap();
        router.push(down(2, 140, 1, 11), None, ms(140)).unwrap();
        assert_eq!(router.claim(1, 1, ms(140)), Err(Fault::TooLate));
        assert_eq!(deliver(&mut router, 260).unwrap().sequence, 2);
        router.push(up(3, 270, 0), None, ms(270)).unwrap();
        router.push(up(4, 290, 1), None, ms(290)).unwrap();
        assert_eq!(deliver(&mut router, 390).unwrap().sequence, 3);
        assert_eq!(deliver(&mut router, 410).unwrap().sequence, 4);
    }

    #[test]
    fn consumed_axis_and_cursor_updates_do_not_corrupt_a_delta_only_contact() {
        use crate::input::{AbsoluteSlot, AbsoluteState, Normalizer};
        let device = crate::transport::tests::touch();
        let initial = AbsoluteState {
            values: BTreeMap::from([(ABS_MT_SLOT, 0)]),
            slots: BTreeMap::from([
                (
                    0,
                    AbsoluteSlot {
                        values: BTreeMap::from([(ABS_MT_POSITION_X, 10), (ABS_MT_POSITION_Y, 20)]),
                    },
                ),
                (
                    1,
                    AbsoluteSlot {
                        values: BTreeMap::from([(ABS_MT_POSITION_X, 30), (ABS_MT_POSITION_Y, 40)]),
                    },
                ),
            ]),
        };
        let mut router = router_for(device.clone(), Limits::default());
        router.seed_absolute(&initial).unwrap();
        let mut application = Normalizer::new(&device);
        application.seed_absolute(&device, initial).unwrap();
        router
            .push(
                down(1, 0, 1, 10),
                Some(Candidate {
                    policy: Policy::Reserve,
                    ..candidate(1)
                }),
                ms(0),
            )
            .unwrap();
        router.push(up(2, 5, 1), None, ms(5)).unwrap();
        assert!(deliver(&mut router, 5).is_none());
        // The physical source still selects slot 1 and keeps (100, 200).
        // Linux may emit only a new tracking ID at that same position.
        router
            .push(
                frame(3, 10, &[(EV_ABS, ABS_MT_TRACKING_ID, 11)]),
                None,
                ms(10),
            )
            .unwrap();
        let received = deliver(&mut router, 10).unwrap();
        let first = application.process(received).unwrap();
        assert_eq!(first.contacts.len(), 1);
        assert_eq!(first.contacts[0].id.slot, 1);
        assert_eq!((first.contacts[0].x, first.contacts[0].y), (100, 200));
        router
            .push(
                frame(
                    4,
                    20,
                    &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, 12)],
                ),
                None,
                ms(20),
            )
            .unwrap();
        let second = application
            .process(deliver(&mut router, 20).unwrap())
            .unwrap();
        assert_eq!(second.contacts.len(), 2);
        assert_eq!(
            second
                .contacts
                .iter()
                .map(|contact| contact.id.slot)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([0, 1])
        );
    }

    #[test]
    fn buffered_repairs_keep_each_frames_coordinates_and_timestamp() {
        use crate::input::{AbsoluteSlot, AbsoluteState, Normalizer};
        let device = crate::transport::tests::touch();
        let initial = AbsoluteState {
            values: BTreeMap::from([(ABS_MT_SLOT, 0)]),
            slots: BTreeMap::from([
                (
                    0,
                    AbsoluteSlot {
                        values: BTreeMap::from([(ABS_MT_POSITION_X, 10), (ABS_MT_POSITION_Y, 20)]),
                    },
                ),
                (
                    1,
                    AbsoluteSlot {
                        values: BTreeMap::from([(ABS_MT_POSITION_X, 30), (ABS_MT_POSITION_Y, 40)]),
                    },
                ),
            ]),
        };
        let mut router = router_for(device.clone(), Limits::default());
        router.seed_absolute(&initial).unwrap();
        let mut application = Normalizer::new(&device);
        application.seed_absolute(&device, initial).unwrap();
        router
            .push(
                down(1, 0, 1, 10),
                Some(Candidate {
                    policy: Policy::Reserve,
                    ..candidate(1)
                }),
                ms(0),
            )
            .unwrap();
        router.push(up(2, 5, 1), None, ms(5)).unwrap();
        router
            .push(
                frame(3, 10, &[(EV_ABS, ABS_MT_TRACKING_ID, 11)]),
                Some(candidate(2)),
                ms(10),
            )
            .unwrap();
        router
            .push(
                frame(
                    4,
                    20,
                    &[
                        (EV_ABS, ABS_MT_SLOT, 0),
                        (EV_ABS, ABS_MT_TRACKING_ID, 12),
                        (EV_ABS, ABS_MT_POSITION_X, 777),
                    ],
                ),
                None,
                ms(20),
            )
            .unwrap();
        router.cancel(2, 1, ms(40)).unwrap();
        let first = deliver(&mut router, 40).unwrap();
        assert_eq!((first.sequence, first.at), (3, ms(10)));
        let first = application.process(first).unwrap();
        assert_eq!(
            (
                first.contacts[0].id.slot,
                first.contacts[0].x,
                first.contacts[0].y
            ),
            (1, 100, 200)
        );
        let second = deliver(&mut router, 50).unwrap();
        assert_eq!((second.sequence, second.at), (4, ms(20)));
        let second = application.process(second).unwrap();
        assert_eq!(second.contacts.len(), 2);
        let head = second
            .contacts
            .iter()
            .find(|contact| contact.id.slot == 0)
            .unwrap();
        assert_eq!((head.x, head.y), (777, 20));
    }

    #[test]
    fn repair_events_count_toward_the_output_storage_limit() {
        let mut router = router(Limits {
            events: 3,
            ..Limits::default()
        });
        router
            .push(
                down(1, 0, 1, 10),
                Some(Candidate {
                    policy: Policy::Reserve,
                    ..candidate(1)
                }),
                ms(0),
            )
            .unwrap();
        router.push(up(2, 5, 1), None, ms(5)).unwrap();
        assert_eq!(
            router.push(
                frame(3, 10, &[(EV_ABS, ABS_MT_TRACKING_ID, 11)]),
                None,
                ms(10)
            ),
            Err(Fault::Overflow)
        );
        assert!(router.recover().unwrap().destroy_virtual_and_release_grab);
    }

    #[test]
    fn claimed_and_cancelled_group_never_leaks_a_release_or_late_finger() {
        let mut router = router(Limits::default());
        router
            .push(down(1, 0, 0, 10), Some(candidate(1)), ms(0))
            .unwrap();
        router.claim(1, 1, ms(120)).unwrap();
        router.cancel(1, 1, ms(130)).unwrap();
        assert_eq!(router.ownership(), Some(Ownership::Draining));
        router.push(down(2, 140, 1, 11), None, ms(140)).unwrap();
        router.push(up(3, 150, 0), None, ms(150)).unwrap();
        router.push(up(4, 160, 1), None, ms(160)).unwrap();
        assert!(deliver(&mut router, 160).is_none());
        let ordinary = down(5, 170, 0, 20);
        router.push(ordinary.clone(), None, ms(170)).unwrap();
        assert_eq!(deliver(&mut router, 170), Some(ordinary));
    }

    #[test]
    fn reused_slot_inherits_epoch_offset_until_queued_release_is_delivered() {
        let mut router = router(Limits::default());
        router
            .push(down(1, 0, 0, 10), Some(candidate(1)), ms(0))
            .unwrap();
        router.push(up(2, 30, 0), None, ms(30)).unwrap();
        router.cancel(1, 1, ms(30)).unwrap();
        deliver(&mut router, 30).unwrap();
        router.push(down(3, 40, 0, 20), None, ms(40)).unwrap();
        assert_eq!(router.delivery_offset(), ms(30));
        assert!(deliver(&mut router, 59).is_none());
        assert_eq!(deliver(&mut router, 60).unwrap().sequence, 2);
        assert_eq!(deliver(&mut router, 70).unwrap().sequence, 3);
    }

    #[test]
    fn later_buffer_raises_offset_without_adding_offsets_cumulatively() {
        let mut router = router(Limits::default());
        router
            .push(down(1, 0, 0, 10), Some(candidate(1)), ms(0))
            .unwrap();
        router.cancel(1, 1, ms(120)).unwrap();
        deliver(&mut router, 120).unwrap();
        router.push(up(2, 125, 0), None, ms(125)).unwrap();
        router
            .push(down(3, 130, 0, 20), Some(candidate(2)), ms(130))
            .unwrap();
        deliver(&mut router, 245).unwrap();
        router.cancel(2, 1, ms(370)).unwrap();
        assert_eq!(router.delivery_offset(), ms(240));
        assert_eq!(deliver(&mut router, 370).unwrap().sequence, 3);
    }

    #[test]
    fn claiming_new_group_does_not_discard_older_queued_app_release() {
        let mut router = router(Limits::default());
        router
            .push(down(1, 0, 0, 10), Some(candidate(1)), ms(0))
            .unwrap();
        router.push(up(2, 20, 0), None, ms(20)).unwrap();
        router.cancel(1, 1, ms(120)).unwrap();
        deliver(&mut router, 120).unwrap();
        router
            .push(
                down(3, 130, 0, 20),
                Some(Candidate {
                    policy: Policy::Buffer {
                        join: ms(0),
                        decision: ms(300),
                    },
                    ..candidate(2)
                }),
                ms(130),
            )
            .unwrap();
        router.claim(2, 1, ms(130)).unwrap();
        assert_eq!(router.queued_frames(), 1);
        assert_eq!(deliver(&mut router, 140).unwrap().sequence, 2);
        router.push(up(4, 260, 0), None, ms(260)).unwrap();
        assert!(deliver(&mut router, 260).is_none());
    }

    #[test]
    fn max_decision_timeout_forwards_instead_of_allowing_late_claim() {
        let mut router = router(Limits::default());
        router
            .push(down(1, 0, 0, 10), Some(candidate(1)), ms(0))
            .unwrap();
        assert_eq!(router.claim(1, 1, ms(300)), Err(Fault::TooLate));
        assert_eq!(deliver(&mut router, 300).unwrap().sequence, 1);
        assert_eq!(router.delivery_offset(), ms(300));
    }

    #[test]
    fn two_devices_have_independent_delivery_epochs() {
        let mut slow = router(Limits::default());
        let mut other = router(Limits::default());
        slow.push(down(1, 0, 0, 10), Some(candidate(1)), ms(0))
            .unwrap();
        other.push(down(1, 10, 0, 20), None, ms(10)).unwrap();
        assert!(deliver(&mut slow, 10).is_none());
        assert!(deliver(&mut other, 10).is_some());
        assert_eq!(slow.ownership(), Some(Ownership::Pending));
    }

    #[test]
    fn overflow_recovers_delivered_state_explicitly_and_never_drops_just_a_release() {
        let mut router = router(Limits {
            frames: 1,
            ..Limits::default()
        });
        router.push(down(1, 0, 0, 10), None, ms(0)).unwrap();
        deliver(&mut router, 0).unwrap();
        router
            .push(
                frame(2, 10, &[(EV_ABS, ABS_MT_POSITION_Y, 300)]),
                None,
                ms(10),
            )
            .unwrap();
        assert_eq!(
            router.push(up(3, 11, 0), None, ms(11)),
            Err(Fault::Overflow)
        );
        let recovery = router.recover().unwrap();
        assert!(recovery.destroy_virtual_and_release_grab);
        assert!(
            recovery
                .neutral_events
                .iter()
                .any(|event| event.code == ABS_MT_TRACKING_ID && event.value == -1)
        );
        assert!(router.recover().is_none());
        assert_eq!(router.due(ms(11)).unwrap_err(), Fault::Stopped);
    }

    #[test]
    fn old_source_and_invalid_candidate_cannot_disrupt_a_working_group() {
        let mut router = router(Limits::default());
        router
            .push(down(1, 0, 0, 10), Some(candidate(1)), ms(0))
            .unwrap();
        let mut stale = up(2, 1000, 0);
        stale.source.generation = 0;
        assert_eq!(router.push(stale, None, ms(1000)), Err(Fault::StaleSource));
        assert_eq!(router.claim(1, 2, ms(1000)), Err(Fault::Unauthorized));
        assert_eq!(
            router.push(down(2, 10, 1, 11), Some(candidate(2)), ms(10)),
            Err(Fault::InvalidCandidate)
        );
        router.claim(1, 1, ms(120)).unwrap();
        assert_eq!(router.ownership(), Some(Ownership::Claimed));
    }

    #[test]
    fn acknowledgement_cannot_release_unwritten_frames_or_retry_old_tickets() {
        let mut router = router(Limits::default());
        router
            .push(down(1, 0, 0, 10), Some(candidate(1)), ms(0))
            .unwrap();
        router.cancel(1, 1, ms(120)).unwrap();
        assert_eq!(router.delivered(99, ms(120)), Err(Fault::WrongTicket));
        assert_eq!(router.queued_frames(), 1);
        deliver(&mut router, 120).unwrap();
        assert_eq!(router.delivered(1, ms(120)), Err(Fault::WrongTicket));
    }

    #[test]
    fn stalled_scheduler_and_session_handoff_require_teardown_not_password_replay() {
        let mut router = router(Limits::default());
        router
            .push(down(1, 0, 0, 10), Some(candidate(1)), ms(0))
            .unwrap();
        router.push(up(2, 20, 0), None, ms(20)).unwrap();
        router.cancel(1, 1, ms(120)).unwrap();
        assert_eq!(router.due(ms(200)).unwrap_err(), Fault::MissedDelivery);
        assert!(router.recover().unwrap().neutral_events.is_empty());
        let mut router = self::router(Limits::default());
        router
            .push(down(1, 0, 0, 10), Some(candidate(1)), ms(0))
            .unwrap();
        router.revoke();
        assert_eq!(router.due(ms(120)).unwrap_err(), Fault::Stopped);
        assert!(router.recover().unwrap().destroy_virtual_and_release_grab);
        assert_eq!(router.queued_frames(), 0);
    }

    #[test]
    fn keyboard_repeat_and_mouse_wheels_pass_through_as_original_frames() {
        let mut device = crate::transport::tests::touch();
        device.axes.clear();
        device.properties.clear();
        device.capabilities = BTreeMap::from([(0, vec![0]), (1, vec![30, 125])]);
        let mut keyboard = router_for(device.clone(), Limits::default());
        for (seq, at, events) in [
            (1, 0, vec![(EV_KEY, 125, 1), (EV_KEY, 30, 1)]),
            (2, 10, vec![(EV_KEY, 30, 2)]),
            (3, 20, vec![(EV_KEY, 30, 0)]),
            (4, 30, vec![(EV_KEY, 125, 0)]),
        ] {
            let input = frame(seq, at, &events);
            keyboard.push(input.clone(), None, ms(at)).unwrap();
            assert_eq!(deliver(&mut keyboard, at), Some(input));
        }
        assert_eq!(keyboard.delivery_offset(), Duration::ZERO);
        device.capabilities =
            BTreeMap::from([(0, vec![0]), (1, vec![0x110]), (2, vec![0, 1, 8, 11])]);
        let mut mouse = router_for(device, Limits::default());
        let input = frame(1, 0, &[(2, 0, 10), (2, 1, -20), (2, 8, 1), (2, 11, 120)]);
        mouse.push(input.clone(), None, ms(0)).unwrap();
        assert_eq!(deliver(&mut mouse, 0), Some(input));
    }

    #[test]
    fn reserve_and_observe_have_distinct_delivery_and_recovery_behavior() {
        let mut reserved = router(Limits::default());
        reserved
            .push(
                down(1, 0, 0, 10),
                Some(Candidate {
                    policy: Policy::Reserve,
                    ..candidate(1)
                }),
                ms(0),
            )
            .unwrap();
        reserved.push(up(2, 10, 0), None, ms(10)).unwrap();
        assert!(deliver(&mut reserved, 10).is_none());
        let mut observed = router(Limits::default());
        let first = down(1, 0, 0, 10);
        observed
            .push(
                first.clone(),
                Some(Candidate {
                    policy: Policy::Observe,
                    ..candidate(1)
                }),
                ms(0),
            )
            .unwrap();
        assert_eq!(deliver(&mut observed, 0), Some(first));
        assert_eq!(observed.claim(1, 1, ms(0)), Err(Fault::TooLate));
        let recovery = observed.recover().unwrap();
        assert!(
            recovery
                .neutral_events
                .iter()
                .any(|event| event.code == ABS_MT_TRACKING_ID && event.value == -1)
        );
        assert_eq!(recovery.neutral_events.last().unwrap().kind, EV_SYN);
    }

    #[test]
    fn nonexistent_start_cannot_advance_or_commit_an_earlier_pending_tap() {
        let mut router = router(Limits::default());
        router
            .push(down(1, 0, 0, 10), Some(candidate(1)), ms(0))
            .unwrap();
        router.push(up(2, 20, 0), None, ms(20)).unwrap();
        assert_eq!(
            router.push(frame(3, 30, &[]), Some(candidate(2)), ms(30)),
            Err(Fault::InvalidCandidate)
        );
        assert_eq!(router.ownership(), Some(Ownership::Pending));
        assert_eq!(router.next_deadline(), Some(ms(300)));
        router.cancel(1, 1, ms(20)).unwrap();
        assert_eq!(deliver(&mut router, 20).unwrap().sequence, 1);
        assert_eq!(deliver(&mut router, 40).unwrap().sequence, 2);
    }
}
