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
        raw.grab()?;
        self.grabbed = true;
        send(&mut self.source, 1, &[(EV_KEY, 30, 1)]);
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
