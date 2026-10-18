use super::*;

impl Engine {
    pub(in crate::rules) fn key_sequences_begin(
        &mut self,
        frame: &NormalizedFrame,
    ) -> io::Result<()> {
        let context = SequenceContext {
            devices: &self.devices,
            authority: &self.authority,
            keys: &self.conditions,
            generation: self.generation,
            at: frame.raw.at,
            admitted_at: frame.raw.at,
            input_watermark: self.input_watermark,
            normalized: None,
        };
        for sequence in self
            .sequences
            .iter_mut()
            .filter(|sequence| sequence.key_timeline)
        {
            for seat in std::mem::take(&mut sequence.reset) {
                sequence.reset_seat(&seat, &context)?;
            }
            for engine in &mut sequence.steps {
                engine.authority = self.authority.clone();
                engine.input_watermark = self.input_watermark;
                engine.replay.push(Record::Frame {
                    frame: frame.raw.clone(),
                })?;
                let observations = engine.replay.take_observations();
                if observations.iter().filter(|observation| matches!(observation, Observation::Input { .. })).count() != 1
                    || !observations.iter().any(|observation| matches!(observation, Observation::Input { frame: normalized } if normalized == frame)) {
                    return Err(invalid("sequence normalization differs from original input"));
                }
                engine.advance(
                    frame.raw.at,
                    self.input_watermark
                        .checked_sub(1)
                        .ok_or_else(|| invalid("unpositioned sequence input"))?,
                )?;
                let state = engine
                    .devices
                    .get_mut(&frame.raw.source.device)
                    .ok_or_else(|| invalid("unregistered sequence source"))?;
                state.input_watermark = self.input_watermark;
                state.last_input = Some((frame.raw.sequence, frame.raw.at));
                state.contacts = frame.contacts.clone();
            }
            sequence.collect(
                None,
                &context,
                &mut self.next_id,
                &mut self.events,
                &mut self.custom,
            )?;
        }
        Ok(())
    }

    pub(in crate::rules) fn key_sequences_edge(
        &mut self,
        frame: &NormalizedFrame,
        edge: (usize, u16, KeyState),
    ) -> io::Result<()> {
        let context = SequenceContext {
            devices: &self.devices,
            authority: &self.authority,
            keys: &self.conditions,
            generation: self.generation,
            at: frame.raw.at,
            admitted_at: frame.raw.at,
            input_watermark: self.input_watermark,
            normalized: None,
        };
        for sequence in self
            .sequences
            .iter_mut()
            .filter(|sequence| sequence.key_timeline)
        {
            for engine in &mut sequence.steps {
                let allowed = policy_selected(
                    &engine.config,
                    &engine.devices[&frame.raw.source.device].device,
                );
                engine.key_edges(frame, &[edge], allowed)?;
            }
            sequence.collect(
                None,
                &context,
                &mut self.next_id,
                &mut self.events,
                &mut self.custom,
            )?;
            if self.events.len() > 16384 {
                return Err(invalid("rule observation queue limit exceeded"));
            }
        }
        Ok(())
    }
}
