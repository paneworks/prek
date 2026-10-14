//! Read-only capability preflight. Missing hardware and unfinished consumers
//! are explicit findings, never silently accepted as operational routing.
use crate::{
    config::{Action, Config, Mode, Trigger},
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
                add("context_provider_unavailable", None, Some(&rule.id), "Live context snapshots and lock-provider negotiation are not connected yet; observation skips this rule until a verified provider is available".into());
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
            if matches!(rule.action, Action::Morf { .. }) {
                add(
                    "morf_adapter_unavailable",
                    None,
                    Some(&rule.id),
                    "Morf context/action negotiation is not implemented yet".into(),
                );
            }
            if matches!(rule.action, Action::Hyprland { .. }) {
                add(
                    "hyprland_adapter_unavailable",
                    None,
                    Some(&rule.id),
                    "Hyprland window context/actions are not connected to the session agent yet"
                        .into(),
                );
            }
        }
        if config.mode == Mode::Route {
            add("routing_pending", None, None, "Routing remains disabled: configured takeover, complete consumer readiness and action/context integration are pending".into());
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
        let config = Config::preset("phone").unwrap();
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
        let config = Config::preset("phone").unwrap();
        let report = Report::inspect(&config, &[touch(2, false)]);
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.code == "finger_capacity"
                    && finding.rule.as_deref() == Some("G10"))
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
