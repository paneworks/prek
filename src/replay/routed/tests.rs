use super::*;
use crate::{
    input::{Device, EV_KEY, EV_SYN, Frame, SYN_REPORT},
    replay::configured::tests::{context, device, frame},
    transport::{Consumer, KernelIdentity},
};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn input(input: Input, now: u64) -> Record {
    Record::Input {
        now: Duration::from_millis(now),
        input: Box::new(input),
    }
}
fn source_device() -> Device {
    let Input::Device { device, .. } = device("seat0", 1) else {
        unreachable!()
    };
    device
}
fn source() -> Source {
    source_device().source
}
fn attach() -> Record {
    let device = source_device();
    let descriptor = Descriptor::mirror(
        &device,
        KernelIdentity {
            bus: 3,
            vendor: 1,
            product: 1,
            version: 1,
        },
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    let consumer = ConsumerEvidence {
        consumer: Consumer::IsolatedReceiver { pid: 1, start: 1 },
        marker: descriptor.marker.clone(),
        name: descriptor.name.clone(),
        class: descriptor.class,
        seat: "seat0".into(),
        at: ms(0),
    };
    Record::Attach {
        at: ms(0),
        descriptor: Box::new(descriptor),
        consumer,
        feedback: false,
    }
}
fn config() -> Config {
    let mut config = Config::preset("minimal").unwrap();
    config.mode = crate::config::Mode::Route;
    config.devices.insert(
        "keys".into(),
        crate::config::Selector {
            keys: vec![crate::config::Key(30)],
            ..Default::default()
        },
    );
    config.policy.allowed_devices.push("keys".into());
    config
}
fn setup(replay: &mut Routed) {
    for record in [
        Record::Header {
            version: FORMAT_VERSION,
        },
        input(context("seat0", 1, 1, 0, State::Desktop), 0),
        input(device("seat0", 1), 0),
        attach(),
    ] {
        replay.push(record).unwrap();
    }
}
fn key_frame(sequence: u64, at: u64, down: bool) -> Frame {
    Frame {
        source: source(),
        sequence,
        at: ms(at),
        events: vec![
            RawEvent {
                kind: EV_KEY,
                code: 30,
                value: i32::from(down),
            },
            RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ],
    }
}
fn recovery(at: u64, keys: bool) -> Outcome {
    let neutral_events = if keys {
        vec![
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
        ]
    } else {
        vec![]
    };
    Outcome::Recovery {
        at: ms(at),
        source: source(),
        neutral_events,
        destroy_virtual_and_release_grab: true,
    }
}

pub(crate) fn checked_trace() -> (Config, Vec<Record>) {
    let mut records = vec![
        Record::Header { version: 1 },
        input(context("seat0", 1, 1, 0, State::Desktop), 0),
        input(device("seat0", 1), 0),
        attach(),
    ];
    let mut expected = vec![];
    for (sequence, at, down) in [(1, 10, true), (2, 20, false)] {
        records.extend([
            input(frame("seat0", 1, sequence, at, i32::from(down)), at),
            Record::Poll { at: ms(at) },
            Record::Write {
                at: ms(at),
                source: source(),
                ticket: sequence,
            },
        ]);
        expected.extend([
            Outcome::Offered {
                at: ms(at),
                scheduled: Box::new(Scheduled {
                    ticket: sequence,
                    due: ms(at),
                    frame: key_frame(sequence, at, down),
                }),
            },
            Outcome::Delivered {
                at: ms(at),
                source: source(),
                ticket: sequence,
            },
        ]);
    }
    records.push(Record::Retire {
        at: ms(20),
        source: source(),
    });
    expected.push(recovery(20, false));
    records.push(Record::Expected { outcomes: expected });
    (config(), records)
}
#[test]
fn ordinary_raw_frames_need_exact_write_receipts_and_complete_explicit_retirement() {
    for _ in 0..2 {
        let mut replay = Routed::new(config(), None).unwrap();
        setup(&mut replay);
        let mut expected = vec![];
        for (sequence, at, down) in [(1, 10, true), (2, 20, false)] {
            replay
                .push(input(frame("seat0", 1, sequence, at, i32::from(down)), at))
                .unwrap();
            replay.push(Record::Poll { at: ms(at) }).unwrap();
            replay.push(Record::Poll { at: ms(at) }).unwrap();
            replay
                .push(Record::Write {
                    at: ms(at),
                    source: source(),
                    ticket: sequence,
                })
                .unwrap();
            expected.push(Outcome::Offered {
                at: ms(at),
                scheduled: Box::new(Scheduled {
                    ticket: sequence,
                    due: ms(at),
                    frame: key_frame(sequence, at, down),
                }),
            });
            expected.push(Outcome::Delivered {
                at: ms(at),
                source: source(),
                ticket: sequence,
            });
        }
        replay
            .push(Record::Retire {
                at: ms(20),
                source: source(),
            })
            .unwrap();
        expected.push(recovery(20, false));
        assert_eq!(replay.outcomes(), expected);
        replay
            .push(Record::Expected { outcomes: expected })
            .unwrap();
        replay.finish(true).unwrap();
        assert!(replay.push(Record::Poll { at: ms(20) }).is_err());
    }
}

#[test]
fn wrong_write_and_unretired_transports_fail_closed() {
    let mut pending = Routed::new(config(), None).unwrap();
    setup(&mut pending);
    pending
        .push(input(frame("seat0", 1, 1, 10, 1), 10))
        .unwrap();
    assert!(
        pending
            .push(Record::Write {
                at: ms(10),
                source: source(),
                ticket: 1
            })
            .is_err()
    );
    assert!(pending.baseline.is_none());
    let mut replay = Routed::new(config(), None).unwrap();
    setup(&mut replay);
    assert!(
        replay
            .push(Record::Write {
                at: ms(0),
                source: source(),
                ticket: 1
            })
            .is_err()
    );
    assert!(replay.push(Record::Poll { at: ms(0) }).is_err());
    let mut replay = Routed::new(config(), None).unwrap();
    setup(&mut replay);
    assert!(replay.finish(false).is_err());
    assert!(replay.baseline.is_none());
}

#[test]
fn simulated_readiness_rejects_held_lost_mismatched_and_stale_sources() {
    for violation in ["held", "lost", "consumer", "metadata", "stale", "clock"] {
        let mut replay = Routed::new(config(), None).unwrap();
        for record in [
            Record::Header { version: 1 },
            input(context("seat0", 1, 1, 0, State::Desktop), 0),
            input(device("seat0", 1), 0),
        ] {
            replay.push(record).unwrap();
        }
        let mut admission = attach();
        match violation {
            "held" => {
                replay.push(input(frame("seat0", 1, 1, 0, 1), 0)).unwrap();
            }
            "lost" => {
                let lost = Input::Frame {
                    frame: Frame {
                        source: source(),
                        sequence: 1,
                        at: ms(0),
                        events: vec![
                            RawEvent {
                                kind: EV_SYN,
                                code: crate::input::SYN_DROPPED,
                                value: 0,
                            },
                            RawEvent {
                                kind: EV_SYN,
                                code: SYN_REPORT,
                                value: 0,
                            },
                        ],
                    },
                };
                replay.push(input(lost, 0)).unwrap();
            }
            "consumer" => {
                if let Record::Attach { consumer, .. } = &mut admission {
                    consumer.marker.push_str("wrong");
                }
            }
            "metadata" => {
                if let Record::Attach { descriptor, .. } = &mut admission {
                    descriptor.name.push_str("wrong");
                }
            }
            "stale" => {
                replay
                    .push(input(
                        Input::Remove {
                            source: source(),
                            at: ms(0),
                        },
                        0,
                    ))
                    .unwrap();
            }
            _ => {
                replay.push(Record::Poll { at: ms(1) }).unwrap();
                if let Record::Attach { at, .. } = &mut admission {
                    *at = ms(1);
                }
            }
        }
        assert!(replay.push(admission).is_err(), "{violation}");
        assert!(replay.baseline.is_none());
    }
}

#[test]
fn missing_expectations_bad_headers_and_backwards_dual_clocks_never_pass() {
    assert!(Routed::new(Config::preset("minimal").unwrap(), None).is_err());
    for bad in [Record::Header { version: 2 }, Record::Poll { at: ms(0) }] {
        let mut replay = Routed::new(config(), None).unwrap();
        assert!(replay.push(bad).is_err());
        assert!(replay.finish(false).is_err());
    }
    for bad in [
        Record::Header { version: 1 },
        input(Input::Tick { at: ms(2) }, 1),
        input(Input::Tick { at: ms(0) }, 0),
    ] {
        let mut replay = Routed::new(config(), None).unwrap();
        setup(&mut replay);
        replay.push(input(Input::Tick { at: ms(1) }, 1)).unwrap();
        assert!(replay.push(bad).is_err());
    }
    let mut replay = Routed::new(config(), None).unwrap();
    setup(&mut replay);
    replay
        .push(Record::Retire {
            at: ms(0),
            source: source(),
        })
        .unwrap();
    assert!(replay.finish(true).is_err());
    assert!(replay.baseline.is_none());
}

#[test]
fn retirement_reports_exact_neutralization_without_claiming_a_kernel_release() {
    let mut replay = Routed::new(config(), None).unwrap();
    setup(&mut replay);
    replay.push(input(frame("seat0", 1, 1, 10, 1), 10)).unwrap();
    replay.push(Record::Poll { at: ms(10) }).unwrap();
    replay
        .push(Record::Write {
            at: ms(10),
            source: source(),
            ticket: 1,
        })
        .unwrap();
    replay
        .push(Record::Retire {
            at: ms(11),
            source: source(),
        })
        .unwrap();
    assert_eq!(replay.outcomes().last(), Some(&recovery(11, true)));
    replay.finish(false).unwrap();
}

fn custom() -> Definition {
    Definition::compile(r#"
local p=require('prek')
local function recognize(ticket)
 if ticket.cause=='cancel' then return nil end
 for _,change in ipairs(ticket.input.changes) do
  if change.kind=='key' and change.state=='press' and
   (ticket.recognizer=='first' or ticket.input.sequence=='3') then
   return {phase='discrete',data={step=ticket.recognizer}}
  end
 end
 return {phase='pending'}
end
local first=p.recognizers.define('first',{selectors={'keys'},inputs={'key'},reply_within='100ms'},recognize)
local last=p.recognizers.define('last',{selectors={'keys'},inputs={'key'},reply_within='100ms'},recognize)
return p.config.new{mode='route',policy={allow_input_events=true,allowed_devices={'keys'}},
 timing={join_window='20ms',decision_window='300ms'},devices={keys={keys={'KEY_A'}}},
 rules={{id='chain',devices={'keys'},contexts={'desktop'},scope='everywhere',delivery='buffer',
 trigger=p.triggers.sequence{steps={first,last},timeout='1s'},action=p.actions.event{name='chain'}}}}
"#, "routed.lua").unwrap()
}

#[test]
fn actual_source_bound_composed_callbacks_consume_all_raw_groups_before_one_dispatch() {
    let definition = custom();
    let mut replay = Routed::new(definition.validate().unwrap(), Some(definition)).unwrap();
    setup(&mut replay);
    replay
        .push(Record::Actions {
            at: ms(0),
            seat: "seat0".into(),
            generation: 1,
            capabilities: BTreeSet::from([Capability::Observations]),
        })
        .unwrap();
    for (sequence, at, down) in [(1, 10, 1), (2, 20, 0), (3, 100, 1), (4, 110, 0)] {
        replay
            .push(input(frame("seat0", 1, sequence, at, down), at))
            .unwrap();
        replay.push(Record::Poll { at: ms(at) }).unwrap();
    }
    assert!(
        !replay
            .outcomes()
            .iter()
            .any(|outcome| matches!(outcome, Outcome::Offered { .. } | Outcome::Delivered { .. })),
        "{:?}",
        replay.outcomes()
    );
    let expected = Event::Matched {
        version: 1,
        rule: "chain".into(),
        id: 2,
        ordinal: 1,
        source: source(),
        at: ms(100),
        config_generation: 1,
        session_generation: 1,
        stage: crate::rules::Stage::Discrete,
        action: crate::config::Action::Event {
            name: "chain".into(),
        },
        evidence: crate::rules::Evidence::Sequence {
            steps: ["first", "last"]
                .into_iter()
                .map(|name| crate::rules::Evidence::Custom {
                    recognizer: name.into(),
                    data: serde_json::json!({"step":name}),
                })
                .collect(),
        },
        output: None,
    };
    assert_eq!(
        replay.outcomes(),
        [Outcome::Dispatch {
            at: ms(100),
            event: Box::new(expected.clone())
        }]
    );
    replay
        .push(Record::Retire {
            at: ms(110),
            source: source(),
        })
        .unwrap();
    replay
        .push(Record::Expected {
            outcomes: vec![
                Outcome::Dispatch {
                    at: ms(100),
                    event: Box::new(expected),
                },
                recovery(110, false),
            ],
        })
        .unwrap();
    replay.finish(true).unwrap();
}

#[test]
fn buffered_fallback_preserves_frame_order_and_original_intervals() {
    let mut config = config();
    let mut rule = custom().validate().unwrap().rules.remove(0);
    rule.trigger = crate::config::Trigger::ButtonTaps {
        key: crate::config::Key(30),
        count: 2,
    };
    config.rules = vec![rule];
    config.timing.join_window = crate::config::Millis(20);
    config.timing.decision_window = crate::config::Millis(300);
    let mut replay = Routed::new(config, None).unwrap();
    setup(&mut replay);
    replay
        .push(Record::Actions {
            at: ms(0),
            seat: "seat0".into(),
            generation: 1,
            capabilities: BTreeSet::from([Capability::Observations]),
        })
        .unwrap();
    replay.push(input(frame("seat0", 1, 1, 10, 1), 10)).unwrap();
    replay.push(input(frame("seat0", 1, 2, 20, 0), 20)).unwrap();
    replay.push(Record::Poll { at: ms(20) }).unwrap();
    assert!(replay.outcomes().is_empty());
    replay
        .push(input(context("seat0", 1, 2, 200, State::Desktop), 200))
        .unwrap();
    for (sequence, at, down) in [(1, 310, true), (2, 320, false)] {
        replay.push(input(Input::Tick { at: ms(at) }, at)).unwrap();
        replay.push(Record::Poll { at: ms(at) }).unwrap();
        assert_eq!(
            replay
                .outcomes()
                .iter()
                .filter_map(
                    |outcome| if let Outcome::Offered { scheduled, .. } = outcome {
                        Some(scheduled.as_ref())
                    } else {
                        None
                    }
                )
                .next_back(),
            Some(&Scheduled {
                ticket: sequence,
                due: ms(at),
                frame: key_frame(sequence, if down { 10 } else { 20 }, down)
            })
        );
        replay
            .push(Record::Write {
                at: ms(at),
                source: source(),
                ticket: sequence,
            })
            .unwrap();
    }
    assert!(
        !replay
            .outcomes()
            .iter()
            .any(|outcome| matches!(outcome, Outcome::Dispatch { .. }))
    );
    replay
        .push(Record::Retire {
            at: ms(320),
            source: source(),
        })
        .unwrap();
    replay.finish(false).unwrap();
}

#[test]
fn wall_only_polls_preserve_the_drained_input_watermark_and_recover_missed_output() {
    let mut replay = Routed::new(config(), None).unwrap();
    setup(&mut replay);
    replay.push(input(frame("seat0", 1, 1, 10, 1), 10)).unwrap();
    replay.push(Record::Poll { at: ms(15) }).unwrap();
    replay
        .push(Record::Write {
            at: ms(15),
            source: source(),
            ticket: 1,
        })
        .unwrap();
    assert_eq!(replay.at, ms(10));
    replay.push(input(frame("seat0", 1, 2, 12, 0), 16)).unwrap();
    replay.push(Record::Poll { at: ms(16) }).unwrap();
    replay
        .push(Record::Write {
            at: ms(16),
            source: source(),
            ticket: 2,
        })
        .unwrap();
    assert_eq!(replay.at, ms(12));
    replay
        .push(Record::Retire {
            at: ms(16),
            source: source(),
        })
        .unwrap();
    replay.finish(false).unwrap();
    let mut replay = Routed::new(config(), None).unwrap();
    setup(&mut replay);
    replay.push(input(frame("seat0", 1, 1, 10, 1), 10)).unwrap();
    assert!(replay.push(Record::Poll { at: ms(100) }).is_err());
    assert!(replay.baseline.is_none());
    assert!(replay.finish(false).is_err());
}

#[test]
fn secure_context_and_new_session_retire_pending_writes_before_they_can_be_acknowledged() {
    for generation in [1, 2] {
        let mut replay = Routed::new(config(), None).unwrap();
        setup(&mut replay);
        replay.push(input(frame("seat0", 1, 1, 10, 1), 10)).unwrap();
        replay.push(Record::Poll { at: ms(10) }).unwrap();
        replay
            .push(input(
                context("seat0", generation, 2, 11, State::Locked),
                11,
            ))
            .unwrap();
        assert!(replay.in_flight.is_empty());
        assert!(replay.attached.is_empty());
        assert_eq!(replay.outcomes().last(), Some(&recovery(11, false)));
        replay.finish(false).unwrap();
    }
}

#[test]
fn routed_recording_limits_and_expectations_are_strict_and_do_not_accept_generic_records() {
    let mut replay = Routed::new(config(), None).unwrap();
    assert!(
        replay
            .read(&mut io::Cursor::new(br#"{"record":"header","version":1}"#))
            .is_err()
    );
    assert!(replay.baseline.is_none());
    let mut replay = Routed::new(config(), None).unwrap();
    setup(&mut replay);
    replay
        .push(Record::Retire {
            at: ms(0),
            source: source(),
        })
        .unwrap();
    replay.push(Record::Expected { outcomes: vec![] }).unwrap();
    assert!(replay.finish(true).is_err());
    let mut replay = Routed::new(config(), None).unwrap();
    setup(&mut replay);
    replay.records = MAX_RECORDS;
    assert!(replay.push(Record::Poll { at: ms(0) }).is_err());
    let mut replay = Routed::new(config(), None).unwrap();
    setup(&mut replay);
    replay.outcome_bytes = MAX_RECORDING_BYTES;
    assert!(
        replay
            .push(Record::Retire {
                at: ms(0),
                source: source()
            })
            .is_err()
    );
}

#[test]
fn held_touch_nested_callbacks_consume_the_whole_stream_on_a_recorded_timer() {
    use crate::{
        gestures::{Point, Space, TouchGesture},
        input::{ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, EV_ABS},
        replay::configured::tests::composition::{
            output, raw, source as touch_source, touch_context, touch_device,
        },
        rules::{Evidence, Stage},
    };
    let source_text = touch_source(true)
        .source
        .replace("return p.config.new{", "return p.config.new{mode='route',")
        .replace("delivery='observe'", "delivery='buffer'");
    let definition = Definition::compile(&source_text, "routed-touch.lua").unwrap();
    let mut replay = Routed::new(definition.validate().unwrap(), Some(definition)).unwrap();
    let mut device = touch_device();
    device.properties = vec![1];
    device
        .capabilities
        .get_mut(&EV_ABS)
        .unwrap()
        .sort_unstable();
    device.axes.insert(
        ABS_MT_TRACKING_ID,
        crate::input::Axis {
            minimum: 0,
            maximum: 65535,
            resolution: 0,
            fuzz: 0,
            flat: 0,
        },
    );
    let source = device.source.clone();
    let descriptor = Descriptor::mirror(
        &device,
        KernelIdentity {
            bus: 3,
            vendor: 1,
            product: 1,
            version: 1,
        },
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    let consumer = ConsumerEvidence {
        consumer: Consumer::IsolatedReceiver { pid: 1, start: 1 },
        marker: descriptor.marker.clone(),
        name: descriptor.name.clone(),
        class: descriptor.class,
        seat: "seat0".into(),
        at: ms(0),
    };
    let mut mapping = Mapping::from_device(&device).unwrap();
    mapping.output = Some(output());
    mapping.generation = 2;
    for record in [
        Record::Header { version: 1 },
        input(touch_context(0, 1), 0),
        input(
            Input::Device {
                device,
                keys: BTreeSet::new(),
                contacts: vec![],
            },
            0,
        ),
        input(
            Input::Mapping {
                source: source.clone(),
                at: ms(0),
                mapping,
            },
            0,
        ),
        Record::Attach {
            at: ms(0),
            descriptor: Box::new(descriptor),
            consumer,
            feedback: false,
        },
        Record::Actions {
            at: ms(0),
            seat: "seat0".into(),
            generation: 1,
            capabilities: BTreeSet::from([Capability::Observations]),
        },
        input(
            raw(
                1,
                0,
                &[
                    (EV_ABS, ABS_MT_SLOT, 0),
                    (EV_ABS, ABS_MT_TRACKING_ID, 10),
                    (EV_ABS, ABS_MT_POSITION_X, 100),
                    (EV_ABS, ABS_MT_POSITION_Y, 500),
                    (EV_ABS, ABS_MT_SLOT, 1),
                    (EV_ABS, ABS_MT_TRACKING_ID, 11),
                    (EV_ABS, ABS_MT_POSITION_X, 200),
                    (EV_ABS, ABS_MT_POSITION_Y, 500),
                ],
            ),
            0,
        ),
        input(Input::Tick { at: ms(20) }, 20),
        Record::Poll { at: ms(20) },
        input(
            raw(
                2,
                30,
                &[
                    (EV_ABS, ABS_MT_SLOT, 0),
                    (EV_ABS, ABS_MT_TRACKING_ID, -1),
                    (EV_ABS, ABS_MT_SLOT, 1),
                    (EV_ABS, ABS_MT_TRACKING_ID, -1),
                ],
            ),
            30,
        ),
        Record::Poll { at: ms(30) },
        Record::Retire {
            at: ms(30),
            source: source.clone(),
        },
    ] {
        replay
            .push(serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap())
            .unwrap();
    }
    let expected = Event::Matched {
        version: 1,
        rule: "chain".into(),
        id: 1,
        ordinal: 1,
        source: source.clone(),
        at: ms(20),
        config_generation: 1,
        session_generation: 1,
        stage: Stage::Discrete,
        action: crate::config::Action::Event {
            name: "chain".into(),
        },
        evidence: Evidence::Sequence {
            steps: vec![
                Evidence::Touch {
                    space: Space::Logical,
                    gesture: TouchGesture::Hold {
                        fingers: 2,
                        position: Point { x: 150.0, y: 500.0 },
                    },
                },
                Evidence::Custom {
                    recognizer: "first".into(),
                    data: serde_json::json!({"step":"first"}),
                },
                Evidence::Custom {
                    recognizer: "last".into(),
                    data: serde_json::json!({"step":"last"}),
                },
            ],
        },
        output: Some("replay-output".into()),
    };
    replay
        .push(Record::Expected {
            outcomes: vec![
                Outcome::Dispatch {
                    at: ms(20),
                    event: Box::new(expected),
                },
                Outcome::Recovery {
                    at: ms(30),
                    source,
                    neutral_events: vec![],
                    destroy_virtual_and_release_grab: true,
                },
            ],
        })
        .unwrap();
    replay.finish(true).unwrap();
}

#[test]
fn callback_failure_releases_shared_vm_leases_and_approval_cannot_be_forged() {
    let definition = custom();
    assert!(Routed::new(definition.validate().unwrap(), None).is_err());
    let mut forged = definition.clone();
    forged.recognizers.pop();
    assert!(Routed::new(definition.validate().unwrap(), Some(forged)).is_err());
    let failed = Definition::compile(
        &definition.source.replace(
            "local function recognize(ticket)",
            "local function recognize(ticket) error('intentional callback failure')",
        ),
        "failed-routing.lua",
    )
    .unwrap();
    let budget = Budget::default();
    for _ in 0..5 {
        let mut replay = Routed::with_budget(
            failed.validate().unwrap(),
            Some(Arc::new(failed.clone())),
            budget.clone(),
        )
        .unwrap();
        setup(&mut replay);
        replay
            .push(Record::Actions {
                at: ms(0),
                seat: "seat0".into(),
                generation: 1,
                capabilities: BTreeSet::from([Capability::Observations]),
            })
            .unwrap();
        assert!(
            replay
                .push(input(frame("seat0", 1, 1, 10, 1), 10))
                .unwrap_err()
                .to_string()
                .contains("intentional callback failure")
        );
        assert!(replay.baseline.is_none());
    }
}

#[test]
fn original_removal_after_transport_retirement_clears_the_native_input_catalog() {
    let mut replay = Routed::new(config(), None).unwrap();
    setup(&mut replay);
    replay.push(input(frame("seat0", 1, 1, 10, 1), 10)).unwrap();
    replay.push(Record::Poll { at: ms(10) }).unwrap();
    replay
        .push(Record::Write {
            at: ms(10),
            source: source(),
            ticket: 1,
        })
        .unwrap();
    replay
        .push(Record::Retire {
            at: ms(11),
            source: source(),
        })
        .unwrap();
    assert!(replay.seats["seat0"].engine.has_input_source(&source()));
    replay
        .push(input(
            Input::Remove {
                source: source(),
                at: ms(12),
            },
            12,
        ))
        .unwrap();
    assert!(!replay.seats["seat0"].engine.has_input_source(&source()));
    assert_eq!(replay.outcomes().last(), Some(&recovery(11, true)));
    replay.finish(false).unwrap();
}
