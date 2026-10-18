use super::*;
use crate::config::EffectKind;

fn keyboard(code: u16) -> bool {
    matches!(code, 1..=0xff | 0x160..=0x21f | 0x224..=0x2bf | 0x2e8..=0x2ff)
}

#[test]
fn domain_key_codes_match_the_generated_keyboard_shape() {
    let descriptor = crate::transport::Descriptor::generated_keyboard(
        Source {
            device: 100,
            generation: 1,
            seat: "seat0".into(),
        },
        BTreeMap::new(),
    )
    .unwrap();
    for code in 0..=1023 {
        assert_eq!(
            keyboard(code),
            descriptor.capabilities[&crate::input::EV_KEY].contains(&code),
            "code={code}"
        );
    }
}
fn root(parents: &[usize], mut index: usize) -> usize {
    while parents[index] != index {
        index = parents[index];
    }
    index
}
fn join(parents: &mut [usize], a: usize, b: usize) {
    let a = root(parents, a);
    let b = root(parents, b);
    parents[b] = a;
}

impl Routing {
    pub(crate) fn key_orders_source(&self, source: &Source) -> bool {
        !self.ordered_keys || self.routes.get(source).is_none_or(|route| route.key_domain)
    }

    pub(in crate::routing) fn refresh_key_domains(&mut self) {
        if !self.ordered_keys {
            return;
        }
        let devices: Vec<_> = self
            .routes
            .values()
            .map(|route| route.router.device())
            .collect();
        let mut parents: Vec<_> = (0..devices.len()).collect();
        let mut seeds: Vec<_> = devices
            .iter()
            .map(|device| {
                device
                    .capabilities
                    .get(&crate::input::EV_KEY)
                    .is_some_and(|codes| codes.iter().any(|code| keyboard(*code)))
            })
            .collect();
        let config = self.engine.config();
        for rule in config
            .rules
            .iter()
            .filter(|rule| rule.enabled && config.mode_active(rule))
        {
            let selected: Vec<_> = devices
                .iter()
                .enumerate()
                .filter(|(_, device)| {
                    (rule.seats.is_empty() || rule.seats.contains(&device.source.seat))
                        && config.devices.iter().any(|(id, selector)| {
                            (rule.devices.is_empty() || rule.devices.contains(id))
                                && selector.matches(device)
                        })
                })
                .map(|(index, _)| index)
                .collect();
            let generated = rule.action.effect_kind() == Some(EffectKind::SyntheticInput)
                || matches!(&rule.action, Action::Lua { requires, .. } if requires.contains(&EffectKind::SyntheticInput));
            if generated {
                for &index in &selected {
                    seeds[index] = true;
                }
            }
            let keys: Vec<_> = rule
                .require_keys
                .iter()
                .map(|key| key.0)
                .chain(
                    rule.layer
                        .as_ref()
                        .and_then(|id| config.layers.iter().find(|layer| &layer.id == id))
                        .map(|layer| layer.key.0),
                )
                .collect();
            let compound = !keys.is_empty()
                || matches!(
                    rule.trigger,
                    Trigger::Sequence { .. } | Trigger::Chord { .. }
                )
                || rule.trigger.recognizers().next().is_some();
            if !compound {
                continue;
            }
            let mut seats = BTreeMap::new();
            for &index in &selected {
                if let Some(&first) = seats.get(&devices[index].source.seat) {
                    join(&mut parents, first, index);
                } else {
                    seats.insert(devices[index].source.seat.clone(), index);
                }
            }
            for (index, device) in devices.iter().enumerate() {
                if let Some(&first) = seats.get(&device.source.seat)
                    && (keys
                        .iter()
                        .any(|code| device.has(crate::input::EV_KEY, *code))
                        || rule.trigger.recognizers().any(|definition| {
                            definition
                                .selectors
                                .iter()
                                .any(|id| config.devices[id].matches(device))
                        }))
                {
                    join(&mut parents, first, index);
                }
            }
        }
        let roots: BTreeSet<_> = seeds
            .iter()
            .enumerate()
            .filter(|(_, seed)| **seed)
            .map(|(index, _)| root(&parents, index))
            .collect();
        let ordered: BTreeSet<_> = devices
            .iter()
            .enumerate()
            .filter(|(index, _)| roots.contains(&root(&parents, *index)))
            .map(|(_, device)| device.source.clone())
            .collect();
        for (source, route) in &mut self.routes {
            route.key_domain = ordered.contains(source);
        }
    }
}
