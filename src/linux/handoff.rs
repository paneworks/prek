//! Bounded evdev-client fanout classification, not consumer or neutral readiness.
use super::{RawDevice, StateProbe, libc, reopen_readonly, set_clock_id};
use crate::input::{EV_SYN, SYN_DROPPED, SYN_REPORT};
use evdev::InputEvent;
use std::{
    error::Error,
    fmt, io,
    os::fd::{AsFd, AsRawFd},
    time::{Duration, Instant},
};

pub const MAX_EVENTS: usize = 65_536;
const MAX_READS: usize = 4096;
const MAX_WORK: Duration = Duration::from_millis(50);

#[derive(Debug, Eq, PartialEq)]
pub enum Preparation {
    /// Original events that the caller must deliver through its observation loop.
    Observe(Vec<InputEvent>),
    Armed,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Cut {
    /// Exact original prefix already broadcast to other evdev clients.
    pub observed: Vec<InputEvent>,
    /// Original events read beyond that prefix, preceding the reader's queue.
    pub retained: Vec<InputEvent>,
}

/// Original events consumed by an unsuccessful handoff attempt.
#[derive(Default)]
pub struct ReadJournal {
    original: Vec<InputEvent>,
    matched: usize,
    witness: usize,
    frozen: bool,
    acquired: bool,
}
impl ReadJournal {
    /// Every original event read by the failed operation, including an unchecked final batch.
    pub fn original(&self) -> &[InputEvent] {
        &self.original
    }
    pub fn into_original(self) -> Vec<InputEvent> {
        self.original
    }
    /// Exact equal events matched before the first mismatch or read failure.
    pub fn matched_prefix(&self) -> usize {
        self.matched
    }
    /// Witness events read during classification, including an invalid final batch.
    pub fn witness_events(&self) -> usize {
        self.witness
    }
    /// The synchronized witness was completely drained, validated and report-terminated.
    pub fn witness_frozen(&self) -> bool {
        self.frozen
    }
    /// This attempt acquired the grab; this does not describe current ownership or release.
    pub fn grab_acquired(&self) -> bool {
        self.acquired
    }
}
impl fmt::Debug for ReadJournal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadJournal")
            .field("original_events", &self.original.len())
            .field("matched_prefix", &self.matched)
            .field("witness_events", &self.witness)
            .field("witness_frozen", &self.frozen)
            .field("grab_acquired", &self.acquired)
            .finish()
    }
}
/// Failed operation with its cause and non-authorizing raw-read journal.
#[derive(Debug)]
pub struct ReadFailure {
    cause: io::Error,
    journal: ReadJournal,
}
impl ReadFailure {
    pub fn journal(&self) -> &ReadJournal {
        &self.journal
    }
    pub fn into_parts(self) -> (io::Error, ReadJournal) {
        (self.cause, self.journal)
    }
}
impl fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.cause, f)
    }
}
impl Error for ReadFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.cause)
    }
}

/// Exclusively borrow a nonblocking, unfiltered monotonic reader at an assembler
/// boundary. An uncommitted grab is released on drop. No method grants readiness
/// or authenticates a consumer, and console input-handler delivery is separate.
pub struct Handoff<'a> {
    protocol: Protocol<Kernel<'a>>,
}
impl<'a> Handoff<'a> {
    pub fn new(reader: &'a mut RawDevice) -> io::Result<Self> {
        if reader.is_grabbed() {
            return Err(invalid("handoff reader is already grabbed"));
        }
        let witness = RawDevice::from_fd(reopen_readonly(reader.as_fd())?.into())?;
        let clock = libc::CLOCK_MONOTONIC;
        // SAFETY: owned private witness fd and initialized c_int clock value.
        unsafe { set_clock_id(witness.as_raw_fd(), &clock) }.map_err(io::Error::from)?;
        let probe = StateProbe::new(reader)?;
        Ok(Self {
            protocol: Protocol::new(Kernel {
                reader,
                witness,
                probe,
            }),
        })
    }
    /// Drain private witness history, then observe the original reader once.
    pub fn prepare(&mut self) -> io::Result<Preparation> {
        self.protocol.prepare()
    }
    pub fn grab(&mut self) -> io::Result<()> {
        self.protocol.grab()
    }
    /// Freeze the witness through the state-query lock and match its exact prefix.
    pub fn classify(&mut self) -> io::Result<&Cut> {
        self.protocol.classify()
    }
    /// Return event data while leaving the original descriptor's grab owned.
    pub fn commit(mut self) -> io::Result<Cut> {
        self.protocol.commit()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Client {
    Witness,
    Reader,
}
trait Port {
    fn read(&mut self, client: Client) -> io::Result<Vec<InputEvent>>;
    fn grab(&mut self) -> io::Result<()>;
    fn synchronize(&mut self) -> io::Result<()>;
    fn release(&mut self) -> io::Result<()>;
}
struct Kernel<'a> {
    reader: &'a mut RawDevice,
    witness: RawDevice,
    probe: StateProbe,
}
impl Port for Kernel<'_> {
    fn read(&mut self, client: Client) -> io::Result<Vec<InputEvent>> {
        match client {
            Client::Witness => self.witness.fetch_events().map(Iterator::collect),
            Client::Reader => self.reader.fetch_events().map(Iterator::collect),
        }
    }
    fn grab(&mut self) -> io::Result<()> {
        self.reader.grab()
    }
    fn synchronize(&mut self) -> io::Result<()> {
        self.probe.synchronize()
    }
    fn release(&mut self) -> io::Result<()> {
        self.reader.ungrab()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Preparing,
    Armed,
    Grabbed,
    Classified,
    Committed,
    Failed,
}
struct Protocol<P: Port> {
    port: P,
    phase: Phase,
    owned: bool,
    cut: Option<Cut>,
    journal: ReadJournal,
}
impl<P: Port> Protocol<P> {
    fn new(port: P) -> Self {
        Self {
            port,
            phase: Phase::Preparing,
            owned: false,
            cut: None,
            journal: ReadJournal::default(),
        }
    }
    fn failure(&mut self, cause: io::Error) -> io::Error {
        self.phase = Phase::Failed;
        io::Error::new(
            cause.kind(),
            ReadFailure {
                cause,
                journal: std::mem::take(&mut self.journal),
            },
        )
    }
    fn read(&mut self, client: Client, budget: &mut Budget) -> io::Result<Vec<InputEvent>> {
        let events = read(&mut self.port, client, budget)?;
        if client == Client::Reader {
            self.journal.original.extend(events.iter().copied());
        } else if self.phase == Phase::Grabbed {
            self.journal.witness = self.journal.witness.saturating_add(events.len());
        }
        budget.check()?;
        Ok(events)
    }
    fn require(&mut self, phase: Phase) -> io::Result<()> {
        if self.phase != phase {
            Err(self.failure(invalid("evdev handoff phase is invalid or retired")))
        } else {
            Ok(())
        }
    }
    fn prepare(&mut self) -> io::Result<Preparation> {
        self.require(Phase::Preparing)?;
        self.journal = ReadJournal::default();
        let result = (|| {
            let mut budget = Budget::new();
            while !self.read(Client::Witness, &mut budget)?.is_empty() {}
            let observed = self.read(Client::Reader, &mut budget)?;
            self.journal.original.clear();
            if observed.is_empty() {
                self.phase = Phase::Armed;
                Ok(Preparation::Armed)
            } else {
                Ok(Preparation::Observe(observed))
            }
        })();
        result.map_err(|error| self.failure(error))
    }
    fn grab(&mut self) -> io::Result<()> {
        self.require(Phase::Armed)?;
        match self.port.grab() {
            Ok(()) => {
                self.owned = true;
                self.journal.acquired = true;
                self.phase = Phase::Grabbed;
                Ok(())
            }
            Err(error) => Err(self.failure(error)),
        }
    }
    fn classify_inner(&mut self) -> io::Result<Cut> {
        let mut budget = Budget::new();
        self.port.synchronize()?;
        let mut observed = Vec::new();
        loop {
            let events = self.read(Client::Witness, &mut budget)?;
            if events.is_empty() {
                break;
            }
            valid(&events)?;
            observed.extend(events);
        }
        if observed
            .last()
            .is_some_and(|event| event.event_type().0 != EV_SYN || event.code() != SYN_REPORT)
        {
            return Err(invalid("frozen evdev witness ends inside a frame"));
        }
        self.journal.frozen = true;
        let mut matched = 0;
        let mut retained = Vec::new();
        while matched < observed.len() {
            let events = self.read(Client::Reader, &mut budget)?;
            if events.is_empty() {
                return Err(invalid(
                    "original evdev reader lacks the frozen witness prefix",
                ));
            }
            valid(&events)?;
            for event in events {
                if matched < observed.len() {
                    if event != observed[matched] {
                        return Err(invalid("evdev witness and original prefix differ"));
                    }
                    matched += 1;
                    self.journal.matched = matched;
                } else {
                    retained.push(event);
                }
            }
        }
        budget.check()?;
        Ok(Cut { observed, retained })
    }
    fn classify(&mut self) -> io::Result<&Cut> {
        self.require(Phase::Grabbed)?;
        match self.classify_inner() {
            Ok(cut) => {
                self.cut = Some(cut);
                self.phase = Phase::Classified;
                Ok(self.cut.as_ref().unwrap())
            }
            Err(error) => Err(self.failure(error)),
        }
    }
    fn commit(&mut self) -> io::Result<Cut> {
        self.require(Phase::Classified)?;
        self.phase = Phase::Committed;
        self.owned = false;
        Ok(self.cut.take().unwrap())
    }
}
impl<P: Port> Drop for Protocol<P> {
    fn drop(&mut self) {
        if self.owned {
            let _ = self.port.release();
        }
    }
}
struct Budget {
    events: usize,
    reads: usize,
    deadline: Instant,
}
impl Budget {
    fn new() -> Self {
        Self {
            events: 0,
            reads: 0,
            deadline: Instant::now() + MAX_WORK,
        }
    }
    fn check(&self) -> io::Result<()> {
        if self.events > MAX_EVENTS || self.reads >= MAX_READS || Instant::now() >= self.deadline {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "evdev handoff event/read/work budget exceeded",
            ))
        } else {
            Ok(())
        }
    }
}
fn read(port: &mut impl Port, client: Client, budget: &mut Budget) -> io::Result<Vec<InputEvent>> {
    loop {
        budget.check()?;
        budget.reads += 1;
        match port.read(client) {
            Ok(events) => {
                budget.events = budget.events.saturating_add(events.len());
                return Ok(events);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                budget.check()?;
                return Ok(Vec::new());
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}
fn valid(events: &[InputEvent]) -> io::Result<()> {
    if events
        .iter()
        .any(|event| event.event_type().0 == EV_SYN && event.code() == SYN_DROPPED)
    {
        Err(invalid("evdev handoff encountered synchronization loss"))
    } else {
        Ok(())
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
