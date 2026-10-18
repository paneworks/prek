use super::*;

pub(super) struct Prefix {
    key_presses: presses::Set,
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
            && self.admits_input(candidate)
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
}
