//! Explicit one-device learning, using the configured shape engine. Completed
//! trajectories are retained only by this diagnostic API, never ordinary runs.

use crate::{
    config::{Action, Config, Key, Mode, Selector, ShapeTemplate, Trigger},
    paths::{Stroke, demo_templates},
    replay::Record,
    rules::{Engine, Event},
};
use std::{io, time::Duration};

pub struct Learner {
    engine: Engine,
    name: String,
    captured: Option<Stroke>,
    config: Config,
    device_path: String,
    selected: bool,
    header: bool,
}
impl Learner {
    pub fn new(
        name: String,
        device_path: String,
        fingers: Option<u16>,
        button: Key,
    ) -> io::Result<Self> {
        let mut config = Config::preset("desktop").map_err(io::Error::other)?;
        config.mode = Mode::Observe;
        config.shapes = vec![demo_templates().remove(0)];
        config.devices.clear();
        config.devices.insert(
            "capture".into(),
            Selector {
                path: Some(device_path.clone()),
                ..Selector::default()
            },
        );
        config.policy.allowed_devices = vec!["capture".into()];
        let rule = &mut config.rules[0];
        rule.id = "learn-shape".into();
        rule.enabled = true;
        rule.devices = vec!["capture".into()];
        rule.action = Action::Event {
            name: "diagnostic_shape".into(),
        };
        rule.trigger = Trigger::Shape {
            name: "v".into(),
            fingers,
            button: fingers.is_none().then_some(button),
        };
        rule.require_keys = if fingers.is_none() {
            vec![button]
        } else {
            Vec::new()
        };
        rule.keys_throughout = true;
        // Validate the requested export name before any reader or output opens.
        let mut template = config.shapes[0].clone();
        template.name = name.clone();
        crate::paths::validate_template(&template).map_err(io::Error::other)?;
        Ok(Self {
            engine: Engine::new_capture(config.clone(), "learn-shape")?,
            name,
            captured: None,
            config,
            device_path,
            selected: false,
            header: false,
        })
    }
    pub fn push(&mut self, record: Record) -> io::Result<()> {
        if self.captured.is_some() {
            return Err(io::Error::other("learning already completed"));
        }
        if !self.header {
            if !matches!(
                record,
                Record::Header {
                    version: crate::replay::FORMAT_VERSION
                }
            ) {
                return Err(io::Error::other(
                    "learning input requires its versioned recording header first",
                ));
            }
            self.header = true;
        }
        if let Record::Device { device, .. } = &record
            && device.path == self.device_path
        {
            if device.virtual_device {
                return Err(io::Error::other(
                    "learning requires a selected original input source",
                ));
            }
            match self.config.rules[0].trigger {
                Trigger::Shape {
                    fingers: Some(fingers),
                    ..
                } if device
                    .touch_capacity()
                    .is_none_or(|capacity| capacity < usize::from(fingers))
                    || crate::gestures::Mapping::from_device(device).is_err() =>
                {
                    return Err(io::Error::other(
                        "selected source lacks the required multi-touch slots",
                    ));
                }
                Trigger::Shape {
                    fingers: None,
                    button,
                    ..
                } if !device.has(crate::input::EV_REL, 0)
                    || !device.has(crate::input::EV_REL, 1)
                    || !device.has(crate::input::EV_KEY, button.unwrap().0) =>
                {
                    return Err(io::Error::other(
                        "selected source lacks relative X/Y or its capture button",
                    ));
                }
                _ => {}
            }
            if !self.selected {
                self.config.policy.allowed_seats = vec![device.source.seat.clone()];
                self.engine = Engine::new_capture(self.config.clone(), "learn-shape")?;
                self.engine.push(Record::Header {
                    version: crate::replay::FORMAT_VERSION,
                })?;
                self.selected = true;
            }
        }
        self.engine.push(record)?;
        self.captured = self.engine.take_strokes().into_iter().next();
        Ok(())
    }
    pub fn deadline(&self) -> Option<Duration> {
        self.engine.deadline()
    }
    pub fn finished(&self) -> bool {
        self.captured.is_some()
    }
    pub fn take_events(&mut self) -> Vec<Event> {
        self.engine.take_events()
    }
    pub fn stroke(&self) -> Option<&Stroke> {
        self.captured.as_ref()
    }
    /// A standalone schema-1 template file with no selectors or active rules.
    pub fn export(&self) -> io::Result<Config> {
        let stroke = self
            .captured
            .as_ref()
            .ok_or_else(|| io::Error::other("no complete valid stroke was captured"))?;
        let template: ShapeTemplate = stroke
            .template(self.name.clone(), 0.94, 0.04)
            .map_err(io::Error::other)?;
        let mut config = Config::preset("minimal").map_err(io::Error::other)?;
        config.shapes = vec![template];
        config.validate().map_err(io::Error::other)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        input::{Device, EV_KEY, EV_REL, EV_SYN, Frame, RawEvent, SYN_REPORT, Source},
        paths::{MatchOutcome, Matcher},
    };
    use std::collections::{BTreeMap, BTreeSet};
    fn device(path: &str) -> Device {
        Device {
            identity: path.into(),
            source: Source {
                device: if path == "chosen" { 1 } else { 2 },
                generation: 1,
                seat: "seat9".into(),
            },
            name: path.into(),
            path: path.into(),
            physical: path.into(),
            unique: String::new(),
            vendor: 1,
            product: 1,
            properties: vec![],
            axes: BTreeMap::new(),
            capabilities: BTreeMap::from([(EV_KEY, vec![0x110]), (EV_REL, vec![0, 1])]),
            virtual_device: false,
        }
    }
    fn learner(held: bool) -> Learner {
        let mut learner = Learner::new("my-L".into(), "chosen".into(), None, Key(0x110)).unwrap();
        learner.push(Record::Header { version: 1 }).unwrap();
        learner
            .push(Record::Device {
                device: device("chosen"),
                keys: if held {
                    BTreeSet::from([0x110])
                } else {
                    BTreeSet::new()
                },
                contacts: vec![],
            })
            .unwrap();
        learner
    }
    fn frame(
        learner: &mut Learner,
        path: &str,
        at: u64,
        sequence: u64,
        values: &[(u16, u16, i32)],
    ) {
        let mut events: Vec<_> = values
            .iter()
            .map(|&(kind, code, value)| RawEvent { kind, code, value })
            .collect();
        events.push(RawEvent {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        });
        learner
            .push(Record::Frame {
                frame: Frame {
                    source: device(path).source,
                    at: Duration::from_millis(at),
                    sequence,
                    events,
                },
            })
            .unwrap();
        learner.take_events();
    }
    #[test]
    fn t35_unknown_shape_can_be_learned_and_reused_without_an_action_binding() {
        let mut learner = learner(false);
        frame(&mut learner, "chosen", 10, 1, &[(EV_KEY, 0x110, 1)]);
        frame(&mut learner, "chosen", 20, 2, &[(EV_REL, 0, 100)]);
        frame(
            &mut learner,
            "chosen",
            30,
            3,
            &[(EV_REL, 1, 100), (EV_KEY, 0x110, 0)],
        );
        let exported = learner.export().unwrap();
        assert!(exported.rules.is_empty() && exported.devices.is_empty());
        assert_eq!(exported.shapes[0].points.len(), 64);
        assert!(
            matches!(Matcher::new(&exported.shapes).unwrap().classify(learner.stroke().unwrap()),MatchOutcome::Matched {shape} if shape.name == "my-L")
        );
    }
    #[test]
    fn t35_source_loss_and_tiny_strokes_cannot_export_a_template() {
        let mut learner = learner(false);
        frame(&mut learner, "chosen", 10, 1, &[(EV_KEY, 0x110, 1)]);
        frame(
            &mut learner,
            "chosen",
            20,
            2,
            &[(EV_REL, 0, 1), (EV_KEY, 0x110, 0)],
        );
        assert!(learner.export().is_err());
        frame(&mut learner, "chosen", 30, 3, &[(EV_KEY, 0x110, 1)]);
        frame(&mut learner, "chosen", 40, 4, &[(EV_REL, 0, 100)]);
        learner
            .push(Record::Remove {
                source: device("chosen").source,
                at: Duration::from_millis(50),
            })
            .unwrap();
        assert!(!learner.finished());
        assert!(learner.export().is_err());
    }
    #[test]
    fn learning_does_not_invent_a_missing_recording_header() {
        let mut learner = Learner::new("my-L".into(), "chosen".into(), None, Key(0x110)).unwrap();
        assert!(
            learner
                .push(Record::Device {
                    device: device("chosen"),
                    keys: BTreeSet::new(),
                    contacts: vec![]
                })
                .is_err()
        );
        assert!(learner.export().is_err());
    }
    #[test]
    fn t35_selection_and_initial_held_state_wait_for_a_fresh_capture() {
        let mut learner = learner(true);
        frame(
            &mut learner,
            "chosen",
            10,
            1,
            &[(EV_REL, 0, 100), (EV_REL, 1, 100), (EV_KEY, 0x110, 0)],
        );
        assert!(!learner.finished());
        learner
            .push(Record::Device {
                device: device("other"),
                keys: BTreeSet::new(),
                contacts: vec![],
            })
            .unwrap();
        frame(&mut learner, "other", 20, 1, &[(EV_KEY, 0x110, 1)]);
        frame(
            &mut learner,
            "other",
            30,
            2,
            &[(EV_REL, 0, 100), (EV_REL, 1, 100), (EV_KEY, 0x110, 0)],
        );
        assert!(learner.export().is_err());
        frame(&mut learner, "chosen", 40, 2, &[(EV_KEY, 0x110, 1)]);
        frame(&mut learner, "chosen", 50, 3, &[(EV_REL, 0, 100)]);
        frame(
            &mut learner,
            "chosen",
            60,
            4,
            &[(EV_REL, 1, 100), (EV_KEY, 0x110, 0)],
        );
        assert!(learner.finished());
    }
}
