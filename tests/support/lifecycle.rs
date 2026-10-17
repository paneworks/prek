//! Exercise the real reader loop's teardown ordering on owned synthetic input.
use super::kernel_fixture::*;
use evdev::raw_stream::RawDevice;
use prek::{
    input::{Device, EV_KEY, EV_SYN, Frame, RawEvent, SYN_REPORT},
    kernel::{Flush, Virtual},
    linux::{self, InputSink, ReaderBuffer, ReaderDisposition},
    replay::Record,
    supervisor::Heartbeat,
};
use std::{io, panic::AssertUnwindSafe, path::PathBuf, time::Duration};

struct CutoverSink {
    source: Virtual,
    output: Option<Virtual>,
    target: Receiver,
    probe: RawDevice,
    device: Device,
    requested: bool,
    active: bool,
    done: bool,
    prefix: Vec<Frame>,
    post: Vec<Frame>,
    boundary: Option<(u64, Duration)>,
    shutdown_held: bool,
    released: bool,
}
impl InputSink for CutoverSink {
    fn reader(
        &mut self,
        raw: &mut RawDevice,
        _device: &Device,
        buffer: ReaderBuffer,
        _health: Option<&Heartbeat>,
    ) -> io::Result<ReaderDisposition> {
        if self.requested || !buffer.is_drained() {
            return Ok(ReaderDisposition::Keep);
        }
        let mut handoff = linux::handoff::Handoff::new(raw)?;
        match handoff.prepare()? {
            linux::handoff::Preparation::Observe(events) => Ok(ReaderDisposition::Observe(events)),
            linux::handoff::Preparation::Armed => {
                send(&mut self.source, 1, &[(prek::input::EV_REL, 0, 11)]);
                handoff.grab()?;
                send(&mut self.source, 2, &[(prek::input::EV_REL, 0, 22)]);
                handoff.classify()?;
                self.requested = true;
                Ok(ReaderDisposition::Cutover(handoff.commit()?))
            }
        }
    }
    fn reader_cutover(
        &mut self,
        source: &prek::input::Source,
        through: u64,
        at: Duration,
    ) -> io::Result<()> {
        assert!(!self.active);
        assert_eq!(self.prefix.len(), 1);
        assert_eq!(&self.prefix[0].source, source);
        assert_eq!(through, self.prefix[0].sequence);
        assert_eq!(at, self.prefix[0].at);
        self.boundary = Some((through, at));
        self.active = true;
        Ok(())
    }
    fn push(&mut self, record: Record) -> io::Result<()> {
        let Record::Frame { frame } = record else {
            return Ok(());
        };
        if !self.active {
            self.prefix.push(frame);
            return Ok(());
        }
        let mut output = frame.clone();
        output.source = self.device.source.clone();
        let target = self.output.as_mut().unwrap();
        target.enqueue(&output)?;
        assert_eq!(target.flush()?, Flush::Complete);
        let received = self.target.next(Duration::from_secs(1)).unwrap();
        assert_eq!(received.events, frame.events);
        assert!(self.target.next(Duration::from_millis(20)).is_none());
        self.post.push(frame);
        self.done = true;
        Ok(())
    }
    fn deadline(&self) -> Option<Duration> {
        None
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn finished(&self) -> bool {
        self.done
    }
    fn shutdown(&mut self) {
        self.shutdown_held = match self.probe.grab() {
            Err(error) => error.raw_os_error() == Some(nix::libc::EBUSY),
            Ok(()) => {
                let _ = self.probe.ungrab();
                false
            }
        };
        drop(self.output.take());
    }
    fn reader_released(&mut self, _: &prek::input::Source) -> io::Result<()> {
        assert!(self.output.is_none());
        self.probe.grab()?;
        self.probe.ungrab()?;
        self.released = true;
        Ok(())
    }
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t58_reader_cutover_preserves_prefix_cursor_and_downstream_tail() {
    let registry = authorized();
    let motion = || {
        let mut device = fixture(false);
        device.name = format!("Prek test cutover {}", device.source.device);
        device.capabilities = std::collections::BTreeMap::from([
            (EV_SYN, vec![0]),
            (prek::input::EV_REL, vec![0, 1]),
        ]);
        device
    };
    let original = motion();
    let device = motion();
    let source =
        Virtual::create(registry.prepare_isolated(descriptor(&original)).unwrap()).unwrap();
    let mut probe = Receiver::open(&source, &original.source);
    assert!(probe.raw.supported_keys().is_none());
    probe.raw.ungrab().unwrap();
    let path = source.event_nodes().unwrap().into_iter().next().unwrap();
    let output = Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let target = Receiver::open(&output, &device.source);
    let mut sink = CutoverSink {
        source,
        output: Some(output),
        target,
        probe: probe.raw,
        device,
        requested: false,
        active: false,
        done: false,
        prefix: vec![],
        post: vec![],
        boundary: None,
        shutdown_held: false,
        released: false,
    };
    linux::observe(&[path], Some(Duration::from_secs(2)), &mut sink).unwrap();
    assert!(sink.done && sink.active && sink.shutdown_held && sink.released);
    assert_eq!(sink.prefix.len(), 1);
    assert_eq!(sink.post.len(), 1);
    assert_eq!(sink.prefix[0].sequence, 1);
    assert_eq!(sink.post[0].sequence, 2);
    assert_eq!(sink.prefix[0].source, sink.post[0].source);
    assert_eq!(sink.prefix[0].events[0].value, 11);
    assert_eq!(sink.post[0].events[0].value, 22);
    assert_eq!(sink.boundary, Some((1, sink.prefix[0].at)));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Exit {
    ReaderError,
    FrameError,
    RemoveError,
    Finished,
    Panic,
}

struct Sink {
    source: Virtual,
    output: Option<Virtual>,
    target: Receiver,
    probe: RawDevice,
    device: Device,
    exit: Exit,
    grabbed: bool,
    delivered: bool,
    done: bool,
    sequence: u64,
    shutdown_held_grab: bool,
    retired_while_grabbed: bool,
    neutral_delivered: bool,
    removed: bool,
}
impl Sink {
    fn probe_busy(&mut self) -> bool {
        match self.probe.grab() {
            Ok(()) => {
                self.probe.ungrab().unwrap();
                false
            }
            Err(error) => error.raw_os_error() == Some(nix::libc::EBUSY),
        }
    }
}
impl InputSink for Sink {
    fn reader(
        &mut self,
        raw: &mut RawDevice,
        _device: &Device,
        buffer: ReaderBuffer,
        _health: Option<&Heartbeat>,
    ) -> io::Result<ReaderDisposition> {
        if self.grabbed {
            return Ok(if self.exit == Exit::RemoveError {
                ReaderDisposition::Reopen
            } else {
                ReaderDisposition::Keep
            });
        }
        if !buffer.is_drained() {
            return Ok(ReaderDisposition::Keep);
        }
        let state = linux::StateProbe::new(raw)?;
        assert!(state.is_neutral(&self.device)?);
        raw.grab()?;
        self.grabbed = true;
        send(&mut self.source, 1, &[(EV_KEY, 30, 1)]);
        // Query after injection while the press is still queued on raw. The
        // forwarding cases below must receive it despite repeated queries.
        assert!(!state.is_neutral(&self.device)?);
        assert!(!state.is_neutral(&self.device)?);
        if self.exit == Exit::ReaderError {
            return Err(io::Error::other("injected reader failure after grab"));
        }
        Ok(ReaderDisposition::Keep)
    }
    fn push(&mut self, record: Record) -> io::Result<()> {
        if let Record::Remove { .. } = record {
            self.removed = true;
            return Err(io::Error::other("injected removal failure before close"));
        }
        let Record::Frame { mut frame } = record else {
            return Ok(());
        };
        frame.source = self.device.source.clone();
        self.sequence = frame.sequence;
        let output = self.output.as_mut().unwrap();
        output.enqueue(&frame)?;
        assert_eq!(output.flush()?, Flush::Complete);
        let received = self.target.next(Duration::from_secs(1)).unwrap();
        assert_eq!(received.events, frame.events);
        self.delivered = true;
        match self.exit {
            Exit::FrameError => Err(io::Error::other("injected forwarding failure")),
            Exit::Panic => panic!("injected forwarding panic"),
            Exit::Finished => {
                self.done = true;
                Ok(())
            }
            _ => Ok(()),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn deadline(&self) -> Option<Duration> {
        None
    }
    fn finished(&self) -> bool {
        self.done
    }
    fn shutdown(&mut self) {
        let Some(mut output) = self.output.take() else {
            return;
        };
        self.shutdown_held_grab = self.probe_busy();
        if self.delivered {
            let neutral = Frame {
                source: self.device.source.clone(),
                at: now(),
                sequence: self.sequence + 1,
                events: vec![
                    RawEvent {
                        kind: EV_KEY,
                        code: 30,
                        value: 0,
                    },
                    RawEvent {
                        kind: EV_SYN,
                        code: SYN_REPORT,
                        value: 0,
                    },
                ],
            };
            if output.enqueue(&neutral).is_ok() && output.flush().ok() == Some(Flush::Complete) {
                self.neutral_delivered = self
                    .target
                    .next(Duration::from_secs(1))
                    .is_some_and(|frame| frame.events == neutral.events);
            }
        }
        drop(output);
        self.retired_while_grabbed = self.probe_busy();
    }
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t57_reader_errors_removal_finish_and_unwind_retire_output_before_releasing_input() {
    let registry = authorized();
    for exit in [
        Exit::ReaderError,
        Exit::FrameError,
        Exit::RemoveError,
        Exit::Finished,
        Exit::Panic,
    ] {
        let source_device = fixture(false);
        let device = fixture(false);
        let source = Virtual::create(
            registry
                .prepare_isolated(descriptor(&source_device))
                .unwrap(),
        )
        .unwrap();
        let output =
            Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
        let mut original = Receiver::open(&source, &source_device.source);
        original.raw.ungrab().unwrap();
        let paths: Vec<PathBuf> = source.event_nodes().unwrap();
        let target = Receiver::open(&output, &device.source);
        let output_nodes = output.event_nodes().unwrap();
        assert_eq!(output_nodes.len(), 1);
        let mut sink = Sink {
            source,
            output: Some(output),
            target,
            probe: original.raw,
            device,
            exit,
            grabbed: false,
            delivered: false,
            done: false,
            sequence: 0,
            shutdown_held_grab: false,
            retired_while_grabbed: false,
            neutral_delivered: false,
            removed: false,
        };
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            linux::observe(&paths, Some(Duration::from_secs(2)), &mut sink)
        }));
        match exit {
            Exit::Panic => assert!(result.is_err()),
            Exit::Finished => assert!(result.unwrap().is_ok()),
            _ => assert!(result.unwrap().is_err()),
        }
        assert!(sink.grabbed, "{exit:?}: fixture never took ownership");
        assert!(
            sink.shutdown_held_grab,
            "{exit:?}: original closed before cleanup"
        );
        assert!(
            sink.retired_while_grabbed,
            "{exit:?}: original released before output retired"
        );
        if exit != Exit::ReaderError {
            assert!(
                sink.delivered && sink.neutral_delivered,
                "{exit:?}: held output was not neutralized"
            );
        }
        assert_eq!(sink.removed, exit == Exit::RemoveError);
        sink.probe
            .grab()
            .expect("original grab leaked after observe returned");
        sink.probe.ungrab().unwrap();
        assert!(sink.output.is_none());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while output_nodes.iter().any(|path| path.exists()) {
            assert!(
                std::time::Instant::now() < deadline,
                "virtual output leaked"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        send(&mut sink.source, 2, &[(EV_KEY, 30, 0)]);
    }
}
