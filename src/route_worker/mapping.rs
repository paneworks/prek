//! Touch device mappings onto the session's verified output geometry.
use super::*;
use crate::gestures::Mapping;

/// The output name and geometry generation a mapping targets.
pub(super) fn target(mapping: &Mapping) -> Option<(String, u64)> {
    mapping
        .output
        .as_ref()
        .map(|output| (output.name.clone(), mapping.generation))
}

impl Stream {
    /// Pushes a new mapping for each routed touch source of a seat whose
    /// target output or its geometry changed.
    pub(super) fn remap(&mut self, name: &str, now: Duration) -> io::Result<()> {
        let Some(seat) = self.seats.get_mut(name) else {
            return Ok(());
        };
        let sources: Vec<_> = self
            .mapped
            .keys()
            .filter(|source| source.seat == name)
            .cloned()
            .collect();
        for source in sources {
            let Some(device) = seat.routing.device(&source) else {
                self.mapped.remove(&source);
                continue;
            };
            let Some(mapping) = seat.subscription.touch_mapping(&self.config, device) else {
                continue;
            };
            let current = target(&mapping);
            if self.mapped.get(&source) != Some(&current) {
                self.mapped.insert(source.clone(), current);
                seat.routing.push(Record::Mapping {
                    source,
                    at: now,
                    mapping,
                })?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Selector,
        context::{OutputState, Snapshot, State},
        gestures::{Output, Point, Transform},
        worker::ProvidedContext,
    };
    fn output(name: &str, generation: u64) -> OutputState {
        OutputState {
            geometry: Output {
                name: name.into(),
                pixel_width: 1116.0,
                pixel_height: 2484.0,
                scale: 1.5,
                origin: Point { x: 0.0, y: 0.0 },
                transform: Transform::Normal,
            },
            geometry_generation: generation,
            screen: crate::config::Screen::On,
        }
    }
    fn subscription(outputs: Vec<OutputState>) -> Subscription {
        Subscription {
            seat: "seat0".into(),
            fence: Fence {
                broker: crate::protocol::Epoch([1, 2]),
                configuration: 1,
                session: 1,
            },
            context: Some(Box::new(ProvidedContext {
                binding: crate::context::Binding {
                    seat: "seat0".into(),
                    session: "model".into(),
                    uid: 1000,
                    compositor_pid: 42,
                    compositor_start: 123,
                    kind: crate::context::SessionKind::User,
                },
                snapshot: Snapshot {
                    generation: 1,
                    revision: 1,
                    captured_at: Duration::ZERO,
                    state: State::Desktop,
                    outputs,
                },
            })),
            capabilities: BTreeSet::new(),
            consumer: None,
        }
    }
    fn config(output: Option<&str>) -> Config {
        let mut config = Config::preset("phone").unwrap();
        config.devices = BTreeMap::from([(
            "touch".into(),
            Selector {
                identity: Some(crate::transport::tests::touch().identity),
                output: output.map(str::to_owned),
                ..Selector::default()
            },
        )]);
        config
    }
    #[test]
    fn a_touch_device_maps_to_its_named_output_or_the_only_one() {
        let touch = crate::transport::tests::touch();
        let only = subscription(vec![output("DSI-1", 4)]);
        let mapped = only.touch_mapping(&config(None), &touch).unwrap();
        assert_eq!(target(&mapped), Some(("DSI-1".into(), 4)));
        assert_eq!(mapped.output.unwrap().scale, 1.5);
        let two = subscription(vec![output("DSI-1", 4), output("HDMI-A-1", 2)]);
        assert_eq!(
            target(&two.touch_mapping(&config(None), &touch).unwrap()),
            None
        );
        assert_eq!(
            target(
                &two.touch_mapping(&config(Some("HDMI-A-1")), &touch)
                    .unwrap()
            ),
            Some(("HDMI-A-1".into(), 2))
        );
        assert_eq!(
            target(&only.touch_mapping(&config(Some("eDP-1")), &touch).unwrap()),
            None
        );
        let mut axisless = touch;
        axisless.axes.remove(&crate::input::ABS_MT_POSITION_X);
        assert!(only.touch_mapping(&config(None), &axisless).is_none());
    }
}
