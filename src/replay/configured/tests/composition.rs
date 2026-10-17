use super::*;
use crate::{
    config::Screen,
    context::OutputState,
    gestures::{Mapping, Output, Point, Space, TouchGesture, Transform},
    input::*,
};

fn source(touch: bool) -> Definition {
    let kind = if touch { "touch" } else { "key" };
    let steps = if touch {
        "p.triggers.touch_hold{fingers=2,after='20ms'},p.triggers.sequence{steps={first,last},timeout='1s'}"
    } else {
        "first,last"
    };
    Definition::compile(&format!(r#"
local p=require('prek');local calls=0
local function recognize(ticket)
 if ticket.cause=='cancel' then assert(ticket.input==nil);return nil end
 calls=calls+1
 if ticket.input.contacts and #ticket.input.contacts>0 then
  assert(ticket.cause=='timer' and ticket.input.sequence=='1' and #ticket.input.changes==0)
  assert(#ticket.input.contacts_started==0 and #ticket.input.contacts_ended==0)
  assert(#ticket.input.contacts==2 and not ticket.input.contacts[1].fresh and not ticket.input.contacts[2].fresh)
 end
 return {{phase='discrete',data={{step=ticket.recognizer}}}}
end
local first=p.recognizers.define('first',{{selectors={{'keys'}},inputs={{{kind:?}}},reply_within='100ms'}},recognize)
local last=p.recognizers.define('last',{{selectors={{'keys'}},inputs={{{kind:?}}},reply_within='100ms'}},recognize)
return p.config.new{{policy={{allow_input_events=true,allowed_devices={{'keys'}}}},devices={{keys={{keys={{'KEY_A'}}}}}},
 timing={{join_window='20ms',decision_window='300ms'}},
 rules={{{{id='chain',devices={{'keys'}},contexts={{'desktop'}},scope='everywhere',delivery='observe',trigger=p.triggers.sequence{{steps={{{steps}}},timeout='1s'}},action=p.actions.event{{name='chain'}}}}}}}}
"#),"composed-replay.lua").unwrap()
}
fn touch_device() -> Device {
    let Record::Device { mut device, .. } = device("seat0", 1) else {
        unreachable!()
    };
    device.capabilities.insert(
        EV_ABS,
        vec![
            ABS_MT_SLOT,
            ABS_MT_TRACKING_ID,
            ABS_MT_POSITION_X,
            ABS_MT_POSITION_Y,
        ],
    );
    device.axes = [
        (ABS_MT_SLOT, 4, 0),
        (ABS_MT_POSITION_X, 1000, 10),
        (ABS_MT_POSITION_Y, 1000, 10),
    ]
    .into_iter()
    .map(|(code, maximum, resolution)| {
        (
            code,
            Axis {
                minimum: 0,
                maximum,
                resolution,
                fuzz: 0,
                flat: 0,
            },
        )
    })
    .collect();
    device
}
fn output() -> Output {
    Output {
        name: "replay-output".into(),
        pixel_width: 1000.0,
        pixel_height: 1000.0,
        scale: 1.0,
        origin: Point::default(),
        transform: Transform::Normal,
    }
}
fn touch_context(at: u64, revision: u64) -> Record {
    let Record::Context {
        at,
        binding,
        mut snapshot,
    } = context("seat0", 1, revision, at, State::Desktop)
    else {
        unreachable!()
    };
    snapshot.outputs.push(OutputState {
        geometry: output(),
        geometry_generation: 2,
        active_workspace: Some(1),
        screen: Screen::On,
        keyboard_known: true,
        keyboard: None,
    });
    Record::Context {
        at,
        binding,
        snapshot,
    }
}
fn raw(sequence: u64, at: u64, values: &[(u16, u16, i32)]) -> Record {
    let mut events: Vec<_> = values
        .iter()
        .map(|(kind, code, value)| RawEvent {
            kind: *kind,
            code: *code,
            value: *value,
        })
        .collect();
    events.push(RawEvent {
        kind: EV_SYN,
        code: SYN_REPORT,
        value: 0,
    });
    Record::Frame {
        frame: Frame {
            source: touch_device().source,
            sequence,
            at: ms(at),
            events,
        },
    }
}
fn setup_touch(replay: &mut Configured) {
    let device = touch_device();
    let mut mapping = Mapping::from_device(&device).unwrap();
    mapping.output = Some(output());
    mapping.generation = 2;
    for record in [
        Record::Header { version: 1 },
        touch_context(0, 1),
        Record::Device {
            device: device.clone(),
            keys: BTreeSet::new(),
            contacts: vec![],
        },
        Record::Mapping {
            source: device.source,
            at: ms(0),
            mapping,
        },
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
    ] {
        replay.push(record).unwrap();
    }
}

#[test]
fn held_touch_nested_steps_run_real_timer_callbacks_without_a_second_frame() {
    let definition = source(true);
    let config = definition.validate().unwrap();
    let mut replay = Configured::new(config, Some(definition)).unwrap();
    setup_touch(&mut replay);
    assert!(matched(&replay).is_empty());
    replay.push(Record::Tick { at: ms(20) }).unwrap();
    let expected = Event::Matched {
        version: 1,
        rule: "chain".into(),
        id: 1,
        ordinal: 1,
        source: touch_device().source,
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
        window: None,
        output: Some("replay-output".into()),
        workspace: Some(1),
    };
    assert_eq!(replay.events(), std::slice::from_ref(&expected));
    replay
        .push(Record::ExpectedRules {
            events: vec![expected],
        })
        .unwrap();
    replay.finish(true).unwrap();
}

#[test]
fn cross_source_composition_keeps_native_evidence_and_cancels_a_retired_prefix() {
    for remove_prefix in [false, true] {
        let definition = source(false);
        let config = definition.validate().unwrap();
        let mut replay = Configured::new(config, Some(definition)).unwrap();
        start(&mut replay, 1);
        replay.push(device("seat0", 2)).unwrap();
        replay.push(frame("seat0", 1, 1, 10, 1)).unwrap();
        assert!(matched(&replay).is_empty());
        if remove_prefix {
            replay
                .push(Record::Remove {
                    source: Source {
                        device: 1,
                        generation: u64::MAX,
                        seat: "seat0".into(),
                    },
                    at: ms(11),
                })
                .unwrap();
        }
        replay.push(frame("seat0", 2, 1, 20, 1)).unwrap();
        assert_eq!(matched(&replay).len(), usize::from(!remove_prefix));
        if !remove_prefix {
            assert!(
                matches!(matched(&replay)[0],Event::Matched {evidence:Evidence::Sequence {steps},..}if steps.len()==2)
            );
        }
        replay.finish(false).unwrap();
    }
}

#[test]
fn changed_geometry_retires_the_frozen_touch_parent_before_any_later_callback() {
    let original = source(true);
    let pending = original.source.replace(
        "return {phase='discrete',data={step=ticket.recognizer}}",
        "return nil",
    );
    assert_ne!(pending, original.source);
    let definition = Definition::compile(&pending, "pending-touch.lua").unwrap();
    let mut replay = Configured::new(definition.validate().unwrap(), Some(definition)).unwrap();
    setup_touch(&mut replay);
    replay.push(Record::Tick { at: ms(20) }).unwrap();
    let mut changed = touch_context(21, 2);
    let Record::Context { snapshot, .. } = &mut changed else {
        unreachable!()
    };
    snapshot.outputs[0].geometry.origin.x = 100.0;
    snapshot.outputs[0].geometry_generation += 1;
    replay.push(changed).unwrap();
    assert!(matched(&replay).is_empty());
    assert!(
        replay
            .events()
            .iter()
            .any(|e| matches!(e, Event::Cancelled { .. }))
    );
    replay.finish(false).unwrap();
}
