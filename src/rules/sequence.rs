use super::*;

mod keys;

pub(super) const MAX_DEFERRED: usize = 1024;
pub(super) struct Retention {
    pub(super) parent: u64,
    _charge: presses::Reservation,
}
impl StepMatch {
    pub(super) fn can_wait_for(&self, first: &Candidate, trigger: &Trigger) -> bool {
        edges::is_key_trigger(trigger)
            && self.candidate.key_start.is_some()
            && self.candidate.started >= first.started
            && first
                .key_start
                .zip(self.candidate.key_start)
                .is_none_or(|(first, later)| later > first)
            && matches!(&self.event, Event::Matched { stage, evidence, .. }
                if !matches!(evidence, Evidence::Hold { repeat: true })
                    && (*stage == Stage::Discrete || *stage == Stage::Begin && matches!(trigger, Trigger::Key { .. })))
    }
    pub(super) fn retain_for(&mut self, parent: u64) -> Result<(), &'static str> {
        if self.retention.is_some() {
            return Ok(());
        }
        let candidate = &self.candidate;
        let event_bytes = crate::recognition::encoded_size(&self.event, presses::MAX_BYTES / 16)
            .map_err(|_| "sequence deferred completion exceeds byte limit")?;
        // Count owned event allocations conservatively; shared press nodes retain their existing charge.
        let bytes = 4 * std::mem::size_of::<Self>()
            + 256
            + event_bytes * 16
            + candidate.source.seat.capacity()
            + candidate
                .permit
                .as_ref()
                .map_or(0, Permit::retained_heap_bytes)
            + candidate.contacts.len() * 256
            + candidate
                .sources
                .iter()
                .map(|source| source.seat.capacity() + 256)
                .sum::<usize>()
            + candidate
                .source_inputs
                .keys()
                .map(|source| source.seat.capacity() + 256)
                .sum::<usize>();
        self.retention = Some(Retention {
            parent,
            _charge: candidate.key_presses.reserve(bytes)?,
        });
        Ok(())
    }
    pub(super) fn retainable(
        &self,
        context: &SequenceContext<'_>,
        record: Option<&Record>,
    ) -> bool {
        !self.candidate.cancelled.load(std::sync::atomic::Ordering::Acquire)
            && self.candidate.session_generation == context.authority.generation()
            && self.expires.into_iter().chain(self.candidate.expires).all(|expires| context.admitted_at < expires)
            && self.candidate.sources.iter().all(|source| {
                context.devices.get(&source.device).is_some_and(|state| state.device.source == *source)
                    && !matches!(record, Some(Record::Mapping { source: changed, .. } | Record::Reconcile { source: changed, .. }) if changed == source)
            })
            && context.devices.get(&self.candidate.source.device).is_some_and(|state| {
                self.candidate.permit.as_ref().is_none_or(|permit| context.authority.validate(permit, input(state, context.admitted_at)).is_ok())
            })
    }
}

pub(super) struct Prefix {
    pub(in crate::rules) key_presses: presses::Set,
    sources: BTreeSet<Source>,
    source_inputs: BTreeMap<Source, u64>,
}
impl Prefix {
    pub(super) fn new(candidate: &Candidate) -> Self {
        Self {
            key_presses: candidate.key_presses.empty_like(),
            sources: BTreeSet::from([candidate.source.clone()]),
            source_inputs: BTreeMap::from([(candidate.source.clone(), candidate.started_input)]),
        }
    }
    fn merge(
        presses: &mut presses::Set,
        sources: &mut BTreeSet<Source>,
        inputs: &mut BTreeMap<Source, u64>,
        candidate: &Candidate,
    ) -> Result<(), &'static str> {
        presses.merge(&candidate.key_presses)?;
        sources.extend(candidate.sources.iter().cloned());
        for (source, &watermark) in &candidate.source_inputs {
            inputs
                .entry(source.clone())
                .and_modify(|old| *old = (*old).max(watermark))
                .or_insert(watermark);
        }
        Ok(())
    }
    fn include(&mut self, candidate: &Candidate) -> Result<(), &'static str> {
        Self::merge(
            &mut self.key_presses,
            &mut self.sources,
            &mut self.source_inputs,
            candidate,
        )
    }
}
impl PendingSequence {
    pub(super) fn reset_projection(&mut self) {
        self.key_presses = self.completed.key_presses.clone();
        self.sources = self.completed.sources.clone();
        self.source_inputs = self.completed.source_inputs.clone();
    }
    pub(super) fn observe(&mut self, candidate: &Candidate) -> Result<(), &'static str> {
        Prefix::merge(
            &mut self.key_presses,
            &mut self.sources,
            &mut self.source_inputs,
            candidate,
        )
    }
    pub(super) fn complete(&mut self, candidate: &Candidate) -> Result<(), &'static str> {
        self.completed.include(candidate)?;
        self.reset_projection();
        Ok(())
    }
    pub(super) fn accepts(&self, candidate: &Candidate, triggers: &[Trigger]) -> bool {
        candidate.source.seat == self.first.source.seat
            && self.admits_step(candidate, triggers)
            && if self.step == 0 {
                candidate.id == self.first.id && candidate.source == self.first.source
            } else {
                candidate.started >= self.finished
                    || self.previous.as_ref().is_some_and(|previous| {
                        same_contact_continuation(
                            &triggers[self.step - 1],
                            &triggers[self.step],
                            previous,
                            candidate,
                        )
                    })
            }
    }
    pub(super) fn admits_step(&self, candidate: &Candidate, triggers: &[Trigger]) -> bool {
        self.admits_input(candidate)
            && self
                .after_key
                .zip(candidate.key_start)
                .is_none_or(|(after, start)| {
                    start > after
                        || start == after
                            && self.step > 0
                            && (matches!(triggers[self.step], Trigger::ButtonHold { .. })
                                && matches!(
                                    triggers[self.step - 1],
                                    Trigger::Key { .. } | Trigger::Chord { .. }
                                )
                                || matches!(triggers[self.step], Trigger::Custom { .. })
                                    && self.continues_previous(candidate))
                })
    }
}
