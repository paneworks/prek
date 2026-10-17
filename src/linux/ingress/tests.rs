use super::*;
use crate::input::{EV_KEY, SYN_REPORT};
use std::collections::BTreeMap;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}
fn source(id: u64) -> Source {
    Source {
        device: id,
        generation: 1,
        seat: "seat0".into(),
    }
}
fn event(at: u64, kind: u16, code: u16, value: i32) -> InputEvent {
    InputEvent::from(nix::libc::input_event {
        time: nix::libc::timeval {
            tv_sec: (at / 1000) as _,
            tv_usec: ((at % 1000) * 1000) as _,
        },
        type_: kind,
        code,
        value,
    })
}
fn key(at: u64, down: bool) -> Vec<InputEvent> {
    vec![
        event(at, EV_KEY, 30, i32::from(down)),
        event(at, EV_SYN, SYN_REPORT, 0),
    ]
}
impl Cursors for BTreeMap<Source, Cursor> {
    fn cursor(&mut self, source: &Source) -> Option<&mut Cursor> {
        self.get_mut(source)
    }
    fn sources(&self) -> Vec<Source> {
        self.keys().cloned().collect()
    }
}
#[derive(Debug, PartialEq)]
enum Effect {
    Frame(bool, Frame),
    Cut(Source, u64, Duration),
    Removed(Source, Duration),
}
#[derive(Default)]
struct Sink {
    effects: Vec<Effect>,
    active: bool,
    fail_cut: bool,
    fail_frame: bool,
    finish: bool,
    done: bool,
}
impl InputSink for Sink {
    fn reader_cutover(&mut self, source: &Source, through: u64, at: Duration) -> io::Result<()> {
        if self.fail_cut {
            return Err(invalid("injected cutover refusal"));
        }
        self.effects.push(Effect::Cut(source.clone(), through, at));
        self.active = true;
        Ok(())
    }
    fn push(&mut self, record: Record) -> io::Result<()> {
        if self.fail_frame {
            return Err(invalid("injected frame refusal"));
        }
        if let Record::Frame { frame } = record {
            self.effects.push(Effect::Frame(self.active, frame));
            self.done |= self.finish;
        } else if let Record::Remove { source, at } = record {
            self.effects.push(Effect::Removed(source, at));
        }
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
}

#[test]
fn original_sequence_and_partial_post_cutover_frame_continue_on_the_actual_dispatch_path() {
    let src = source(1);
    let mut cursors = BTreeMap::from([(src.clone(), Cursor::new(src.clone()))]);
    let mut pending = Pending::default();
    pending
        .extend(cursors.get_mut(&src).unwrap().ingest(key(1, true)).unwrap())
        .unwrap();
    let mut sink = Sink::default();
    let mut at = ms(0);
    dispatch(&mut pending, &mut cursors, &mut sink, ms(1), &mut at).unwrap();
    let frames = cursors
        .get_mut(&src)
        .unwrap()
        .cut(
            Cut {
                observed: key(10, false),
                retained: vec![event(20, EV_KEY, 30, 1)],
            },
            true,
        )
        .unwrap();
    pending.extend(frames).unwrap();
    dispatch(&mut pending, &mut cursors, &mut sink, ms(15), &mut at).unwrap();
    assert_eq!(at, ms(10));
    assert_eq!(
        sink.effects.last(),
        Some(&Effect::Cut(src.clone(), 2, ms(10)))
    );
    assert!(!cursors[&src].assembler.at_boundary());
    let tail = cursors
        .get_mut(&src)
        .unwrap()
        .ingest(vec![event(20, EV_SYN, SYN_REPORT, 0)])
        .unwrap();
    pending.extend(tail).unwrap();
    dispatch(&mut pending, &mut cursors, &mut sink, ms(25), &mut at).unwrap();
    let frames: Vec<_> = sink
        .effects
        .iter()
        .filter_map(|effect| {
            if let Effect::Frame(active, frame) = effect {
                Some((*active, frame))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        frames
            .iter()
            .map(|(_, frame)| frame.sequence)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        frames.iter().map(|(active, _)| *active).collect::<Vec<_>>(),
        [false, false, true]
    );
    assert_eq!(frames[2].1.source, src);
    assert_eq!(frames[2].1.at, ms(20));
    assert_eq!(
        frames[2]
            .1
            .events
            .iter()
            .map(|event| (event.kind, event.code, event.value))
            .collect::<Vec<_>>(),
        [(EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]
    );
    assert_eq!(
        sink.effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Cut(..)))
            .count(),
        1
    );
}

#[test]
fn global_watermark_waits_for_a_future_prefix_but_not_for_a_future_post_frame() {
    let src = source(1);
    let other = source(2);
    let mut cursors = BTreeMap::from([
        (src.clone(), Cursor::new(src.clone())),
        (other.clone(), Cursor::new(other.clone())),
    ]);
    let mut pending = Pending::default();
    pending
        .extend(
            cursors
                .get_mut(&src)
                .unwrap()
                .cut(
                    Cut {
                        observed: key(50, true),
                        retained: key(60, false),
                    },
                    true,
                )
                .unwrap(),
        )
        .unwrap();
    pending
        .extend(
            cursors
                .get_mut(&other)
                .unwrap()
                .ingest(key(45, true))
                .unwrap(),
        )
        .unwrap();
    let mut sink = Sink::default();
    let mut at = ms(10);
    dispatch(&mut pending, &mut cursors, &mut sink, ms(40), &mut at).unwrap();
    assert!(sink.effects.is_empty());
    assert_eq!(at, ms(10));
    dispatch(&mut pending, &mut cursors, &mut sink, ms(55), &mut at).unwrap();
    assert!(
        matches!(&sink.effects[..], [Effect::Frame(false,a),Effect::Frame(false,b),Effect::Cut(s,1,t)] if a.source==other && b.source==src && s==&src && *t==ms(50))
    );
    assert_eq!(pending.frames.len(), 1);
    dispatch(&mut pending, &mut cursors, &mut sink, ms(65), &mut at).unwrap();
    assert!(
        matches!(sink.effects.last(),Some(Effect::Frame(true,frame)) if frame.at==ms(60) && frame.sequence==2)
    );
}

#[test]
fn an_empty_prefix_announces_before_poll_without_advancing_an_unread_tail() {
    let src = source(1);
    let mut cursors = BTreeMap::from([(src.clone(), Cursor::new(src.clone()))]);
    let mut pending = Pending::default();
    pending
        .extend(
            cursors
                .get_mut(&src)
                .unwrap()
                .cut(
                    Cut {
                        observed: vec![],
                        retained: key(10, true),
                    },
                    true,
                )
                .unwrap(),
        )
        .unwrap();
    let mut sink = Sink::default();
    announce_ready(&mut cursors, &mut sink, ms(3)).unwrap();
    assert_eq!(sink.effects, [Effect::Cut(src, 0, ms(3))]);
    let mut at = ms(3);
    dispatch(&mut pending, &mut cursors, &mut sink, ms(10), &mut at).unwrap();
    assert!(matches!(sink.effects.last(),Some(Effect::Frame(true,frame)) if frame.sequence==1));
}

#[test]
fn incomplete_queued_unacknowledged_or_repeated_cuts_fail_closed() {
    for mode in [
        "partial", "queued", "external", "repeat", "prefix", "loss", "size",
    ] {
        let mut cursor = Cursor::new(source(1));
        let mut cut = Cut {
            observed: vec![],
            retained: vec![],
        };
        let mut drained = true;
        match mode {
            "partial" => {
                cursor.ingest(vec![event(0, EV_KEY, 30, 1)]).unwrap();
            }
            "queued" => {
                cursor.ingest(key(0, true)).unwrap();
            }
            "external" => drained = false,
            "repeat" => {
                cursor
                    .cut(
                        Cut {
                            observed: vec![],
                            retained: vec![],
                        },
                        true,
                    )
                    .unwrap();
            }
            "prefix" => cut.observed = vec![event(0, EV_KEY, 30, 1)],
            "loss" => cut.retained = vec![event(0, EV_SYN, SYN_DROPPED, 0)],
            _ => cut.retained = vec![event(0, EV_SYN, SYN_REPORT, 0); MAX_PENDING + 1],
        }
        assert!(cursor.cut(cut, drained).is_err(), "{mode}");
        assert!(cursor.ingest(key(1, false)).is_err());
        assert!(cursor.ready().is_none());
    }
}

#[test]
fn loss_oversized_partial_frames_and_backwards_events_retire_immediately() {
    for mode in ["drop", "size", "clock"] {
        let mut cursor = Cursor::new(source(1));
        let bad = match mode {
            "drop" => vec![event(10, EV_SYN, SYN_DROPPED, 0)],
            "size" => vec![event(10, EV_KEY, 30, 1); 4097],
            _ => {
                cursor.ingest(key(10, true)).unwrap();
                key(9, false)
            }
        };
        assert!(cursor.ingest(bad).is_err(), "{mode}");
        assert!(cursor.ingest(key(20, false)).is_err());
        assert!(drain(&mut cursor, || Err(io::ErrorKind::WouldBlock.into())).is_err());
    }
}

#[test]
fn pending_storage_bounds_raw_events_atomically_and_reclaims_cancelled_storage() {
    let mut cursor = Cursor::new(source(1));
    let frame = cursor.ingest(key(1, true)).unwrap().remove(0);
    let mut pending = Pending::default();
    pending
        .extend(vec![frame.clone(); MAX_PENDING / 2])
        .unwrap();
    assert!(pending.extend(vec![frame.clone()]).is_err());
    assert_eq!(pending.frames.len(), MAX_PENDING / 2);
    assert_eq!(pending.events, MAX_PENDING);
    let mut other = Pending::default();
    other.extend(vec![frame.clone()]).unwrap();
    assert!(pending.check_join(&other).is_err());
    pending.retain(|_| false);
    pending.extend(other.frames).unwrap();
    assert_eq!(pending.events, 2);
    assert_eq!(pending.frames, [frame]);
}

#[test]
fn callback_frame_and_cursor_errors_cannot_deliver_a_post_cutover_frame() {
    for mode in ["callback", "frame", "order", "clock"] {
        let src = source(1);
        let mut cursor = Cursor::new(src.clone());
        let mut frames = cursor
            .cut(
                Cut {
                    observed: vec![],
                    retained: key(10, true),
                },
                true,
            )
            .unwrap();
        let mut at = ms(0);
        let mut sink = Sink::default();
        match mode {
            "callback" => sink.fail_cut = true,
            "frame" => sink.fail_frame = true,
            "order" => frames[0].sequence = 2,
            _ => at = ms(11),
        }
        let mut cursors = BTreeMap::from([(src.clone(), cursor)]);
        let mut pending = Pending::default();
        pending.extend(frames).unwrap();
        assert!(dispatch(&mut pending, &mut cursors, &mut sink, ms(20), &mut at).is_err());
        assert!(
            !sink
                .effects
                .iter()
                .any(|effect| matches!(effect, Effect::Frame(..)))
        );
        assert_eq!(cursors[&src].delivered, 0);
        assert!(cursors[&src].failed);
    }
}

#[test]
fn finish_at_the_prefix_stops_without_announcing_or_delivering_the_suffix() {
    let src = source(1);
    let mut cursor = Cursor::new(src.clone());
    let frames = cursor
        .cut(
            Cut {
                observed: key(10, true),
                retained: key(20, false),
            },
            true,
        )
        .unwrap();
    let mut cursors = BTreeMap::from([(src, cursor)]);
    let mut pending = Pending::default();
    pending.extend(frames).unwrap();
    let mut sink = Sink {
        finish: true,
        ..Default::default()
    };
    let mut at = ms(0);
    assert!(dispatch(&mut pending, &mut cursors, &mut sink, ms(20), &mut at).unwrap());
    assert!(matches!(&sink.effects[..],[Effect::Frame(false,frame)] if frame.sequence==1));
    assert_eq!(at, ms(10));
}

#[test]
fn the_real_drain_adapter_preserves_partial_batches_and_bounds_interrupted_reads() {
    let mut cursor = Cursor::new(source(1));
    let mut parts = vec![
        vec![event(10, EV_KEY, 30, 1)],
        vec![event(10, EV_SYN, SYN_REPORT, 0)],
    ]
    .into_iter();
    let frames = drain(&mut cursor, || {
        parts.next().ok_or_else(|| io::ErrorKind::WouldBlock.into())
    })
    .unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].sequence, 1);
    assert_eq!(frames[0].events.len(), 2);
    let mut cursor = Cursor::new(source(2));
    let mut reads = 0;
    assert!(
        drain(&mut cursor, || {
            reads += 1;
            Err(io::ErrorKind::Interrupted.into())
        })
        .is_err()
    );
    assert_eq!(reads, 4096);
    assert!(cursor.failed);
}

#[test]
fn sinks_without_explicit_cutover_support_refuse_the_data_boundary() {
    struct Ordinary;
    impl InputSink for Ordinary {
        fn push(&mut self, _: Record) -> io::Result<()> {
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn deadline(&self) -> Option<Duration> {
            None
        }
    }
    let src = source(1);
    let mut cursor = Cursor::new(src.clone());
    cursor
        .cut(
            Cut {
                observed: vec![],
                retained: vec![],
            },
            true,
        )
        .unwrap();
    let mut cursors = BTreeMap::from([(src.clone(), cursor)]);
    assert_eq!(
        announce_ready(&mut cursors, &mut Ordinary, ms(0))
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert!(cursors[&src].failed);
}

#[test]
fn failed_source_pending_input_is_cancelled_before_healthy_frames_without_a_future_tick() {
    let failed = source(1);
    let healthy = source(2);
    let mut cursors = BTreeMap::from([
        (failed.clone(), Cursor::new(failed.clone())),
        (healthy.clone(), Cursor::new(healthy.clone())),
    ]);
    let mut pending = Pending::default();
    pending
        .extend(
            cursors
                .get_mut(&failed)
                .unwrap()
                .ingest(key(50, true))
                .unwrap(),
        )
        .unwrap();
    pending
        .extend(
            cursors
                .get_mut(&healthy)
                .unwrap()
                .ingest(key(10, true))
                .unwrap(),
        )
        .unwrap();
    let mut sink = Sink::default();
    let mut at = ms(5);
    retire_source(&mut pending, &mut cursors, &mut sink, &failed, at).unwrap();
    assert!(cursors[&failed].failed);
    cursors.remove(&failed);
    dispatch(&mut pending, &mut cursors, &mut sink, ms(60), &mut at).unwrap();
    assert!(
        matches!(&sink.effects[..],[Effect::Removed(source,t),Effect::Frame(false,frame)] if source==&failed && *t==ms(5) && frame.source==healthy && frame.at==ms(10))
    );
    assert_eq!(at, ms(10));
    assert!(pending.is_empty());
}

#[test]
fn a_future_pending_frame_supplies_a_real_poll_deadline_without_copying_the_queue() {
    let mut pending = Pending::default();
    let mut cursor = Cursor::new(source(1));
    pending
        .extend(cursor.ingest(key(50, true)).unwrap())
        .unwrap();
    let storage = pending.frames.as_ptr();
    assert_eq!(pending.deadline(), Some(ms(50)));
    assert!(pending.ready(ms(10)).is_empty());
    assert_eq!(pending.frames.as_ptr(), storage);
    assert_eq!(pending.deadline(), Some(ms(50)));
    assert_eq!(pending.ready(ms(50)).len(), 1);
    assert_eq!(pending.deadline(), None);
    assert_eq!(pending.events, 0);
}
