use super::*;

impl Stream {
    pub(super) fn key_drain_pending(&self, seat: &str) -> bool {
        self.key_consumer_pending(seat)
            || self
                .closing_outputs
                .iter()
                .any(|(recovery, _)| recovery.source.seat == seat)
    }
    fn key_consumer_pending(&self, seat: &str) -> bool {
        self.closing_keys
            .iter()
            .any(|keys| keys.seat() == seat && !keys.closed())
            || self
                .seats
                .get(seat)
                .and_then(|seat| seat.keys.as_ref())
                .is_some_and(|keys| keys.retiring() && !keys.closed())
    }
    fn retire_closing_outputs(&mut self, now: Duration) {
        for (recovery, output) in std::mem::take(&mut self.closing_outputs) {
            if self.key_consumer_pending(&recovery.source.seat) {
                self.closing_outputs.push((recovery, output));
            } else {
                Self::retire_output(recovery, output, now);
            }
        }
    }
    pub(super) fn offer_key_drains(&mut self) -> io::Result<()> {
        let requests: Vec<_> = self
            .closing_keys
            .iter_mut()
            .chain(
                self.seats
                    .values_mut()
                    .filter_map(|seat| seat.keys.as_mut()),
            )
            .filter_map(keys::Slot::drain_request)
            .collect();
        for request in requests {
            self.queue(Message::KeyDrain { request })?;
        }
        Ok(())
    }
    pub(super) fn acknowledge_key_drain(
        &mut self,
        request: &crate::keys::drain::Request,
        at: Duration,
        now: Duration,
    ) -> bool {
        self.closing_keys
            .iter_mut()
            .chain(
                self.seats
                    .values_mut()
                    .filter_map(|seat| seat.keys.as_mut()),
            )
            .any(|keys| keys.acknowledge_drain(request, at, now))
    }
    pub(super) fn next_key_source(&mut self, seat: &str) -> io::Result<Source> {
        self.key_generation = self
            .key_generation
            .checked_add(1)
            .ok_or_else(|| invalid("generated source incarnation exhausted"))?;
        Ok(Source {
            device: self.key_generation,
            generation: self.key_generation,
            seat: seat.into(),
        })
    }
    pub(super) fn close_keys(&mut self, mut keys: keys::Slot, now: Duration) -> io::Result<()> {
        let retired = keys.retire(now);
        if self.closing_keys.len() >= crate::service::MAX_CLIENTS {
            return Err(invalid("too many generated keyboards awaiting cleanup"));
        }
        self.closing_keys.push(keys);
        retired
    }
    pub(super) fn refresh_keys(&mut self, now: Duration) -> io::Result<()> {
        if let Some(error) = &self.key_failure {
            return Err(io::Error::new(error.kind(), error.to_string()));
        }
        for keys in &mut self.closing_keys {
            keys.advance(now)?;
        }
        self.closing_keys.retain(|keys| !keys.closed());
        self.retire_closing_outputs(now);
        for seat in self.seats.values_mut() {
            let permitted = seat
                .subscription
                .capabilities
                .contains(&protocol::Capability::SyntheticInput)
                && now < seat.expires
                && seat.routing.authority().state(now) == crate::context::State::Desktop;
            if let Some(keys) = &mut seat.keys {
                if !permitted
                    || seat
                        .subscription
                        .consumer
                        .as_ref()
                        .is_none_or(|consumer| !keys.belongs_to(seat.subscription.fence, consumer))
                {
                    keys.retire(now)?;
                }
                keys.advance(now)?;
            }
            let available = permitted && seat.keys.as_ref().is_some_and(|keys| keys.available(now));
            seat.routing.key_output_available(available);
            if let Some(keys) = &mut seat.keys {
                keys.poll(&mut seat.routing, now)?;
            }
        }
        self.track_key_holds(now)?;
        self.offer_key_drains()
    }
    /// Retimes a seat's withheld input once its generated drain completes.
    fn track_key_holds(&mut self, now: Duration) -> io::Result<()> {
        let seats: BTreeSet<_> = self
            .key_holds
            .keys()
            .chain(self.seats.keys())
            .cloned()
            .chain(self.closing_keys.iter().map(|keys| keys.seat().to_owned()))
            .collect();
        for seat in seats {
            if self.key_drain_pending(&seat) {
                self.key_holds.entry(seat).or_insert(now);
            } else if let Some(start) = self.key_holds.remove(&seat)
                && let Some(owner) = self.seats.get_mut(&seat)
            {
                owner.routing.defer_input(start, now)?;
            }
        }
        Ok(())
    }
    pub(super) fn service_keys(&mut self, health: Option<&Heartbeat>) -> io::Result<()> {
        let now = monotonic()?;
        self.refresh_keys(now)?;
        let names: Vec<_> = self.seats.keys().cloned().collect();
        for name in names {
            let seat = &self.seats[&name];
            let permitted = seat
                .subscription
                .capabilities
                .contains(&protocol::Capability::SyntheticInput)
                && now < seat.expires
                && seat.routing.authority().state(now) == crate::context::State::Desktop;
            if !permitted {
                continue;
            }
            if seat.keys.is_none()
                && !self
                    .closing_keys
                    .iter()
                    .any(|keys| keys.source().seat == name)
            {
                let health = health
                    .ok_or_else(|| invalid("generated keyboard requires supervisor ownership"))?;
                let subscription = seat.subscription.clone();
                let source = self.next_key_source(&name)?;
                let consumer = subscription
                    .consumer
                    .clone()
                    .ok_or_else(|| invalid("generated keyboard has no bound consumer"))?;
                let prepared = crate::keys::native::Prepared::new(
                    &self.registry,
                    health,
                    source,
                    consumer.clone(),
                )?;
                self.seats.get_mut(&name).unwrap().keys =
                    Some(keys::Slot::new(subscription.fence, consumer, prepared)?);
            }
            let seat = self.seats.get_mut(&name).unwrap();
            if let Some(descriptor) = seat.keys.as_mut().and_then(|keys| keys.offer(now)) {
                let subscription = seat.subscription.identity();
                self.queue(Message::Transport {
                    subscription,
                    descriptor: Box::new(descriptor),
                })?;
            }
        }
        Ok(())
    }
    pub(super) fn shutdown_keys(&mut self) {
        if let Some(error) = &self.key_failure {
            eprintln!("generated keyboard failed; consumer neutrality unproven: {error}");
        }
        let until = monotonic()
            .unwrap_or(Duration::ZERO)
            .saturating_add(Duration::from_millis(250));
        while !self.closing_keys.is_empty() {
            let result = (|| {
                let now = monotonic()?;
                if now >= until {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "generated keyboard shutdown did not drain",
                    ));
                }
                for keys in &mut self.closing_keys {
                    keys.advance(now)?;
                }
                self.offer_key_drains()?;
                self.messages.flush(&mut self.channel)?;
                for _ in 0..8 {
                    let Some(packet) = self.channel.receive::<Command>()? else {
                        break;
                    };
                    if let Command::KeyDrained { request, at } = packet.message {
                        self.acknowledge_key_drain(&request, at, monotonic()?);
                    }
                }
                self.closing_keys.retain(|keys| !keys.closed());
                if !self.closing_keys.is_empty() {
                    let mut fds = self
                        .closing_keys
                        .iter()
                        .filter_map(|keys| {
                            keys.poll_fd().map(|fd| PollFd::new(fd, PollFlags::POLLOUT))
                        })
                        .collect::<Vec<_>>();
                    fds.push(PollFd::new(self.channel.as_fd(), PollFlags::POLLIN));
                    nix::poll::poll(&mut fds, 1u16).map_err(io::Error::other)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!(
                    "generated keyboard cleanup failed; consumer neutrality unproven: {error}"
                );
                break;
            }
        }
        self.closing_keys.clear();
        self.retire_closing_outputs(monotonic().unwrap_or(until));
    }
}
