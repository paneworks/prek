use crate::{
    input::{Frame, Source},
    keys::{
        native::Prepared,
        output::{Output, Progress, Sink},
        worker::Seat as Actor,
    },
    protocol::Fence,
    routing::{KeyWork, Routing},
    rules::Event,
    transport::{Consumer, ConsumerEvidence, Descriptor},
};
use std::{
    io,
    os::fd::{AsFd, BorrowedFd},
    time::Duration,
};

const OFFER: Duration = Duration::from_millis(100);
#[cfg(test)]
use crate::keys::worker::Submission;

pub(super) trait Preparation {
    type Sink: Sink;
    fn completed_at(at: Duration) -> io::Result<Duration>;
    fn descriptor(&self) -> &Descriptor;
    fn deadline(&self) -> Duration;
    fn observe(&mut self, evidence: &ConsumerEvidence, at: Duration) -> io::Result<()>;
    fn activate(&mut self, at: Duration) -> io::Result<Option<Output<Self::Sink>>>;
}
impl Preparation for Prepared {
    type Sink = crate::kernel::Virtual;
    fn completed_at(_at: Duration) -> io::Result<Duration> {
        crate::ipc::monotonic()
    }
    fn descriptor(&self) -> &Descriptor {
        self.descriptor()
    }
    fn deadline(&self) -> Duration {
        self.deadline()
    }
    fn observe(&mut self, evidence: &ConsumerEvidence, _at: Duration) -> io::Result<()> {
        self.observe(evidence)
    }
    fn activate(&mut self, _at: Duration) -> io::Result<Option<Output<Self::Sink>>> {
        self.activate()
    }
}

enum Phase<P: Preparation> {
    Preparing(Box<P>),
    Active(Box<Actor<P::Sink>>),
    Closing(Box<Actor<P::Sink>>),
    Closed,
}
pub(super) struct Slot<P: Preparation = Prepared> {
    fence: Fence,
    descriptor: Descriptor,
    consumer: Consumer,
    evidence: Option<ConsumerEvidence>,
    offered: Option<Duration>,
    phase: Phase<P>,
    failed: bool,
    retirement: Option<Duration>,
    drain: Option<crate::keys::drain::Request>,
    drain_offered: bool,
}
impl<P: Preparation> Slot<P> {
    pub(super) fn new(fence: Fence, consumer: Consumer, prepared: P) -> io::Result<Self> {
        fence.validate()?;
        let descriptor = prepared.descriptor().clone();
        descriptor.validate().map_err(invalid)?;
        if !descriptor.is_generated_keyboard()
            || crate::transport::Marker::parse(&descriptor.marker)
                .map_err(invalid)?
                .publication
                .is_none()
            || descriptor.is_isolated() != matches!(&consumer, Consumer::IsolatedReceiver { .. })
        {
            return Err(invalid("generated slot role or consumer isolation differs"));
        }
        Ok(Self {
            fence,
            descriptor,
            consumer,
            evidence: None,
            offered: None,
            phase: Phase::Preparing(Box::new(prepared)),
            failed: false,
            retirement: None,
            drain: None,
            drain_offered: false,
        })
    }
    pub(super) fn matches(&self, fence: Fence, source: &Source, marker: &str) -> bool {
        self.fence == fence && self.descriptor.source == *source && self.descriptor.marker == marker
    }
    pub(super) fn belongs_to(&self, fence: Fence, consumer: &Consumer) -> bool {
        self.fence == fence && &self.consumer == consumer
    }
    pub(super) fn source(&self) -> &Source {
        &self.descriptor.source
    }

    #[cfg(feature = "native-test-harness")]
    pub(super) fn fixture_settled(&self) -> bool {
        match &self.phase {
            Phase::Active(actor) => actor.fixture_settled(),
            Phase::Closing(_) => false,
            Phase::Preparing(_) => false,
            Phase::Closed => true,
        }
    }
    pub(super) fn offer(&mut self, at: Duration) -> Option<Descriptor> {
        if self.failed
            || matches!(self.phase, Phase::Closing(_) | Phase::Closed)
            || self
                .offered
                .is_some_and(|old| at < old.saturating_add(OFFER))
        {
            return None;
        }
        self.offered = Some(at);
        Some(self.descriptor.clone())
    }
    pub(super) fn observe(
        &mut self,
        fence: Fence,
        source: &Source,
        marker: &str,
        evidence: &ConsumerEvidence,
        at: Duration,
    ) -> io::Result<bool> {
        if !self.matches(fence, source, marker)
            || self.failed
            || matches!(self.phase, Phase::Closing(_) | Phase::Closed)
        {
            return Ok(false);
        }
        if matches!(self.phase, Phase::Active(_)) && !self.available(at)
            || matches!(&self.phase,Phase::Preparing(prepared) if at >= prepared.deadline())
        {
            self.failed = true;
            self.retire(at)?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "generated lease already expired; a fresh sample cannot restore old work",
            ));
        }
        if !evidence.valid_for(&self.descriptor, &self.consumer, at)
            || self
                .evidence
                .as_ref()
                .is_some_and(|old| evidence.at < old.at)
        {
            self.failed = true;
            self.retire(at)?;
            return Err(invalid(
                "generated consumer evidence is stale, reordered or mismatched",
            ));
        }
        if let Phase::Preparing(prepared) = &mut self.phase
            && let Err(error) = prepared.observe(evidence, at)
        {
            self.failed = true;
            self.retire(at)?;
            return Err(error);
        }
        self.evidence = Some(evidence.clone());
        Ok(true)
    }
    pub(super) fn available(&self, at: Duration) -> bool {
        !self.failed
            && self
                .evidence
                .as_ref()
                .is_some_and(|evidence| evidence.valid_for(&self.descriptor, &self.consumer, at))
            && matches!(&self.phase,Phase::Active(actor) if !actor.failed() && !actor.paused())
    }
    pub(super) fn advance(&mut self, at: Duration) -> io::Result<()> {
        let result = self.advance_inner(at);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn advance_inner(&mut self, at: Duration) -> io::Result<()> {
        if let Phase::Preparing(prepared) = &mut self.phase {
            if at >= prepared.deadline() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "generated preparation expired",
                ));
            }
            if let Some(output) = prepared.activate(at)? {
                if output.source() != &self.descriptor.source {
                    return Err(invalid("generated activation returned another source"));
                }
                let actor = Actor::new(self.fence, output).map_err(|failure| failure.error)?;
                self.phase = Phase::Active(Box::new(actor));
            }
        }
        if matches!(self.phase, Phase::Active(_)) && !self.available(at) {
            self.retire(at)?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "generated consumer or execution lease lost; worker recovery required",
            ));
        }
        if matches!(self.phase, Phase::Closing(_))
            && self.retirement.is_some_and(|deadline| at >= deadline)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "generated consumer did not acknowledge neutral drain",
            ));
        }
        if let Phase::Closing(actor) = &mut self.phase
            && self.drain.is_none()
            && actor.drain(at)? == Progress::Complete
        {
            let nonce = crate::protocol::Epoch::random()?;
            let requested_at = P::completed_at(at)?;
            let deadline = self.retirement.unwrap();
            if requested_at < at || requested_at >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "generated local drain exhausted the consumer acknowledgement budget",
                ));
            }
            self.drain = Some(crate::keys::drain::Request {
                fence: self.fence,
                source: self.descriptor.source.clone(),
                marker: self.descriptor.marker.clone(),
                consumer: self.consumer.clone(),
                nonce,
                requested_at,
                deadline,
            });
        }
        Ok(())
    }
    pub(super) fn retire(&mut self, at: Duration) -> io::Result<()> {
        let phase = std::mem::replace(&mut self.phase, Phase::Closed);
        match phase {
            Phase::Active(mut actor) => {
                let result = actor.retire(at);
                self.retirement = actor.retirement_deadline();
                self.phase = Phase::Closing(actor);
                result?;
            }
            Phase::Closing(actor) => self.phase = Phase::Closing(actor),
            Phase::Preparing(_) | Phase::Closed => {}
        }
        Ok(())
    }
    pub(super) fn closed(&self) -> bool {
        matches!(self.phase, Phase::Closed)
    }
    pub(super) fn drain_request(&mut self) -> Option<crate::keys::drain::Request> {
        if self.drain_offered {
            return None;
        }
        let request = self.drain.clone()?;
        self.drain_offered = true;
        Some(request)
    }
    pub(super) fn acknowledge_drain(
        &mut self,
        request: &crate::keys::drain::Request,
        at: Duration,
        now: Duration,
    ) -> bool {
        if !matches!(self.phase, Phase::Closing(_))
            || !self.drain_offered
            || self.drain.as_ref() != Some(request)
            || at < request.requested_at
            || at > now
            || now >= request.deadline
        {
            return false;
        }
        self.phase = Phase::Closed;
        self.drain = None;
        true
    }
    pub(super) fn retiring(&self) -> bool {
        matches!(self.phase, Phase::Closing(_) | Phase::Closed)
    }
    pub(super) fn poll(&mut self, routing: &mut Routing, at: Duration) -> io::Result<()> {
        if !self.available(at) {
            routing.revoke_key_work();
        }
        match &mut self.phase {
            Phase::Active(actor) => {
                actor.poll(routing, at)?;
            }
            Phase::Closing(actor) => {
                if self.drain.is_none() {
                    actor.drain(at)?;
                }
            }
            Phase::Preparing(_) | Phase::Closed => {}
        }
        Ok(())
    }
    #[cfg(test)]
    pub(super) fn enqueue(&mut self, work: KeyWork, at: Duration) -> io::Result<()> {
        if !self.available(at) {
            return Err(invalid("key dispatch has no live generated output"));
        }
        let Phase::Active(actor) = &mut self.phase else {
            unreachable!()
        };
        match actor.enqueue(work, at)? {
            Submission::Ignored | Submission::Queued | Submission::Remap(_) => Ok(()),
        }
    }
    pub(super) fn admit(
        &mut self,
        routing: &Routing,
        work: Vec<KeyWork>,
        input: Option<&Frame>,
        at: Duration,
    ) -> io::Result<()> {
        if !self.available(at) {
            return if work.is_empty() {
                Ok(())
            } else {
                Err(invalid("key dispatch has no live generated output"))
            };
        }
        let Phase::Active(actor) = &mut self.phase else {
            unreachable!()
        };
        actor.admit(routing, work, input, at)?;
        Ok(())
    }
    pub(super) fn seat(&self) -> &str {
        &self.descriptor.source.seat
    }
    pub(super) fn cancel(&mut self, event: &Event, at: Duration) -> io::Result<()> {
        if let Phase::Active(actor) | Phase::Closing(actor) = &mut self.phase {
            actor.cancel(self.fence, event, at)?;
        }
        Ok(())
    }
    pub(super) fn retire_source(&mut self, source: &Source, at: Duration) -> io::Result<()> {
        if let Phase::Active(actor) | Phase::Closing(actor) = &mut self.phase {
            actor.retire_source(source, at)?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub(super) fn blocks_raw(&self, source: &Source, sequence: u64) -> bool {
        match &self.phase {
            Phase::Active(actor) | Phase::Closing(actor) => actor.blocks_raw(source, sequence),
            _ => false,
        }
    }
    pub(super) fn blocks_input(
        &self,
        routing: &Routing,
        source: &Source,
        sequence: u64,
        span: Option<crate::router::InputSpan>,
    ) -> bool {
        if matches!(self.phase, Phase::Closing(_)) {
            return source.seat == self.descriptor.source.seat;
        }
        match &self.phase {
            Phase::Active(actor) | Phase::Closing(actor) => span.map_or_else(
                || actor.blocks_raw(source, sequence),
                |span| actor.blocks_ordered_span(routing, source, span),
            ),
            _ => false,
        }
    }
    pub(super) fn deadline(&self) -> Option<Duration> {
        let phase = match &self.phase {
            Phase::Preparing(prepared) => Some(prepared.deadline()),
            Phase::Active(actor) => actor.deadline(),
            Phase::Closing(_) => self.retirement,
            Phase::Closed => None,
        };
        phase
            .into_iter()
            .chain((!self.retiring()).then(|| {
                self.offered
                    .map_or(Duration::ZERO, |at| at.saturating_add(OFFER))
            }))
            .chain(
                matches!(self.phase, Phase::Active(_))
                    .then(|| self.evidence.as_ref().map(ConsumerEvidence::deadline))
                    .flatten(),
            )
            .min()
    }
    pub(super) fn poll_fd(&self) -> Option<BorrowedFd<'_>>
    where
        P::Sink: AsFd,
    {
        match &self.phase {
            Phase::Active(actor) | Phase::Closing(actor) => actor.poll_fd(),
            _ => None,
        }
    }
}
fn invalid(reason: &str) -> io::Error {
    crate::protocol::invalid(reason)
}

#[cfg(test)]
impl Slot<Prepared> {
    pub(crate) fn ordinary_file_fixture(
        fence: Fence,
        consumer: Consumer,
        device: crate::kernel::Virtual,
        at: Duration,
    ) -> io::Result<Self> {
        fence.validate()?;
        let descriptor = device.descriptor().clone();
        descriptor.validate().map_err(invalid)?;
        if !device.is_ordinary_file_fixture()
            || !descriptor.is_generated_keyboard()
            || !matches!(&consumer, Consumer::IsolatedReceiver { .. })
        {
            return Err(invalid(
                "active model requires its isolated generated ordinary-file adapter",
            ));
        }
        let evidence = ConsumerEvidence {
            consumer: consumer.clone(),
            marker: descriptor.marker.clone(),
            name: descriptor.name.clone(),
            class: descriptor.class,
            seat: descriptor.source.seat.clone(),
            settings: descriptor.consumer_settings.clone(),
            at,
        };
        let actor = Actor::new(fence, Output::new(device)?).map_err(|failure| failure.error)?;
        Ok(Self {
            fence,
            descriptor,
            consumer,
            evidence: Some(evidence),
            offered: Some(at),
            phase: Phase::Active(Box::new(actor)),
            failed: false,
            retirement: None,
            drain: None,
            drain_offered: false,
        })
    }
}

#[cfg(test)]
mod tests;
