//! Original-frame assembly, bounded pending input and delivered cutover cursors.
use super::{InputSink, handoff::Cut};
use crate::{
    input::{Assembly, EV_SYN, Frame, FrameAssembler, RawEvent, SYN_DROPPED, Source},
    replay::Record,
};
use evdev::InputEvent;
use std::{
    io,
    time::{Duration, UNIX_EPOCH},
};

pub(super) const MAX_PENDING: usize = 65_536;
pub(super) struct Cursor {
    pub(super) assembler: FrameAssembler,
    completed: u64,
    delivered: u64,
    last_at: Option<Duration>,
    partial: usize,
    cutover: Option<u64>,
    failed: bool,
}
impl Cursor {
    pub(super) fn new(source: Source) -> Self {
        Self {
            assembler: FrameAssembler::new(source),
            completed: 0,
            delivered: 0,
            last_at: None,
            partial: 0,
            cutover: None,
            failed: false,
        }
    }
    pub(super) fn cut_pending(&self) -> bool {
        self.cutover.is_some()
    }
    pub(super) fn retire(&mut self) {
        self.failed = true;
    }
    fn live(&self) -> io::Result<()> {
        if self.failed {
            Err(invalid("input cursor is retired"))
        } else {
            Ok(())
        }
    }
    pub(super) fn ingest(&mut self, events: Vec<InputEvent>) -> io::Result<Vec<Frame>> {
        self.live()?;
        let result = self.ingest_inner(events);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn ingest_inner(&mut self, events: Vec<InputEvent>) -> io::Result<Vec<Frame>> {
        if events.len() > MAX_PENDING {
            return Err(invalid("input batch event limit exceeded"));
        }
        let mut frames = Vec::new();
        for event in events {
            let at = event
                .timestamp()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| invalid("invalid monotonic input timestamp"))?;
            if self.last_at.is_some_and(|last| at < last) {
                return Err(invalid("input source clock moved backwards"));
            }
            self.last_at = Some(at);
            let raw = RawEvent {
                kind: event.event_type().0,
                code: event.code(),
                value: event.value(),
            };
            self.partial += 1;
            if self.partial > 4096 || raw.kind == EV_SYN && raw.code == SYN_DROPPED {
                return Err(invalid(
                    "input synchronization lost; retire or reconcile this source",
                ));
            }
            match self.assembler.push(raw, at) {
                Assembly::Frame(frame) => {
                    self.partial = 0;
                    self.completed = frame.sequence;
                    frames.push(frame);
                }
                Assembly::Lost => {
                    return Err(invalid(
                        "input synchronization lost; retire or reconcile this source",
                    ));
                }
                Assembly::Pending => {}
            }
        }
        Ok(frames)
    }
    pub(super) fn cut(&mut self, cut: Cut, drained: bool) -> io::Result<Vec<Frame>> {
        self.live()?;
        let result = (|| {
            if !drained
                || !self.assembler.at_boundary()
                || self.completed != self.delivered
                || self.cutover.is_some()
            {
                return Err(invalid("input cutover requires a drained original cursor"));
            }
            if cut.observed.len().saturating_add(cut.retained.len()) > MAX_PENDING {
                return Err(invalid("input cutover event limit exceeded"));
            }
            let mut frames = self.ingest_inner(cut.observed)?;
            if !self.assembler.at_boundary() {
                return Err(invalid("input cutover prefix ends inside a frame"));
            }
            self.cutover = Some(self.completed);
            frames.extend(self.ingest_inner(cut.retained)?);
            Ok(frames)
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn expect_delivery(&mut self, sequence: u64) -> io::Result<()> {
        if self.failed
            || self.delivered.checked_add(1) != Some(sequence)
            || sequence > self.completed
        {
            self.failed = true;
            return Err(invalid("original input delivery cursor is out of order"));
        }
        Ok(())
    }
    fn delivered(&mut self, sequence: u64) -> io::Result<()> {
        self.expect_delivery(sequence)?;
        self.delivered = sequence;
        Ok(())
    }
    fn ready(&self) -> Option<u64> {
        self.cutover
            .filter(|through| !self.failed && self.delivered >= *through)
    }
}

pub(super) trait Cursors {
    fn cursor(&mut self, source: &Source) -> Option<&mut Cursor>;
    fn sources(&self) -> Vec<Source>;
}

#[derive(Default)]
pub(super) struct Pending {
    pub(super) frames: Vec<Frame>,
    events: usize,
    ordered: bool,
}
impl Pending {
    pub(super) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
    pub(super) fn extend(&mut self, frames: Vec<Frame>) -> io::Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        let events = frames.iter().fold(0usize, |count, frame| {
            count.saturating_add(frame.events.len())
        });
        if self.frames.len().saturating_add(frames.len()) > MAX_PENDING
            || self.events.saturating_add(events) > MAX_PENDING
        {
            return Err(invalid("global pending input frame/event limit exceeded"));
        }
        self.events += events;
        self.ordered = false;
        self.frames.extend(frames);
        Ok(())
    }
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&Frame) -> bool) {
        self.frames.retain(|frame| {
            let retained = keep(frame);
            if !retained {
                self.events -= frame.events.len();
            }
            retained
        });
    }
    pub(super) fn check_join(&self, other: &Self) -> io::Result<()> {
        if self.frames.len().saturating_add(other.frames.len()) > MAX_PENDING
            || self.events.saturating_add(other.events) > MAX_PENDING
        {
            Err(invalid("global pending input frame/event limit exceeded"))
        } else {
            Ok(())
        }
    }
    fn ready(&mut self, watermark: Duration) -> Vec<Frame> {
        if !self.ordered {
            self.frames
                .sort_by(|a, b| (a.at, &a.source, a.sequence).cmp(&(b.at, &b.source, b.sequence)));
            self.ordered = true;
        }
        let end = self.frames.partition_point(|frame| frame.at <= watermark);
        if end == 0 {
            return Vec::new();
        }
        if end == self.frames.len() {
            self.events = 0;
            return std::mem::take(&mut self.frames);
        }
        let future = self.frames.split_off(end);
        let ready = std::mem::replace(&mut self.frames, future);
        self.events -= ready.iter().map(|frame| frame.events.len()).sum::<usize>();
        ready
    }
    pub(super) fn deadline(&self) -> Option<Duration> {
        if self.is_empty() {
            None
        } else if self.ordered {
            self.frames.first().map(|frame| frame.at)
        } else {
            self.frames.iter().map(|frame| frame.at).min()
        }
    }
}

fn announce(
    cursors: &mut impl Cursors,
    sink: &mut impl InputSink,
    source: &Source,
    at: Duration,
) -> io::Result<()> {
    let cursor = cursors
        .cursor(source)
        .ok_or_else(|| invalid("input source has no original reader"))?;
    cursor.live()?;
    if let Some(through) = cursor.ready() {
        if let Err(error) = sink.reader_cutover(source, through, at) {
            cursor.failed = true;
            return Err(error);
        }
        cursor.cutover = None;
    }
    Ok(())
}

pub(super) fn dispatch(
    pending: &mut Pending,
    cursors: &mut impl Cursors,
    sink: &mut impl InputSink,
    watermark: Duration,
    delivered_at: &mut Duration,
) -> io::Result<bool> {
    for frame in pending.ready(watermark) {
        if frame.at < *delivered_at {
            if let Some(cursor) = cursors.cursor(&frame.source) {
                cursor.retire();
            }
            return Err(invalid("input frame precedes the delivered watermark"));
        }
        cursors
            .cursor(&frame.source)
            .ok_or_else(|| invalid("input source has no original reader"))?
            .expect_delivery(frame.sequence)?;
        announce(cursors, sink, &frame.source, *delivered_at)?;
        let source = frame.source.clone();
        let sequence = frame.sequence;
        let at = frame.at;
        if let Err(error) = sink.push(Record::Frame { frame }) {
            cursors.cursor(&source).unwrap().retire();
            return Err(error);
        }
        cursors
            .cursor(&source)
            .ok_or_else(|| invalid("input source has no original reader"))?
            .delivered(sequence)?;
        *delivered_at = at;
        if sink.finished() {
            return Ok(true);
        }
    }
    announce_ready(cursors, sink, *delivered_at)?;
    Ok(false)
}

pub(super) fn announce_ready(
    cursors: &mut impl Cursors,
    sink: &mut impl InputSink,
    at: Duration,
) -> io::Result<()> {
    for source in cursors.sources() {
        announce(cursors, sink, &source, at)?;
    }
    Ok(())
}

pub(super) fn retire_source(
    pending: &mut Pending,
    cursors: &mut impl Cursors,
    sink: &mut impl InputSink,
    source: &Source,
    at: Duration,
) -> io::Result<()> {
    let cursor = cursors
        .cursor(source)
        .ok_or_else(|| invalid("retiring input source has no original reader"))?;
    cursor.retire();
    pending.retain(|frame| &frame.source != source);
    sink.push(Record::Remove {
        source: source.clone(),
        at,
    })
}

pub(super) fn drain(
    cursor: &mut Cursor,
    mut fetch: impl FnMut() -> io::Result<Vec<InputEvent>>,
) -> io::Result<Vec<Frame>> {
    cursor.live()?;
    let mut frames = Vec::new();
    let mut events = 0usize;
    for _ in 0..4096 {
        let batch = match fetch() {
            Ok(events) => events,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(frames),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                cursor.failed = true;
                return Err(error);
            }
        };
        if batch.is_empty() {
            return Ok(frames);
        }
        events = events.saturating_add(batch.len());
        if events > MAX_PENDING {
            cursor.failed = true;
            return Err(invalid("original input drain event limit exceeded"));
        }
        frames.extend(cursor.ingest(batch)?);
    }
    cursor.failed = true;
    Err(invalid("original input drain read limit exceeded"))
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
