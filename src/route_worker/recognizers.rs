//! Private callback and rule commands ordered behind the sole reader watermark.
use super::*;
use crate::{
    input_events::{Event as Input, ResetReason},
    protocol::Capability,
};

fn permits_input(config: &Config, subscription: &Subscription, now: Duration) -> bool {
    crate::input_events::permitted_subscription(
        config,
        &subscription.capabilities,
        subscription.context.as_deref(),
        &subscription.seat,
        subscription.fence.session,
        now,
    )
}
fn permits_recognition(config: &Config, subscription: &Subscription, now: Duration) -> bool {
    crate::recognition::permitted_subscription(
        config,
        &subscription.capabilities,
        subscription.context.as_deref(),
        &subscription.seat,
        subscription.fence.session,
        now,
    )
}
fn error_text(error: io::Error) -> String {
    error
        .to_string()
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect()
}

impl Stream {
    fn input_states(&self, seat: &str, now: Duration) -> Vec<Input> {
        let mut states: BTreeMap<_, _> = self
            .baseline
            .input_states(now)
            .into_iter()
            .filter(|input| input.source().seat == seat)
            .map(|input| (input.source().clone(), input))
            .collect();
        if let Some(owner) = self.seats.get(seat) {
            states.extend(
                owner
                    .routing
                    .input_states(now)
                    .into_iter()
                    .map(|input| (input.source().clone(), input)),
            );
        }
        states.into_values().collect()
    }
    pub(super) fn report_input(
        &mut self,
        seat: &str,
        mut input: Input,
        now: Duration,
    ) -> io::Result<()> {
        let Some(owner) = self.seats.get(seat) else {
            return Ok(());
        };
        if let Input::Reset { at, .. } = &mut input {
            *at = (*at).max(if owner.input_enabled {
                owner.input_started
            } else {
                owner.metadata_started
            });
        }
        let subscription = owner.subscription.identity();
        let ordinary = owner.input_enabled
            && input.at() >= owner.input_started
            && permits_input(&self.config, &owner.subscription, now);
        if owner.metadata_enabled
            && matches!(input, Input::Reset { .. })
            && permits_recognition(&self.config, &owner.subscription, now)
            && let Some(metadata) = crate::recognition::Metadata::project(
                input.clone(),
                &crate::recognition::metadata_selectors(&self.config),
            )?
        {
            self.queue(Message::Outcome {
                report: Report::RecognizerMetadata {
                    subscription: subscription.clone(),
                    metadata: Box::new(metadata),
                },
            })?;
        }
        if ordinary {
            self.queue(Message::Outcome {
                report: Report::Input {
                    subscription,
                    input: Box::new(input),
                },
            })?;
        }
        Ok(())
    }
    pub(super) fn sync_seat(&mut self, name: &str, now: Duration) -> io::Result<()> {
        let Some(seat) = self.seats.get_mut(name) else {
            return Ok(());
        };
        let ready = permits_recognition(&self.config, &seat.subscription, now);
        let ordinary = permits_input(&self.config, &seat.subscription, now);
        let metadata = ready && !ordinary;
        let initial = ordinary && !seat.input_enabled || metadata && !seat.metadata_enabled;
        let changed = seat.recognizer_ready != Some(ready)
            && (seat.recognizer_ready.is_some()
                || seat
                    .subscription
                    .capabilities
                    .contains(&Capability::CustomRecognizers));
        if changed {
            seat.routing.recognition_ready(ready, now)?;
            seat.recognizer_ready = Some(ready);
        }
        if ordinary && !seat.input_enabled {
            seat.input_started = now;
        }
        if metadata && !seat.metadata_enabled {
            seat.metadata_started = now;
        }
        seat.input_enabled = ordinary;
        seat.metadata_enabled = metadata;
        let subscription = seat.subscription.identity();
        if changed {
            self.queue(Message::Outcome {
                report: Report::RecognizerReady {
                    subscription,
                    ready,
                },
            })?;
        }
        if initial {
            for input in self.input_states(name, now) {
                self.report_input(name, input, now)?;
            }
        }
        Ok(())
    }
    pub(super) fn enqueue(&mut self, command: Command, now: Duration) -> io::Result<()> {
        let bytes = crate::recognition::encoded_size(&command, protocol::MAX_QUEUED_BYTES)?;
        if self.pending.len() + self.messages.messages.len() >= protocol::MAX_QUEUED_PACKETS
            || self.pending.len() >= crate::recognition::MAX_PENDING
            || self.pending_bytes + self.messages.bytes + bytes > protocol::MAX_QUEUED_BYTES
        {
            return Err(invalid("routing private command budget exhausted"));
        }
        let (seat, fence, operation) = match command {
            Command::Recognition {
                seat,
                fence,
                proposal,
            } => {
                if proposal.fence != fence || proposal.ticket == 0 {
                    return Err(invalid("invalid private recognizer proposal"));
                }
                crate::recognition::validate_data(&proposal.data)?;
                (seat, fence, Operation::Recognition(proposal))
            }
            Command::Rules {
                seat,
                fence,
                id,
                transaction,
            } => {
                if id == 0 {
                    return Err(invalid("invalid private rule identity"));
                }
                transaction.validate().map_err(io::Error::other)?;
                (seat, fence, Operation::Rules { id, transaction })
            }
            _ => return Err(invalid("not a queued routing command")),
        };
        fence.validate()?;
        if seat.is_empty() || seat.len() > 256 || seat.chars().any(char::is_control) {
            return Err(invalid("invalid private seat identity"));
        }
        self.pending.push_back(Pending {
            seat,
            fence,
            operation,
            received_at: now,
            bytes,
        });
        self.pending_bytes += bytes;
        Ok(())
    }
    pub(super) fn apply_pending(&mut self, at: Duration, now: Duration) -> io::Result<()> {
        while self
            .pending
            .front()
            .is_some_and(|pending| pending.received_at <= at)
        {
            let pending = self.pending.pop_front().unwrap();
            self.pending_bytes -= pending.bytes;
            let result = (|| {
                if now.saturating_sub(pending.received_at) >= crate::recognition::MAX_REPLY {
                    return Err(invalid(
                        "private routing command expired before input drain",
                    ));
                }
                self.sync_seat(&pending.seat, now)?;
                let seat = self
                    .seats
                    .get_mut(&pending.seat)
                    .filter(|seat| {
                        seat.subscription.fence == pending.fence
                            && now < seat.expires
                            && seat.routing.authority().state(now) == crate::context::State::Desktop
                    })
                    .ok_or_else(|| invalid("private command requires the current owning seat"))?;
                match &pending.operation {
                    Operation::Recognition(proposal) => {
                        if seat.recognizer_ready != Some(true) {
                            return Err(invalid("recognizer helper is unavailable"));
                        }
                        seat.routing
                            .recognition_response_after_input(proposal.clone(), at, now)
                    }
                    Operation::Rules { transaction, .. } => {
                        if !self.config.policy.allow_runtime_rules
                            || !seat
                                .subscription
                                .capabilities
                                .contains(&Capability::RuntimeRules)
                        {
                            return Err(invalid("runtime rules are unavailable"));
                        }
                        seat.routing.transact_rules(transaction, at)?;
                        let owner = seat.subscription.identity();
                        self.messages.retain(|message| !matches!(message,
                            Message::Outcome { report: Report::Observation { subscription, .. }
                                | Report::Dispatch { subscription, .. } | Report::Recognition { subscription, .. } }
                            if owner.same_registration(subscription)));
                        Ok(())
                    }
                }
            })();
            let message = match pending.operation {
                Operation::Recognition(proposal) => Message::RecognitionResult {
                    seat: pending.seat.clone(),
                    fence: pending.fence,
                    ticket: proposal.ticket,
                    error: result.err().map(error_text),
                },
                Operation::Rules { id, .. } => Message::RulesChanged {
                    seat: pending.seat.clone(),
                    fence: pending.fence,
                    id,
                    revision: self
                        .seats
                        .get(&pending.seat)
                        .map_or(0, |seat| seat.routing.rule_revision()),
                    error: result.err().map(error_text),
                },
            };
            self.collect(&pending.seat, now, Some(at))?;
            self.queue(message)?;
        }
        Ok(())
    }
    pub(super) fn input_reports(
        &mut self,
        record: &Record,
        baseline: Vec<Input>,
        now: Duration,
    ) -> io::Result<()> {
        let names: Vec<_> = self.seats.keys().cloned().collect();
        for name in names {
            let seat = &mut self.seats.get_mut(&name).unwrap();
            let at = match record {
                Record::Frame { frame } => frame.at,
                Record::Tick { at }
                | Record::Remove { at, .. }
                | Record::Reconcile { at, .. }
                | Record::Mapping { at, .. } => *at,
                _ => now,
            };
            if at < seat.started && matches!(record, Record::Frame { .. } | Record::Tick { .. }) {
                continue;
            }
            let source = match record {
                Record::Frame { frame } => Some(&frame.source),
                Record::Remove { source, .. }
                | Record::Reconcile { source, .. }
                | Record::Mapping { source, .. } => Some(source),
                _ => None,
            };
            if source.is_some_and(|source| source.seat != name) {
                continue;
            }
            let owned = source.is_some_and(|source| {
                self.grabbed.contains(source) && seat.routing.has_source(source)
            });
            let mut routed = None;
            if matches!(record, Record::Tick { .. }) || owned {
                seat.routing.capture_input(seat.input_enabled);
                let mut record = record.clone();
                match &mut record {
                    Record::Remove { at, .. }
                    | Record::Reconcile { at, .. }
                    | Record::Mapping { at, .. } => {
                        *at = (*at).max(seat.started);
                    }
                    _ => {}
                }
                seat.routing.push_at(record, now)?;
                routed = seat.routing.take_input();
            }
            let states = self.input_states(&name, now);
            for input in baseline.iter().filter(|input| input.source().seat == name) {
                let mut input = input.clone();
                if let Input::Reset { reason, .. } = &input {
                    if *reason != ResetReason::Removed
                        && let Some(Input::Reset { metadata, .. }) =
                            states.iter().find(|state| state.source() == input.source())
                        && let Input::Reset {
                            metadata: target, ..
                        } = &mut input
                    {
                        *target = metadata.clone();
                    }
                } else if let Some(frame) = &routed {
                    input = frame.clone();
                }
                self.report_input(&name, input, now)?;
            }
            if let Record::Remove { source, .. }
            | Record::Reconcile { source, .. }
            | Record::Mapping { source, .. } = record
            {
                self.queue(Message::Outcome {
                    report: Report::SourceInvalidated {
                        subscription: self.seats[&name].subscription.identity(),
                        source: source.clone(),
                    },
                })?;
            }
            self.collect_input(
                &name,
                now,
                matches!(record, Record::Tick { .. }).then_some(at),
                if owned {
                    if let Record::Frame { frame } = record {
                        Some(frame)
                    } else {
                        None
                    }
                } else {
                    None
                },
            )?;
        }
        Ok(())
    }
    pub(super) fn baseline_inputs(
        &mut self,
        record: &Record,
        now: Duration,
    ) -> io::Result<Vec<Input>> {
        let before = match record {
            Record::Remove { source, at } => {
                self.baseline.input_reset(source, *at, ResetReason::Removed)
            }
            Record::Device { device, .. } => self
                .baseline
                .input_states(now)
                .into_iter()
                .find(|input| {
                    input.source().device == device.source.device
                        && input.source() != &device.source
                })
                .and_then(|input| {
                    self.baseline
                        .input_reset(input.source(), now, ResetReason::Removed)
                }),
            _ => None,
        };
        let ordinary = self.seats.values().any(|seat| seat.input_enabled);
        self.baseline.capture_input(ordinary);
        self.baseline.push(record.clone())?;
        self.baseline.take_events();
        let mut inputs: Vec<_> = before.into_iter().collect();
        match record {
            Record::Device { device, .. } => inputs.extend(self.baseline.input_reset(
                &device.source,
                now,
                ResetReason::Attached,
            )),
            Record::Reconcile { source, at, .. } => inputs.extend(self.baseline.input_reset(
                source,
                *at,
                ResetReason::Reconciled,
            )),
            Record::Mapping { source, at, .. } => inputs.extend(self.baseline.input_reset(
                source,
                *at,
                ResetReason::MappingChanged,
            )),
            Record::Frame { frame } if ordinary => {
                if let Some(input) = self.baseline.take_input() {
                    inputs.push(input);
                } else {
                    inputs.extend(self.baseline.input_reset(
                        &frame.source,
                        frame.at,
                        ResetReason::Lost,
                    ));
                }
            }
            _ => {}
        }
        Ok(inputs)
    }
    pub(super) fn prune_reports(&mut self, now: Duration) -> io::Result<()> {
        let config = &self.config;
        let seats = &self.seats;
        let grabbed = &self.grabbed;
        self.messages.retain(|message| {
            let Message::Outcome { report } = message else {
                return true;
            };
            if matches!(report, Report::ContextLost { .. } | Report::Ended { .. }) {
                return true;
            }
            let Some(seat) = seats.get(&report.subscription().seat).filter(|seat| {
                seat.subscription.same_registration(report.subscription()) && now < seat.expires
            }) else {
                return false;
            };
            match report {
                Report::Dispatch { .. } => true,
                Report::RecognizerMetadata { metadata, .. } => {
                    permits_recognition(config, &seat.subscription, now) && metadata.fresh(now)
                }
                Report::RecognizerReady { ready: true, .. } => {
                    permits_recognition(config, &seat.subscription, now)
                }
                Report::Recognition { ticket, .. } => {
                    seat.recognizer_ready == Some(true)
                        && permits_recognition(config, &seat.subscription, now)
                        && now < ticket.deadline
                        && ticket.revision == seat.routing.rule_revision()
                        && grabbed.contains(ticket.input.source())
                        && seat.routing.has_source(ticket.input.source())
                }
                Report::Input { input, .. } => {
                    permits_input(config, &seat.subscription, now)
                        && input.at() >= seat.input_started
                }
                _ => true,
            }
        });
        Ok(())
    }
}
