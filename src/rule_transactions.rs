//! Atomic runtime rule, layer-activation and named-mode edits. Policy, selectors, layer
//! definitions and engine mode remain
//! owned by the approved configuration; caller authorization is separate.
use crate::config::{Config, ConfigError, LayerActivation, Rule};
use serde::{Deserialize, Serialize};

pub const MAX_OPERATIONS: usize = 64;
pub const MAX_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Enabled {
        id: String,
        enabled: bool,
    },
    Replace {
        rule: Box<Rule>,
    },
    Remove {
        id: String,
    },
    Layer {
        id: String,
        activation: LayerActivation,
    },
    Mode {
        id: Option<String>,
    },
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Transaction {
    pub revision: u64,
    pub changes: Vec<Change>,
}
impl Transaction {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.revision == 0 || self.changes.is_empty() || self.changes.len() > MAX_OPERATIONS {
            return Err(ConfigError(
                "invalid rule transaction revision/operation count".into(),
            ));
        }
        for change in &self.changes {
            let id = match change {
                Change::Enabled { id, .. } | Change::Remove { id } | Change::Layer { id, .. } => id,
                Change::Replace { rule } => &rule.id,
                Change::Mode { id: Some(id) } => id,
                Change::Mode { id: None } => continue,
            };
            if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
                return Err(ConfigError("invalid runtime rule identity".into()));
            }
        }
        // Use the same bounded writer for local and wire-originated requests.
        struct Buffer(usize);
        impl std::io::Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self
                    .0
                    .checked_add(bytes.len())
                    .filter(|size| *size <= MAX_BYTES)
                    .ok_or_else(|| std::io::Error::other("rule transaction exceeds byte limit"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer(Buffer(0), self).map_err(|error| ConfigError(error.to_string()))
    }
    pub fn prepare(&self, current: &Config, revision: u64) -> Result<Config, ConfigError> {
        self.prepare_for(current, revision, crate::config::Validation::Public)
    }
    pub(crate) fn prepare_for(
        &self,
        current: &Config,
        revision: u64,
        validation: crate::config::Validation,
    ) -> Result<Config, ConfigError> {
        self.validate()?;
        if self.revision != revision || revision == u64::MAX {
            return Err(ConfigError(
                "stale or exhausted runtime rule revision".into(),
            ));
        }
        let mut next = current.clone();
        for change in &self.changes {
            match change {
                Change::Enabled { id, enabled } => {
                    let rule = next
                        .rules
                        .iter_mut()
                        .find(|rule| &rule.id == id)
                        .ok_or_else(|| ConfigError(format!("unknown runtime rule {id}")))?;
                    rule.enabled = *enabled;
                }
                Change::Replace { rule } => {
                    if let Some(position) = next.rules.iter().position(|old| old.id == rule.id) {
                        next.rules[position] = rule.as_ref().clone();
                    } else {
                        next.rules.push(rule.as_ref().clone());
                    }
                }
                Change::Remove { id } => {
                    let position = next
                        .rules
                        .iter()
                        .position(|rule| &rule.id == id)
                        .ok_or_else(|| ConfigError(format!("unknown runtime rule {id}")))?;
                    next.rules.remove(position);
                }
                Change::Mode { id } => {
                    if id
                        .as_ref()
                        .is_some_and(|id| !next.modes.iter().any(|mode| mode.id == *id))
                    {
                        return Err(ConfigError("unknown runtime mode".into()));
                    }
                    for mode in &mut next.modes {
                        mode.active = id.as_ref() == Some(&mode.id);
                    }
                }
                Change::Layer { id, activation } => {
                    let layer = next
                        .layers
                        .iter_mut()
                        .find(|layer| &layer.id == id)
                        .ok_or_else(|| ConfigError(format!("unknown runtime layer {id}")))?;
                    layer.activation = *activation;
                }
            }
        }
        // Includes ID/reference/unit/priority/conflict and policy validation.
        next.validate_for(validation)?;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        Config::parse_named("version=1\n[[rules]]\nid='a'\ndelivery='reserve'\n[rules.trigger]\nkind='key'\nkey='KEY_A'\n[rules.action]\nkind='event'\nname='a'","rules.toml").unwrap()
    }
    #[test]
    fn complete_transaction_validates_before_any_current_rule_is_changed() {
        let current = config();
        let valid = Transaction {
            revision: 1,
            changes: vec![Change::Enabled {
                id: "a".into(),
                enabled: false,
            }],
        };
        let next = valid.prepare(&current, 1).unwrap();
        assert!(!next.rules[0].enabled && current.rules[0].enabled);
        assert_eq!(next.policy, current.policy);
        assert_eq!(next.devices, current.devices);
        assert_eq!(next.mode, current.mode);
        assert_eq!(next.layers, current.layers);
        let invalid = Transaction {
            revision: 1,
            changes: vec![
                valid.changes[0].clone(),
                Change::Remove {
                    id: "missing".into(),
                },
            ],
        };
        assert!(invalid.prepare(&current, 1).is_err());
        assert!(current.rules[0].enabled);
        assert!(valid.prepare(&current, 2).is_err());
        assert!(
            Transaction {
                revision: u64::MAX,
                ..valid.clone()
            }
            .prepare(&current, u64::MAX)
            .is_err()
        );
    }
    #[test]
    fn replacements_additions_removals_and_conflicts_use_full_schema_validation() {
        let current = config();
        let mut changed = current.rules[0].clone();
        changed.priority = 20;
        let replace = Transaction {
            revision: 1,
            changes: vec![Change::Replace {
                rule: Box::new(changed.clone()),
            }],
        };
        assert_eq!(replace.prepare(&current, 1).unwrap().rules[0].priority, 20);
        changed.id = "b".into();
        changed.priority = 1;
        let add = Transaction {
            revision: 1,
            changes: vec![Change::Replace {
                rule: Box::new(changed.clone()),
            }],
        };
        assert_eq!(add.prepare(&current, 1).unwrap().rules.len(), 2);
        changed.priority = current.rules[0].priority;
        let conflict = Transaction {
            revision: 1,
            changes: vec![Change::Replace {
                rule: Box::new(changed),
            }],
        };
        assert!(conflict.prepare(&current, 1).is_err());
        let remove = Transaction {
            revision: 1,
            changes: vec![Change::Remove { id: "a".into() }],
        };
        assert!(remove.prepare(&current, 1).unwrap().rules.is_empty());
    }
    #[test]
    fn layer_edits_are_atomic_with_rules_and_cannot_redefine_physical_bindings() {
        let mut current = config();
        current.layers.push(crate::config::Layer {
            id: "touch".into(),
            key: crate::config::Key(125),
            toggle: true,
            activation: LayerActivation::Physical,
        });
        current.rules[0].layer = Some("touch".into());
        let transaction = Transaction {
            revision: 1,
            changes: vec![
                Change::Layer {
                    id: "touch".into(),
                    activation: LayerActivation::Active,
                },
                Change::Enabled {
                    id: "a".into(),
                    enabled: false,
                },
            ],
        };
        let next = transaction.prepare(&current, 1).unwrap();
        assert_eq!(next.layers[0].activation, LayerActivation::Active);
        assert_eq!(
            (next.layers[0].key, next.layers[0].toggle),
            (current.layers[0].key, true)
        );
        assert!(!next.rules[0].enabled);
        assert_eq!(current.layers[0].activation, LayerActivation::Physical);
        let mut invalid = transaction.clone();
        invalid.changes.push(Change::Layer {
            id: "missing".into(),
            activation: LayerActivation::Inactive,
        });
        assert!(invalid.prepare(&current, 1).is_err());
        assert!(current.rules[0].enabled);
        assert!(serde_json::from_str::<Transaction>(r#"{"revision":1,"changes":[{"operation":"layer","id":"touch","activation":"active","key":"KEY_A"}]}"#).is_err());
    }

    #[test]
    fn simultaneous_layer_activation_rejects_ambiguity_but_atomic_layer_swap_is_valid() {
        let mut current = config();
        current.layers = ["one", "two"]
            .iter()
            .map(|id| crate::config::Layer {
                id: (*id).into(),
                key: crate::config::Key(125),
                toggle: false,
                activation: LayerActivation::Inactive,
            })
            .collect();
        current.rules[0].layer = Some("one".into());
        let mut other = current.rules[0].clone();
        other.id = "b".into();
        other.layer = Some("two".into());
        current.rules.push(other);
        current.validate().unwrap();
        let activate = |id: &str| Change::Layer {
            id: id.into(),
            activation: LayerActivation::Active,
        };
        assert!(
            Transaction {
                revision: 1,
                changes: vec![activate("one"), activate("two")]
            }
            .prepare(&current, 1)
            .is_err()
        );
        let one = Transaction {
            revision: 1,
            changes: vec![activate("one")],
        }
        .prepare(&current, 1)
        .unwrap();
        let two = Transaction {
            revision: 2,
            changes: vec![
                Change::Layer {
                    id: "one".into(),
                    activation: LayerActivation::Inactive,
                },
                activate("two"),
            ],
        }
        .prepare(&one, 2)
        .unwrap();
        assert_eq!(two.layers[0].activation, LayerActivation::Inactive);
        assert_eq!(two.layers[1].activation, LayerActivation::Active);
        assert!(
            current
                .layers
                .iter()
                .all(|layer| layer.activation == LayerActivation::Inactive)
        );
        let physical = Transaction {
            revision: 1,
            changes: vec![
                Change::Layer {
                    id: "one".into(),
                    activation: LayerActivation::Physical,
                },
                Change::Layer {
                    id: "two".into(),
                    activation: LayerActivation::Physical,
                },
            ],
        };
        assert!(physical.prepare(&current, 1).is_err());
    }

    #[test]
    fn serialized_changes_reject_unknown_privilege_fields_and_resource_overflows() {
        assert!(
            serde_json::from_str::<Transaction>(r#"{"revision":1,"changes":[],"policy":{}}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<Transaction>(r#"{"revision":1,"changes":[{"operation":"enabled","id":"a","enabled":false,"seat":"other"}]}"#).is_err());
        let change = Change::Enabled {
            id: "a".into(),
            enabled: false,
        };
        assert!(
            Transaction {
                revision: 0,
                changes: vec![change.clone()]
            }
            .validate()
            .is_err()
        );
        assert!(
            Transaction {
                revision: 1,
                changes: vec![change; MAX_OPERATIONS + 1]
            }
            .validate()
            .is_err()
        );
        let mut rule = config().rules.remove(0);
        rule.action = crate::config::Action::Command {
            argv: vec!["x".repeat(4096); 16],
        };
        let oversized = Transaction {
            revision: 1,
            changes: vec![Change::Replace {
                rule: Box::new(rule),
            }],
        };
        assert!(oversized.validate().unwrap_err().0.contains("byte limit"));
    }

    #[test]
    fn named_mode_selection_is_atomic_validated_and_does_not_change_routing_authority() {
        use crate::config::NamedMode;
        let mut current = config();
        current.modes = vec![
            NamedMode {
                id: "normal".into(),
                active: true,
            },
            NamedMode {
                id: "navigate".into(),
                active: false,
            },
        ];
        current.rules[0].when_mode = Some("normal".into());
        let mut other = current.rules[0].clone();
        other.id = "b".into();
        other.when_mode = Some("navigate".into());
        current.rules.push(other);
        current.validate().unwrap();
        let select = |id: Option<&str>| Transaction {
            revision: 1,
            changes: vec![Change::Mode {
                id: id.map(str::to_owned),
            }],
        };
        let next = select(Some("navigate")).prepare(&current, 1).unwrap();
        assert!(!next.mode_active(&next.rules[0]) && next.mode_active(&next.rules[1]));
        assert_eq!(next.mode, current.mode);
        assert_eq!(next.policy, current.policy);
        assert_eq!(next.devices, current.devices);
        assert!(current.modes[0].active && !current.modes[1].active);
        let reset = select(None).prepare(&current, 1).unwrap();
        assert!(reset.modes.iter().all(|mode| !mode.active));
        assert!(select(Some("missing")).prepare(&current, 1).is_err());
        assert!(select(Some("navigate")).prepare(&current, 2).is_err());
        let mut invalid = current.clone();
        invalid.modes[1].active = true;
        assert!(invalid.validate().is_err());
        invalid.modes[1].active = false;
        invalid.modes[1].id = "normal".into();
        assert!(invalid.validate().is_err());
        invalid.modes[1].id = "\n".into();
        assert!(invalid.validate().is_err());
        invalid.modes.clear();
        assert!(invalid.validate().is_err());
        invalid = current.clone();
        invalid.modes = (0..65)
            .map(|n| NamedMode {
                id: n.to_string(),
                active: false,
            })
            .collect();
        assert!(invalid.validate().is_err());
        invalid = current.clone();
        invalid.rules[1].when_mode = None;
        invalid.validate().unwrap_err();
        // A conflicting activation can be made valid only by a full atomic edit.
        let mut conflict = current.clone();
        conflict.rules[0].when_mode = None;
        assert!(select(Some("navigate")).prepare(&conflict, 1).is_err());
        let mut batch = select(Some("navigate"));
        batch.changes.push(Change::Enabled {
            id: "a".into(),
            enabled: false,
        });
        assert!(batch.prepare(&conflict, 1).is_ok());
        assert!(
            serde_json::from_str::<Transaction>(
                r#"{"revision":1,"changes":[{"operation":"mode","id":"normal","route":true}]}"#
            )
            .is_err()
        );
    }
}
