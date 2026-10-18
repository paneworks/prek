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
            key_edge: None,
            finish_report: false,
        };
        for sequence in self
            .sequences
            .iter_mut()
            .filter(|sequence| sequence.key_timeline)
        {
            if sequence.has_custom && self.custom.is_none() {
                return Err(invalid("custom sequence coordinator is unavailable"));
            }
            let after_frame = self
                .input_watermark
                .checked_sub(1)
                .ok_or_else(|| invalid("unpositioned sequence input"))?;
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
                let custom = key_only_custom(&engine.workers[0].rule.trigger);
                if custom {
                    engine.next_id = self.next_id;
                    std::mem::swap(&mut engine.custom, &mut self.custom);
                }
                let result = engine.advance(frame.raw.at, after_frame);
                if custom {
                    std::mem::swap(&mut engine.custom, &mut self.custom);
                    self.next_id = engine.next_id;
                }
                result?;
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
        finish_report: bool,
    ) -> io::Result<()> {
        let context = SequenceContext {
            devices: &self.devices,
            authority: &self.authority,
            keys: &self.conditions,
            generation: self.generation,
            at: frame.raw.at,
            admitted_at: frame.raw.at,
            input_watermark: self.input_watermark,
            normalized: Some(frame),
            key_edge: Some(edge),
            finish_report,
        };
        for sequence in self
            .sequences
            .iter_mut()
            .filter(|sequence| sequence.key_timeline)
        {
            if sequence.has_custom && self.custom.is_none() {
                return Err(invalid("custom sequence coordinator is unavailable"));
            }
            for (index, engine) in sequence.steps.iter_mut().enumerate() {
                let allowed = policy_selected(
                    &engine.config,
                    &engine.devices[&frame.raw.source.device].device,
                );
                let custom = key_only_custom(&engine.workers[0].rule.trigger);
                if custom {
                    engine.custom_seats = self
                        .devices
                        .values()
                        .map(|state| &state.device.source.seat)
                        .filter(|seat| {
                            sequence
                                .pending
                                .get(*seat)
                                .map_or(index == 0, |pending| pending.step == index)
                        })
                        .cloned()
                        .collect();
                    engine.next_id = self.next_id;
                    std::mem::swap(&mut engine.custom, &mut self.custom);
                }
                let result = engine.key_edges(frame, &[edge], allowed, finish_report);
                if custom {
                    std::mem::swap(&mut engine.custom, &mut self.custom);
                    self.next_id = engine.next_id;
                }
                result?;
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
