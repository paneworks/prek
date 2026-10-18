use super::*;

pub(super) struct Edge<'a> {
    pub(super) state: &'a DeviceState,
    pub(super) authority: &'a Authority,
    pub(super) conditions: &'a Keys,
    pub(super) config: &'a Config,
    pub(super) at: Duration,
    pub(super) position: InputPosition,
    pub(super) code: u16,
    pub(super) key_state: KeyState,
    pub(super) generation: u64,
    pub(super) next_id: &'a mut u64,
    pub(super) events: &'a mut Vec<Event>,
}
impl Edge<'_> {
    fn start(&mut self, worker: &mut Worker) {
        start_candidate(
            worker,
            self.state,
            self.authority,
            self.conditions,
            (self.at, &[]),
            self.next_id,
            self.events,
        );
    }
    fn record(&mut self, worker: &mut Worker, owner: &Source) -> bool {
        presses::record(
            worker,
            owner,
            presses::Press {
                source: self.state.device.source.clone(),
                code: self.code,
                position: self.position,
            },
            self.at,
            self.events,
        )
    }
    fn report(&mut self, worker: &mut Worker, signals: Vec<Signal>) {
        report_signals(
            worker,
            signals,
            self.generation,
            self.at,
            InputPosition::after_frame(self.position.frame),
            self.events,
        );
    }
}

pub(super) fn process(worker: &mut Worker, mut edge: Edge<'_>) -> io::Result<()> {
    if edge.key_state == KeyState::Repeat {
        return Ok(());
    }
    match &worker.rule.trigger {
        Trigger::ButtonTaps { key, .. } | Trigger::ButtonHold { key, .. } if key.0 == edge.code => {
        }
        Trigger::Chord { keys, .. } if keys.iter().any(|key| key.0 == edge.code) => {
            return chord(worker, edge);
        }
        _ => return Ok(()),
    }
    let source = &edge.state.device.source;
    if worker.trace
        && edge.key_state == KeyState::Release
        && matches!(worker.rule.trigger, Trigger::ButtonTaps { .. })
    {
        worker.released.insert(source.clone(), edge.at);
    }
    if edge.key_state == KeyState::Press && !worker.candidates.contains_key(source) {
        edge.start(worker);
    }
    if let Some(candidate) = worker.candidates.get_mut(source) {
        if edge.key_state == KeyState::Press {
            candidate
                .source_inputs
                .insert(source.clone(), edge.state.input_watermark);
        } else if let Recognizer::Taps(buttons) = &worker.recognizer
            && buttons.is_tap_release(
                Button {
                    device: source.device,
                    code: edge.code,
                },
                edge.at,
            )
        {
            candidate.key_tap_end = Some(edge.position);
        }
        candidate.key_edge(edge.position, edge.key_state);
    }
    if edge.key_state == KeyState::Press && !edge.record(worker, source) {
        return Ok(());
    }
    let signals = match &mut worker.recognizer {
        Recognizer::Taps(buttons) if worker.candidates.contains_key(source) => buttons
            .update(
                Button {
                    device: source.device,
                    code: edge.code,
                },
                edge.key_state == KeyState::Press,
                edge.at,
            )
            .into_iter()
            .map(Signal::Button)
            .collect(),
        Recognizer::Taps(_) => Vec::new(),
        Recognizer::Keys(keys) if worker.candidates.contains_key(source) => keys
            .update(source, edge.code, edge.key_state, edge.at)
            .map_err(invalid)?
            .into_iter()
            .map(Signal::Key)
            .collect(),
        Recognizer::Keys(keys) => {
            keys.reconcile(source, edge.state.keys.iter().copied())
                .map_err(invalid)?;
            Vec::new()
        }
        _ => unreachable!(),
    };
    edge.report(worker, signals);
    if edge.key_state == KeyState::Release {
        if matches!(worker.rule.trigger, Trigger::ButtonTaps { .. }) {
            if let Some(candidate) = worker.candidates.get_mut(source) {
                candidate.expires = Some(
                    edge.at
                        .saturating_add(edge.config.timing.tap_gap.duration()),
                );
            }
        } else {
            worker.candidates.remove(source);
        }
    } else if let Some(candidate) = worker.candidates.get_mut(source) {
        candidate.expires = None;
    }
    Ok(())
}

fn chord(worker: &mut Worker, mut edge: Edge<'_>) -> io::Result<()> {
    let source = &edge.state.device.source;
    if edge.key_state == KeyState::Press
        && !worker
            .candidates
            .keys()
            .any(|owner| owner.seat == source.seat)
    {
        edge.start(worker);
        if let Some(candidate) = worker.candidates.get_mut(source)
            && let Trigger::Chord { timeout, .. } = worker.rule.trigger
        {
            candidate.expires = Some(edge.at.saturating_add(timeout.duration()));
        }
    }
    for candidate in worker
        .candidates
        .values_mut()
        .filter(|candidate| candidate.source.seat == source.seat)
    {
        candidate.key_edge(edge.position, edge.key_state);
        if edge.key_state == KeyState::Press {
            candidate.sources.insert(source.clone());
            candidate
                .source_inputs
                .insert(source.clone(), edge.state.input_watermark);
        }
    }
    if edge.key_state == KeyState::Press {
        let owners: Vec<_> = worker
            .candidates
            .keys()
            .filter(|owner| owner.seat == source.seat)
            .cloned()
            .collect();
        for owner in owners {
            edge.record(worker, &owner);
        }
    }
    let Recognizer::Keys(keys) = &mut worker.recognizer else {
        unreachable!()
    };
    if !worker
        .candidates
        .keys()
        .any(|owner| owner.seat == source.seat)
    {
        keys.reconcile(source, edge.state.keys.iter().copied())
            .map_err(invalid)?;
        return Ok(());
    }
    let signals = keys
        .update(source, edge.code, edge.key_state, edge.at)
        .map_err(invalid)?
        .into_iter()
        .map(Signal::Key)
        .collect();
    edge.report(worker, signals);
    let Recognizer::Keys(keys) = &worker.recognizer else {
        unreachable!()
    };
    let Trigger::Chord { keys: required, .. } = &worker.rule.trigger else {
        unreachable!()
    };
    if required.iter().all(|key| !keys.down(&source.seat, key.0)) {
        worker
            .candidates
            .retain(|owner, _| owner.seat != source.seat);
    }
    Ok(())
}
