use super::*;
use crate::input::EV_KEY;

mod timed;

pub(super) fn is_key_trigger(trigger: &Trigger) -> bool {
    matches!(
        trigger,
        Trigger::Key { .. }
            | Trigger::ButtonTaps { .. }
            | Trigger::ButtonHold { .. }
            | Trigger::Chord { .. }
    )
}

pub(super) fn positioned_keys(frame: &NormalizedFrame) -> io::Result<Vec<(usize, u16, KeyState)>> {
    let mut next = 0;
    let mut result = Vec::new();
    for change in &frame.changes {
        let Change::Key { code, state } = *change else {
            continue;
        };
        let value = match state {
            KeyState::Release => 0,
            KeyState::Press => 1,
            KeyState::Repeat => 2,
        };
        let offset = frame.raw.events[next..]
            .iter()
            .position(|event| event.kind == EV_KEY && event.code == code && event.value == value)
            .ok_or_else(|| invalid("normalized key edge has no ordered original event"))?;
        let position = next + offset;
        next = position + 1;
        result.push((position, code, state));
    }
    Ok(result)
}

impl Engine {
    pub(super) fn key_edges(
        &mut self,
        frame: &NormalizedFrame,
        edges: &[(usize, u16, KeyState)],
        allowed: bool,
    ) -> io::Result<()> {
        let source = &frame.raw.source;
        let at = frame.raw.at;
        for &(event, code, key_state) in edges {
            if allowed {
                self.conditions
                    .update(source, code, key_state, at)
                    .map_err(invalid)?;
            }
            let state = self
                .devices
                .get_mut(&source.device)
                .ok_or_else(|| invalid("unregistered key source"))?;
            match key_state {
                KeyState::Press => {
                    state.keys.insert(code);
                    state.key_origins.insert(
                        code,
                        InputPosition {
                            frame: self.input_watermark,
                            event,
                        },
                    );
                }
                KeyState::Release => {
                    state.keys.remove(&code);
                    state.key_origins.remove(&code);
                }
                KeyState::Repeat => {}
            }
            for sequence in &mut self.sequences {
                if sequence.rule.keys_throughout
                    && !conditions(&sequence.rule, &self.conditions, &source.seat)
                {
                    sequence.condition_loss = Some((
                        source.seat.clone(),
                        InputPosition {
                            frame: self.input_watermark,
                            event,
                        },
                    ));
                    sequence.cancel(
                        &source.seat,
                        at,
                        "activation condition released",
                        &mut self.events,
                    );
                }
            }
            for worker in &mut self.workers {
                let custom_key = key_only_custom(&worker.rule.trigger);
                if !is_key_trigger(&worker.rule.trigger) && !custom_key {
                    continue;
                }
                let finish = matches!(worker.rule.trigger, Trigger::Key { key } if key.0 == code && key_state == KeyState::Release).then_some((source, code));
                validate_candidates_except(
                    worker,
                    &self.authority,
                    &self.devices,
                    &self.conditions,
                    at,
                    &mut self.events,
                    finish,
                );
                let state = &self.devices[&source.device];
                if !selected(&self.config, &worker.rule, &state.device) {
                    continue;
                }
                if custom_key {
                    if self.coordinated_step && !self.custom_seats.contains(&source.seat) {
                        continue;
                    }
                    custom::key_frame(
                        worker,
                        state,
                        frame,
                        (event, code, key_state),
                        edges.last().is_some_and(|edge| edge.0 == event),
                        custom::FrameContext {
                            config: &self.config,
                            authority: &self.authority,
                            keys: &self.conditions,
                            custom: self.custom.as_mut().unwrap(),
                            next_id: &mut self.next_id,
                            events: &mut self.events,
                        },
                    );
                    continue;
                }
                let Trigger::Key { key } = worker.rule.trigger else {
                    timed::process(
                        worker,
                        timed::Edge {
                            state,
                            devices: &self.devices,
                            authority: &self.authority,
                            conditions: &self.conditions,
                            config: &self.config,
                            at,
                            position: InputPosition {
                                frame: self.input_watermark,
                                event,
                            },
                            code,
                            key_state,
                            generation: self.generation,
                            next_id: &mut self.next_id,
                            events: &mut self.events,
                        },
                    )?;
                    continue;
                };
                if key.0 != code || key_state == KeyState::Repeat {
                    continue;
                }
                if key_state == KeyState::Press && !worker.candidates.contains_key(source) {
                    start_candidate(
                        worker,
                        state,
                        &self.authority,
                        &self.conditions,
                        (at, &[]),
                        &mut self.next_id,
                        &mut self.events,
                    );
                }
                if key_state == KeyState::Press
                    && let Some(candidate) = worker.candidates.get_mut(source)
                {
                    candidate
                        .source_inputs
                        .insert(source.clone(), state.input_watermark);
                }
                if let Some(candidate) = worker.candidates.get_mut(source) {
                    candidate.key_edge(
                        InputPosition {
                            frame: self.input_watermark,
                            event,
                        },
                        key_state,
                    );
                }
                if key_state == KeyState::Press
                    && !presses::record(
                        worker,
                        source,
                        presses::Press {
                            source: source.clone(),
                            code,
                            position: InputPosition {
                                frame: self.input_watermark,
                                event,
                            },
                        },
                        at,
                        &mut self.events,
                    )
                {
                    continue;
                }
                report_signals(
                    worker,
                    vec![Signal::Edge {
                        source: source.clone(),
                        down: key_state == KeyState::Press,
                    }],
                    self.generation,
                    at,
                    InputPosition {
                        frame: self.input_watermark,
                        event,
                    },
                    &mut self.events,
                );
                if key_state == KeyState::Release {
                    worker.candidates.remove(source);
                } else if let Some(candidate) = worker.candidates.get_mut(source) {
                    candidate.expires = None;
                }
            }
            self.key_sequences_edge(frame, (event, code, key_state))?;
            self.custom_sync(at, crate::recognition::Retirement::Cancelled);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
