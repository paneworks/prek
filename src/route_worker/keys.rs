use crate::{
    input::{Frame, Source},
    keys::{
        native::Prepared,
        output::{Output, Progress, Sink},
        worker::{Seat as Actor, Submission},
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

pub(super) trait Preparation {
    type Sink: Sink;
    fn descriptor(&self) -> &Descriptor;
    fn deadline(&self) -> Duration;
    fn observe(&mut self, evidence: &ConsumerEvidence, at: Duration) -> io::Result<()>;
    fn activate(&mut self, at: Duration) -> io::Result<Option<Output<Self::Sink>>>;
}
impl Preparation for Prepared {
    type Sink = crate::kernel::Virtual;
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
        if let Phase::Closing(actor) = &mut self.phase
            && actor.drain(at)? == Progress::Complete
        {
            self.phase = Phase::Closed;
        }
        Ok(())
    }
    pub(super) fn retire(&mut self, at: Duration) -> io::Result<()> {
        let phase = std::mem::replace(&mut self.phase, Phase::Closed);
        match phase {
            Phase::Active(mut actor) => {
                let result = actor.retire(at);
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
                actor.drain(at)?;
            }
            Phase::Preparing(_) | Phase::Closed => {}
        }
        Ok(())
    }
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
    pub(super) fn repeat(&mut self, frame: &Frame, at: Duration) -> io::Result<()> {
        if !self.available(at) {
            return Ok(());
        }
        if frame
            .events
            .iter()
            .any(|event| event.kind == crate::input::EV_KEY && event.value == 2)
            && frame
                .events
                .iter()
                .any(|event| event.kind == crate::input::EV_KEY && matches!(event.value, 0 | 1))
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "mixed key/repeat input requires the shared input-position projector",
            ));
        }
        let Phase::Active(actor) = &mut self.phase else {
            unreachable!()
        };
        for (index, event) in frame
            .events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.kind == crate::input::EV_KEY && event.value == 2)
        {
            let handles = actor.repeat_handles(&frame.source, event.code);
            if !handles.is_empty() {
                actor.repeat(&handles, frame, index, at)?;
            }
        }
        Ok(())
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
    pub(super) fn blocks_raw(&self, source: &Source, sequence: u64) -> bool {
        match &self.phase {
            Phase::Active(actor) | Phase::Closing(actor) => actor.blocks_raw(source, sequence),
            _ => false,
        }
    }
    pub(super) fn deadline(&self) -> Option<Duration> {
        let phase = match &self.phase {
            Phase::Preparing(prepared) => Some(prepared.deadline()),
            Phase::Active(actor) | Phase::Closing(actor) => actor.deadline(),
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
        })
    }
}

#[cfg(test)]
mod tests;
