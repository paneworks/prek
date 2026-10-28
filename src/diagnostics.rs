//! Read-only capability preflight. Missing hardware and unfinished consumers
//! are explicit findings, never silently accepted as operational routing.
use crate::{
    config::{Config, Mode, Trigger},
    input::Device,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Entry {
    pub path: PathBuf,
    pub available: bool,
    pub device: Option<Device>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Finding {
    pub code: String,
    pub selector: Option<String>,
    pub rule: Option<String>,
    pub message: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Report {
    pub version: u32,
    pub ready: bool,
    pub mode: Mode,
    pub matches: BTreeMap<String, Vec<String>>,
    pub findings: Vec<Finding>,
}
impl Report {
    pub fn inspect(config: &Config, inventory: &[Entry]) -> Self {
        let matches: BTreeMap<_, Vec<_>> = config
            .devices
            .iter()
            .map(|(id, selector)| {
                (
                    id.clone(),
                    inventory
                        .iter()
                        .filter_map(|entry| {
                            entry
                                .device
                                .as_ref()
                                .filter(|device| entry.available && selector.matches(device))
                        })
                        .map(|device| device.identity.clone())
                        .collect(),
                )
            })
            .collect();
        let mut findings = Vec::new();
        let mut add = |code: &str, selector: Option<&str>, rule: Option<&str>, message: String| {
            findings.push(Finding {
                code: code.into(),
                selector: selector.map(str::to_owned),
                rule: rule.map(str::to_owned),
                message,
            })
        };
        for (id, selector) in &config.devices {
            let required =
                config.rules.iter().any(|rule| {
                    rule.enabled && (rule.devices.is_empty() || rule.devices.contains(id))
                }) || config.mode == Mode::Route && config.policy.allowed_devices.contains(id);
            if !required {
                continue;
            }
            if matches[id].is_empty() {
                let inaccessible = inventory.iter().any(|entry| {
                    !entry.available
                        && selector
                            .path
                            .as_ref()
                            .is_some_and(|path| *path == entry.path.to_string_lossy())
                });
                add(
                    if inaccessible {
                        "access_denied"
                    } else {
                        "device_unavailable"
                    },
                    Some(id),
                    None,
                    if inaccessible {
                        "Selected event node cannot be read".into()
                    } else {
                        "No readable physical device matches this selector".into()
                    },
                );
            }
            let mut identities = std::collections::BTreeSet::new();
            for identity in &matches[id] {
                if !identities.insert(identity) {
                    add(
                        "ambiguous_identity",
                        Some(id),
                        None,
                        format!("Multiple event nodes share stable identity {identity}"),
                    );
                }
            }
        }
        for rule in config.rules.iter().filter(|rule| rule.enabled) {
            if let Some(reason) = crate::rules::unsupported(&rule.trigger) {
                add(
                    "rule_runtime_unavailable",
                    None,
                    Some(&rule.id),
                    reason.into(),
                );
            }
            if crate::rules::requires_context(rule) {
                add("context_provider_unavailable", None, Some(&rule.id), "Needs live context from a broker-verified session agent (`prek session --context`); observation without one skips this rule".into());
            }
            if config.mode == Mode::Route
                && rule.delivery != crate::config::Delivery::Observe
                && rule.trigger.contains_touch()
                && rule.region.is_none()
            {
                add(
                    "touch_start_delay",
                    None,
                    Some(&rule.id),
                    format!(
                        "Every touch start on this rule's devices can wait up to the {} ms join window for further fingers; a region limits the wait to its edge strip",
                        config.timing.join_window.0
                    ),
                );
            }
            let finger_count = maximum_fingers(&rule.trigger);
            for (id, selector) in config
                .devices
                .iter()
                .filter(|(id, _)| rule.devices.is_empty() || rule.devices.contains(id))
            {
                for device in inventory.iter().filter_map(|entry| {
                    entry
                        .device
                        .as_ref()
                        .filter(|device| entry.available && selector.matches(device))
                }) {
                    if let Some(fingers) = finger_count {
                        match device.touch_capacity() {
                            Some(capacity) if usize::from(fingers) > capacity => add(
                                "finger_capacity",
                                Some(id),
                                Some(&rule.id),
                                format!(
                                    "{} supports {capacity} contacts; rule requires {fingers}",
                                    device.name
                                ),
                            ),
                            None => add(
                                "unknown_finger_capacity",
                                Some(id),
                                Some(&rule.id),
                                format!("{} does not report usable MT slot capacity", device.name),
                            ),
                            _ => {}
                        }
                    }
                }
            }
        }
        if config.mode == Mode::Route {
            add("routing_pending", None, None, "Routing runs only under `prek broker --mode route`, takes over devices held by the bound Wayland compositor at a neutral idle cut and has passed only bounded trials on real hardware; suspend, hotplug, reboot and rollback are not yet accepted".into());
        }
        Self {
            version: 1,
            ready: findings.is_empty(),
            mode: config.mode,
            matches,
            findings,
        }
    }
}
fn maximum_fingers(trigger: &Trigger) -> Option<u16> {
    if let Trigger::Sequence { steps, .. } = trigger {
        steps.iter().filter_map(maximum_fingers).max()
    } else {
        trigger.fingers()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Selector, input::*};
    fn phone() -> Config {
        let mut config = Config::preset("phone").unwrap();
        config.rules.push(crate::config::Rule {
            id: "three".into(),
            enabled: true,
            priority: 0,
            devices: vec!["touch".into()],
            seats: vec![],
            contexts: vec![crate::config::Context::Desktop],
            scope: crate::config::Scope::Everywhere,
            region: None,
            output: None,
            screen: None,
            require_keys: vec![],
            keys_throughout: false,
            layer: None,
            when_mode: None,
            delivery: crate::config::Delivery::Buffer,
            trigger: Trigger::Swipe {
                fingers: 3,
                direction: crate::config::Direction::Vertical,
                travel: crate::config::Distance::Millimetres(12.0),
                fallback_travel: crate::config::Distance::Normalized(0.04),
                continuous: false,
            },
            action: crate::config::Action::Event {
                name: "three".into(),
            },
        });
        config.validate().unwrap();
        config
    }
    fn touch(capacity: usize, virtual_device: bool) -> Entry {
        let axis = |maximum| Axis {
            minimum: 0,
            maximum,
            resolution: 10,
            fuzz: 0,
            flat: 0,
        };
        Entry {
            path: "/dev/input/test".into(),
            available: true,
            error: None,
            device: Some(Device {
                identity: "test-touch".into(),
                source: Source {
                    device: 1,
                    generation: 1,
                    seat: "seat0".into(),
                },
                name: "test".into(),
                path: "/dev/input/test".into(),
                physical: "test".into(),
                unique: "test".into(),
                vendor: 1,
                product: 1,
                properties: vec![1],
                capabilities: std::collections::BTreeMap::from([(
                    EV_ABS,
                    vec![ABS_MT_POSITION_X, ABS_MT_POSITION_Y],
                )]),
                axes: std::collections::BTreeMap::from([
                    (ABS_MT_SLOT, axis(capacity as i32 - 1)),
                    (ABS_MT_POSITION_X, axis(1000)),
                    (ABS_MT_POSITION_Y, axis(2000)),
                ]),
                virtual_device,
            }),
        }
    }
    #[test]
    fn t04_minimal_preflight_needs_no_hardware_or_desktop() {
        let report = Report::inspect(&Config::preset("minimal").unwrap(), &[]);
        assert!(report.ready);
        assert!(report.findings.is_empty());
    }
    #[test]
    fn t04_virtual_devices_are_excluded_and_unknown_capacity_is_reported() {
        let config = phone();
        let report = Report::inspect(&config, &[touch(10, true)]);
        assert!(report.matches["touch"].is_empty());
        let mut entry = touch(10, false);
        entry.device.as_mut().unwrap().axes.remove(&ABS_MT_SLOT);
        assert!(
            Report::inspect(&config, &[entry])
                .findings
                .iter()
                .any(|finding| finding.code == "unknown_finger_capacity")
        );
    }
    #[test]
    fn t04_finger_capacity_and_unfinished_routing_are_honest() {
        let config = phone();
        let report = Report::inspect(&config, &[touch(2, false)]);
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.code == "finger_capacity"
                    && finding.rule.as_deref() == Some("three"))
        );
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.code == "routing_pending")
        );
        assert!(!report.ready);
    }
    #[test]
    fn consuming_touch_rules_without_a_region_report_their_start_delay() {
        let delayed = |config: &Config| {
            Report::inspect(config, &[touch(10, false)])
                .findings
                .iter()
                .any(|finding| {
                    finding.code == "touch_start_delay" && finding.rule.as_deref() == Some("three")
                })
        };
        let mut config = phone();
        assert!(delayed(&config));
        config.rules.last_mut().unwrap().region = Some(crate::config::Region {
            edge: crate::config::Edge::Bottom,
            depth: crate::config::Distance::Pixels(20.0),
            from: 0.0,
            to: 1.0,
        });
        assert!(!delayed(&config));
        config.rules.last_mut().unwrap().region = None;
        config.rules.last_mut().unwrap().delivery = crate::config::Delivery::Observe;
        assert!(!delayed(&config));
    }
    #[test]
    fn t04_unreadable_explicit_node_and_ambiguous_identity_are_distinct() {
        let mut config = Config::preset("phone").unwrap();
        config.devices.insert(
            "touch".into(),
            Selector {
                path: Some("/dev/input/test".into()),
                ..Selector::default()
            },
        );
        let unreadable = Entry {
            path: "/dev/input/test".into(),
            available: false,
            device: None,
            error: Some("permission denied".into()),
        };
        assert!(
            Report::inspect(&config, &[unreadable])
                .findings
                .iter()
                .any(|finding| finding.code == "access_denied")
        );
        assert!(
            Report::inspect(&config, &[touch(10, false), touch(10, false)])
                .findings
                .iter()
                .any(|finding| finding.code == "ambiguous_identity")
        );
    }
}
