//! Versioned declarative rules and last-good configuration management.
use crate::{
    Timing,
    input::{ABS_MT_POSITION_X, ABS_MT_POSITION_Y, Device, EV_KEY},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    time::Duration,
};

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_RULES: usize = 1024;
pub(crate) const MAX_SELECTORS: usize = 256;
pub(crate) const MAX_LAYERS: usize = 64;
pub(crate) const MAX_SHAPES: usize = 128;
pub(crate) const MAX_MODES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Millis(pub u64);
impl Millis {
    pub fn duration(self) -> Duration {
        Duration::from_millis(self.0)
    }
    pub fn parse(text: &str) -> Result<Self, String> {
        let (number, factor) = if let Some(value) = text.strip_suffix("ms") {
            (value, 1)
        } else if let Some(value) = text.strip_suffix('s') {
            (value, 1000)
        } else if let Some(value) = text.strip_suffix('m') {
            (value, 60_000)
        } else {
            return Err("duration requires ms, s or m units".into());
        };
        let value: u64 = number
            .parse()
            .map_err(|_| "duration must be a whole positive number")?;
        let value = value
            .checked_mul(factor)
            .filter(|value| *value > 0 && *value <= 3_600_000)
            .ok_or("duration must be between 1 ms and one hour")?;
        Ok(Self(value))
    }
}
impl Serialize for Millis {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{}ms", self.0))
    }
}
impl<'de> Deserialize<'de> for Millis {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Distance {
    Millimetres(f64),
    Pixels(f64),
    Normalized(f64),
}
impl Distance {
    pub fn value(self) -> f64 {
        match self {
            Self::Millimetres(v) | Self::Pixels(v) | Self::Normalized(v) => v,
        }
    }
    pub fn parse(text: &str) -> Result<Self, String> {
        let (number, unit) = if let Some(value) = text.strip_suffix("mm") {
            (value, 0)
        } else if let Some(value) = text.strip_suffix("px") {
            (value, 1)
        } else if let Some(value) = text.strip_suffix("norm") {
            (value, 2)
        } else {
            return Err("distance requires mm, px or norm units".into());
        };
        let value: f64 = number.parse().map_err(|_| "invalid distance")?;
        if !value.is_finite() || value <= 0.0 || value > if unit == 2 { 2.0 } else { 1_000_000.0 } {
            return Err("distance is outside supported positive bounds".into());
        }
        Ok(match unit {
            0 => Self::Millimetres(value),
            1 => Self::Pixels(value),
            _ => Self::Normalized(value),
        })
    }
}
impl Serialize for Distance {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&match self {
            Self::Millimetres(v) => format!("{v}mm"),
            Self::Pixels(v) => format!("{v}px"),
            Self::Normalized(v) => format!("{v}norm"),
        })
    }
}
impl<'de> Deserialize<'de> for Distance {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Key(pub u16);
impl Key {
    pub fn parse(name: &str) -> Result<Self, String> {
        let alias = match name.to_lowercase().as_str() {
            "super" | "meta" | "sup" => Some(125),
            "ctrl" | "control" => Some(29),
            "alt" => Some(56),
            "shift" => Some(42),
            "enter" | "return" => Some(28),
            "space" => Some(57),
            "power" => Some(116),
            "volume_up" => Some(115),
            "volume_down" => Some(114),
            _ => None,
        };
        if let Some(code) = alias {
            return Ok(Self(code));
        }
        if let Some(code) = name.strip_prefix("code:") {
            return code
                .parse::<u16>()
                .map(Self)
                .map_err(|_| "invalid numeric key code".into());
        }
        #[cfg(target_os = "linux")]
        if let Ok(code) = name.parse::<evdev::KeyCode>() {
            return Ok(Self(code.0));
        }
        Err(format!(
            "unknown key {name:?}; use a Linux KEY_/BTN_ name, alias or code:N"
        ))
    }
}
impl Serialize for Key {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("code:{}", self.0))
    }
}
impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Observe,
    Route,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    Observe,
    Buffer,
    Reserve,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Context {
    Console,
    Desktop,
    Locking,
    Locked,
    Greeter,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Everywhere,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
    Horizontal,
    Vertical,
    Any,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Screen {
    On,
    Off,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}
/// A strip along one edge of the mapped output in which every finger must start.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Region {
    pub edge: Edge,
    /// Logical pixels, or a fraction of the output's extent across the edge.
    pub depth: Distance,
    /// Span along the edge as fractions of its length from its top or left end.
    #[serde(default)]
    pub from: f64,
    #[serde(default = "whole_edge")]
    pub to: f64,
}
fn whole_edge() -> f64 {
    1.0
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timings {
    pub tap_max: Millis,
    pub tap_gap: Millis,
    pub hold_after: Millis,
    pub join_window: Millis,
    pub decision_window: Millis,
    pub swipe_timeout: Millis,
    pub tap_motion: Distance,
    pub fallback_motion: Distance,
    pub pan_slop: Distance,
}
impl Default for Timings {
    fn default() -> Self {
        Self {
            tap_max: Millis(250),
            tap_gap: Millis(300),
            hold_after: Millis(600),
            join_window: Millis(120),
            decision_window: Millis(300),
            swipe_timeout: Millis(1500),
            tap_motion: Distance::Millimetres(3.0),
            fallback_motion: Distance::Normalized(0.01),
            pan_slop: Distance::Millimetres(3.0),
        }
    }
}
impl Timings {
    pub fn buttons(&self) -> Timing {
        Timing {
            tap_max: self.tap_max.duration(),
            tap_gap: self.tap_gap.duration(),
            hold_after: self.hold_after.duration(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Selector {
    pub identity: Option<String>,
    pub path: Option<String>,
    pub name: Option<String>,
    pub physical: Option<String>,
    pub unique: Option<String>,
    pub seat: Option<String>,
    pub keys: Vec<Key>,
    pub absolute_axes: Vec<u16>,
    pub properties: Vec<u16>,
    /// Output a matched touch device maps to; the session's only output when unset.
    pub output: Option<String>,
}
impl Selector {
    pub fn matches(&self, device: &Device) -> bool {
        !device.virtual_device
            && self.identity.as_ref().is_none_or(|v| *v == device.identity)
            && self.path.as_ref().is_none_or(|v| *v == device.path)
            && self.name.as_ref().is_none_or(|v| *v == device.name)
            && self.physical.as_ref().is_none_or(|v| *v == device.physical)
            && self.unique.as_ref().is_none_or(|v| *v == device.unique)
            && self.seat.as_ref().is_none_or(|v| *v == device.source.seat)
            && self.keys.iter().all(|key| device.has(EV_KEY, key.0))
            && self
                .absolute_axes
                .iter()
                .all(|code| device.has(crate::input::EV_ABS, *code))
            && self
                .properties
                .iter()
                .all(|property| device.properties.contains(property))
    }
    fn constrained(&self) -> bool {
        self.identity.is_some()
            || self.path.is_some()
            || self.name.is_some()
            || self.physical.is_some()
            || self.unique.is_some()
            || !self.keys.is_empty()
            || !self.absolute_axes.is_empty()
            || !self.properties.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub allowed_devices: Vec<String>,
    pub allowed_seats: Vec<String>,
    /// Root-approved permission; runtime transactions cannot edit policy.
    pub allow_runtime_rules: bool,
    /// Root-approved, explicit opt-in to normalized desktop input observation.
    pub allow_input_events: bool,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            allowed_devices: Vec::new(),
            allowed_seats: vec!["seat0".into()],
            allow_runtime_rules: false,
            allow_input_events: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trigger {
    /// Requires a trusted asynchronous ticket coordinator and source binding.
    Custom {
        recognizer: crate::recognition::Definition,
        timeout: Millis,
        #[serde(default)]
        wake: Option<Millis>,
    },
    /// Preserve press/release ownership, primarily for remapping.
    Key {
        key: Key,
    },
    ButtonTaps {
        key: Key,
        count: u32,
    },
    ButtonHold {
        key: Key,
        after: Millis,
        #[serde(default)]
        repeat: Option<Repeat>,
    },
    Chord {
        keys: Vec<Key>,
        ordered: bool,
        timeout: Millis,
    },
    TouchTaps {
        fingers: u16,
        count: u32,
    },
    TouchHold {
        fingers: u16,
        after: Millis,
    },
    Swipe {
        fingers: u16,
        direction: Direction,
        travel: Distance,
        fallback_travel: Distance,
        #[serde(default)]
        continuous: bool,
    },
    Pinch {
        fingers: u16,
        ratio: f64,
    },
    Rotate {
        fingers: u16,
        degrees: f64,
    },
    Shape {
        name: String,
        /// None selects an armed mouse path; Some selects touch contacts.
        fingers: Option<u16>,
        /// Capture delimiter for mouse paths; defaults to BTN_LEFT.
        #[serde(default)]
        button: Option<Key>,
    },
    Wheel {
        horizontal: bool,
        minimum: f64,
    },
    Sequence {
        steps: Vec<Trigger>,
        timeout: Millis,
    },
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Repeat {
    pub delay: Millis,
    pub interval: Millis,
}
impl Trigger {
    pub(crate) fn recognizers(&self) -> impl Iterator<Item = &crate::recognition::Definition> {
        let mut current = std::slice::from_ref(self).iter();
        let mut parents = Vec::new();
        std::iter::from_fn(move || {
            loop {
                match current.next() {
                    Some(Self::Custom { recognizer, .. }) => return Some(recognizer),
                    Some(Self::Sequence { steps, .. }) => {
                        parents.push(std::mem::replace(&mut current, steps.iter()))
                    }
                    Some(_) => {}
                    None => current = parents.pop()?,
                }
            }
        })
    }

    pub fn contains_touch(&self) -> bool {
        self.fingers().is_some()
            || matches!(self, Self::Custom { recognizer, .. } if recognizer.inputs.contains(&crate::recognition::InputKind::Touch))
            || matches!(self, Self::Sequence { steps, .. } if steps.iter().any(Self::contains_touch))
    }
    pub fn fingers(&self) -> Option<u16> {
        match self {
            Self::TouchTaps { fingers, .. }
            | Self::TouchHold { fingers, .. }
            | Self::Swipe { fingers, .. }
            | Self::Pinch { fingers, .. }
            | Self::Rotate { fingers, .. } => Some(*fingers),
            Self::Shape { fingers, .. } => *fingers,
            _ => None,
        }
    }
}

#[cfg(test)]
mod recognizer_walk_tests {
    use super::*;
    use crate::recognition::{Definition, InputKind};

    fn custom(name: &str) -> Trigger {
        Trigger::Custom {
            recognizer: Definition {
                name: name.into(),
                selectors: BTreeSet::from(["keyboard".into()]),
                inputs: BTreeSet::from([InputKind::Key]),
                reply_within: Millis(250),
                continuous: false,
            },
            timeout: Millis(1000),
            wake: None,
        }
    }
    fn sequence(steps: Vec<Trigger>) -> Trigger {
        Trigger::Sequence {
            steps,
            timeout: Millis(1000),
        }
    }
    fn profile(trigger: Trigger) -> Config {
        let mut config = Config::preset("desktop").unwrap();
        config.policy.allow_input_events = true;
        config.policy.allowed_devices = config.devices.keys().cloned().collect();
        config.rules[0].trigger = trigger;
        config
    }

    #[test]
    fn nested_declarations_are_walked_in_step_order_without_ordinary_triggers() {
        let trigger = sequence(vec![
            Trigger::Key { key: Key(30) },
            custom("first"),
            sequence(vec![
                custom("second"),
                sequence(vec![custom("first"), custom("third")]),
            ]),
        ]);
        assert_eq!(
            trigger
                .recognizers()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "first", "third"]
        );
        assert!(Trigger::Key { key: Key(30) }.recognizers().next().is_none());
    }

    #[test]
    fn source_registry_collects_nested_and_retained_declarations_canonically() {
        let mut config = profile(sequence(vec![
            custom("b"),
            sequence(vec![custom("a"), custom("b")]),
        ]));
        config.recognizers = vec![custom("unused").recognizers().next().unwrap().clone()];
        assert_eq!(
            config
                .recognizer_definitions()
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "unused"]
        );
    }

    #[test]
    fn nested_permission_and_declaration_conflicts_are_not_hidden_by_composition() {
        let trigger = sequence(vec![custom("same"), custom("same")]);
        let mut config = profile(trigger);
        let Trigger::Sequence { steps, .. } = &mut config.rules[0].trigger else {
            unreachable!()
        };
        let Trigger::Custom { recognizer, .. } = &mut steps[1] else {
            unreachable!()
        };
        recognizer.reply_within = Millis(100);
        assert!(
            config
                .validate()
                .unwrap_err()
                .0
                .contains("consistent and bounded")
        );
        let mut config = profile(sequence(vec![
            Trigger::Key { key: Key(30) },
            custom("hidden"),
        ]));
        config.policy.allow_input_events = false;
        assert!(
            config
                .validate()
                .unwrap_err()
                .0
                .contains("protected input permission")
        );
    }

    #[test]
    fn nested_walk_preserves_empty_branches_and_returns_to_parent_siblings() {
        let trigger = sequence(vec![
            sequence(vec![]),
            sequence(vec![sequence(vec![]), custom("inner")]),
            custom("last"),
        ]);
        assert_eq!(
            trigger
                .recognizers()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["inner", "last"]
        );
    }

    #[test]
    fn recursive_approval_does_not_enable_uncoordinated_custom_sequence_execution() {
        let mut config = profile(sequence(vec![
            Trigger::Key { key: Key(30) },
            custom("nested"),
        ]));
        config.rules[0].enabled = true;
        config.validate().unwrap();
        assert!(crate::rules::unsupported(&config.rules[0].trigger).is_some());
        assert!(crate::rules::Engine::new(config).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn coordinated_validation_admits_declarations_but_requires_a_real_coordinator() {
        let mut config = profile(sequence(vec![
            Trigger::Key { key: Key(30) },
            custom("nested"),
        ]));
        config.rules[0].enabled = true;
        config.rules[0].delivery = Delivery::Observe;
        config.rules[0].scope = Scope::Everywhere;
        config.rules[0].contexts = vec![Context::Desktop];
        config.validate().unwrap();
        let text = toml::to_string(&config).unwrap();
        assert_eq!(Config::parse(&text).unwrap(), config);
        assert!(crate::rules::Engine::new(config.clone()).is_err());
        let fence = crate::protocol::Fence {
            broker: crate::protocol::Epoch([1, 2]),
            configuration: 1,
            session: 1,
        };
        assert!(crate::rules::Engine::with_recognition(config.clone(), fence).is_ok());
        crate::worker::preflight(&config).unwrap();
        assert!(Config::parse(&format!("validation='coordinated'\n{text}")).is_err());
    }

    #[test]
    fn coordinated_plans_keep_permission_source_declaration_depth_and_timeout_validation() {
        let valid = profile(sequence(vec![
            Trigger::Key { key: Key(30) },
            custom("nested"),
        ]));
        for violation in 0..6 {
            let mut config = valid.clone();
            match violation {
                0 => config.policy.allow_input_events = false,
                1 => config.rules[0].devices = vec!["unknown".into()],
                2 => {
                    if let Trigger::Sequence { timeout, .. } = &mut config.rules[0].trigger {
                        *timeout = Millis(0);
                    }
                }
                3 => {
                    if let Trigger::Sequence { steps, .. } = &mut config.rules[0].trigger {
                        steps.truncate(1);
                    }
                }
                4 => {
                    let mut bad = custom("nested");
                    if let Trigger::Custom { recognizer, .. } = &mut bad {
                        recognizer.reply_within = Millis(100);
                    }
                    config.recognizers = bad.recognizers().cloned().collect();
                }
                _ => {
                    for _ in 0..9 {
                        config.rules[0].trigger = sequence(vec![
                            Trigger::Key { key: Key(30) },
                            config.rules[0].trigger.clone(),
                        ]);
                    }
                }
            }
            assert!(config.validate().is_err(), "violation={violation}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nested_callbacks_require_context_and_are_excluded_from_unfenced_baselines() {
        let mut config = profile(sequence(vec![
            Trigger::Key { key: Key(30) },
            custom("nested"),
        ]));
        let rule = &mut config.rules[0];
        rule.enabled = true;
        rule.contexts.clear();
        rule.scope = Scope::Everywhere;
        rule.action = Action::Event {
            name: "composed".into(),
        };
        assert!(crate::rules::requires_context(rule));
        let baseline = crate::worker::preflight_profile(&config);
        assert!(!baseline.rules[0].enabled);
        assert!(config.rules[0].enabled);
        assert_eq!(baseline.rules[0].trigger, config.rules[0].trigger);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn coordinated_route_preflight_checks_buffer_timing_before_disabling_callback_rules() {
        let mut config = profile(sequence(vec![custom("first"), custom("last")]));
        config.mode = Mode::Route;
        config.rules[0].enabled = true;
        config.rules[0].delivery = Delivery::Buffer;
        config.timing.join_window = config.timing.decision_window;
        config.validate().unwrap();
        let error = crate::worker::preflight(&config).unwrap_err();
        assert!(error.to_string().contains("join_window < decision_window"));
        config.timing.join_window = Millis(1);
        crate::worker::preflight(&config).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn coordinated_preflight_keeps_enabled_graph_exclusivity_and_compiled_count_bounds() {
        let mut config = profile(sequence(vec![custom("first"), custom("last")]));
        config.mode = Mode::Route;
        config.rules[0].enabled = true;
        config.rules[0].delivery = Delivery::Buffer;
        let mut duplicate = config.rules[0].clone();
        duplicate.id = "duplicate".into();
        config.rules.push(duplicate);
        let error = crate::worker::preflight(&config).unwrap_err();
        assert!(error.to_string().contains("exclusive"), "{error}");
        config.rules.truncate(1);
        config.rules[0].trigger = sequence(vec![custom("first"); 16]);
        config.rules = (0..257)
            .map(|index| {
                let mut rule = config.rules[0].clone();
                rule.id = format!("rule-{index}");
                rule.priority = index;
                rule
            })
            .collect();
        let error = crate::worker::preflight(&config).unwrap_err();
        assert!(error.to_string().contains("4096 compiled"), "{error}");
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Lua {
        handler: String,
        requires: BTreeSet<EffectKind>,
    },
    Event {
        name: String,
    },
    Keys {
        keys: Vec<Key>,
    },
    Remap {
        key: Key,
    },
    Command {
        #[serde(deserialize_with = "bounded_argv")]
        argv: Vec<String>,
    },
}
fn bounded_argv<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    struct Argument(String);
    impl<'de> Deserialize<'de> for Argument {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Text;
            impl<'de> serde::de::Visitor<'de> for Text {
                type Value = Argument;
                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("an argument of at most 4096 bytes without NUL")
                }
                fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Argument, E> {
                    if value.len() > 4096 || value.contains('\0') {
                        return Err(E::custom("command argument exceeds bounds"));
                    }
                    Ok(Argument(value.into()))
                }
                fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Argument, E> {
                    if value.len() > 4096 || value.contains('\0') {
                        return Err(E::custom("command argument exceeds bounds"));
                    }
                    Ok(Argument(value))
                }
            }
            deserializer.deserialize_string(Text)
        }
    }
    struct Arguments;
    impl<'de> serde::de::Visitor<'de> for Arguments {
        type Value = Vec<String>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("at most 256 command arguments")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut result = Vec::new();
            while let Some(argument) = sequence.next_element::<Argument>()? {
                if result.len() == 256 {
                    return Err(serde::de::Error::custom("command argv exceeds 256 entries"));
                }
                result.push(argument.0);
            }
            Ok(result)
        }
    }
    deserializer.deserialize_seq(Arguments)
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    Observations,
    Commands,
    SyntheticInput,
}
impl Action {
    pub fn effect_kind(&self) -> Option<EffectKind> {
        Some(match self {
            Self::Lua { .. } => return None,
            Self::Event { .. } => EffectKind::Observations,
            Self::Command { .. } => EffectKind::Commands,
            Self::Keys { .. } | Self::Remap { .. } => EffectKind::SyntheticInput,
        })
    }
}
fn enabled() -> bool {
    true
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default)]
    pub seats: Vec<String>,
    #[serde(default)]
    pub contexts: Vec<Context>,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub region: Option<Region>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub screen: Option<Screen>,
    #[serde(default)]
    pub require_keys: Vec<Key>,
    #[serde(default)]
    pub keys_throughout: bool,
    #[serde(default)]
    pub layer: Option<String>,
    /// Optional named operating mode; unrelated to Observe/Route authority.
    #[serde(default)]
    pub when_mode: Option<String>,
    #[serde(default)]
    pub delivery: Delivery,
    pub trigger: Trigger,
    pub action: Action,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    pub id: String,
    pub key: Key,
    #[serde(default)]
    pub toggle: bool,
    #[serde(default)]
    pub activation: LayerActivation,
}

/// An approved named operating mode. At most one is selected per seat.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamedMode {
    pub id: String,
    #[serde(default)]
    pub active: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerActivation {
    #[default]
    Physical,
    Active,
    Inactive,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeTemplate {
    pub name: String,
    pub points: Vec<[f64; 2]>,
    pub threshold: f64,
    pub margin: f64,
    #[serde(default)]
    pub rotation_invariant: bool,
    #[serde(default)]
    pub reflection_invariant: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShapeCapture {
    pub max_samples: usize,
    pub timeout: Millis,
    pub minimum_length: Distance,
    pub minimum_size: Distance,
    pub fallback_length: Distance,
    pub fallback_size: Distance,
    pub mouse_minimum_length_counts: f64,
    pub mouse_minimum_size_counts: f64,
}
impl Default for ShapeCapture {
    fn default() -> Self {
        Self {
            max_samples: 2048,
            timeout: Millis(5000),
            minimum_length: Distance::Millimetres(20.0),
            minimum_size: Distance::Millimetres(12.0),
            fallback_length: Distance::Normalized(0.1),
            fallback_size: Distance::Normalized(0.04),
            mouse_minimum_length_counts: 20.0,
            mouse_minimum_size_counts: 12.0,
        }
    }
}
impl ShapeCapture {
    pub fn touch_limits(&self, throughout: bool) -> crate::paths::CaptureLimits {
        crate::paths::CaptureLimits {
            max_samples: self.max_samples,
            timeout: self.timeout.duration(),
            minimum_length: self.minimum_length,
            minimum_size: self.minimum_size,
            fallback_length: self.fallback_length,
            fallback_size: self.fallback_size,
            armed_throughout: throughout,
        }
    }
    pub fn mouse_limits(&self, throughout: bool) -> crate::paths::RelativeLimits {
        crate::paths::RelativeLimits {
            max_samples: self.max_samples,
            timeout: self.timeout.duration(),
            minimum_length: self.mouse_minimum_length_counts,
            minimum_size: self.mouse_minimum_size_counts,
            armed_throughout: throughout,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub timing: Timings,
    #[serde(default)]
    pub shape_capture: ShapeCapture,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub devices: BTreeMap<String, Selector>,
    #[serde(default)]
    pub layers: Vec<Layer>,
    #[serde(default)]
    pub modes: Vec<NamedMode>,
    #[serde(default)]
    pub shapes: Vec<ShapeTemplate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recognizers: Vec<crate::recognition::Definition>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overlay {
    pub version: u32,
    #[serde(default)]
    pub disabled_rules: Vec<String>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug)]
pub struct ConfigError(pub String);
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ConfigError {}

impl Config {
    pub(crate) fn recognizer_definitions(&self) -> Vec<crate::recognition::Definition> {
        let mut definitions = BTreeMap::new();
        for definition in self.recognizers.iter().chain(
            self.rules
                .iter()
                .flat_map(|rule| rule.trigger.recognizers()),
        ) {
            definitions.insert(definition.name.clone(), definition.clone());
        }
        definitions.into_values().collect()
    }
    /// Lua is selected by its explicit file suffix; legacy TOML remains a
    /// compatibility format. Worker snapshots use the compiled TOML model.
    pub fn parse_named(text: &str, name: &str) -> Result<Self, ConfigError> {
        if std::path::Path::new(name)
            .extension()
            .is_some_and(|extension| extension == "lua")
        {
            crate::lua::compile(text, name)
        } else {
            Self::parse(text)
        }
    }
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        if text.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError("configuration exceeds 1 MiB".into()));
        }
        let config: Self = toml::from_str(text).map_err(|error| ConfigError(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<(), ConfigError> {
        let fail = |message: String| Err(ConfigError(message));
        if self.version != SCHEMA_VERSION {
            return fail(format!("unsupported schema version {}", self.version));
        }
        if self.rules.len() > MAX_RULES
            || self.devices.len() > MAX_SELECTORS
            || self.layers.len() > MAX_LAYERS
            || self.shapes.len() > MAX_SHAPES
            || self.recognizers.len() > crate::recognition::MAX_RECOGNIZERS
        {
            return fail("configuration collection limit exceeded".into());
        }
        let compiled: usize = self
            .rules
            .iter()
            .filter(|rule| rule.enabled)
            .map(|rule| recognizer_count(&rule.trigger))
            .sum();
        if compiled > 4096 {
            return fail("configuration exceeds 4096 compiled recognizers".into());
        }
        if crate::Buttons::new(self.timing.buttons()).is_err() {
            return fail("timing: hold_after must exceed positive tap_max/tap_gap".into());
        }
        let capture = &self.shape_capture;
        if !(2..=65536).contains(&capture.max_samples)
            || validate_duration(capture.timeout).is_err()
            || [
                capture.minimum_length,
                capture.minimum_size,
                capture.fallback_length,
                capture.fallback_size,
            ]
            .iter()
            .any(|distance| validate_distance(*distance).is_err())
            || !matches!(capture.fallback_length, Distance::Normalized(_))
            || !matches!(capture.fallback_size, Distance::Normalized(_))
            || [
                capture.mouse_minimum_length_counts,
                capture.mouse_minimum_size_counts,
            ]
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0 || *value > 1e9)
        {
            return fail("shape_capture: invalid sample, timeout, size or fallback limits".into());
        }
        for duration in [
            self.timing.tap_max,
            self.timing.tap_gap,
            self.timing.hold_after,
            self.timing.join_window,
            self.timing.decision_window,
            self.timing.swipe_timeout,
        ] {
            validate_duration(duration).map_err(|error| ConfigError(format!("timing: {error}")))?;
        }
        for distance in [
            self.timing.tap_motion,
            self.timing.pan_slop,
            self.timing.fallback_motion,
        ] {
            validate_distance(distance).map_err(|error| ConfigError(format!("timing: {error}")))?;
        }
        if !matches!(self.timing.fallback_motion, Distance::Normalized(_)) {
            return fail("timing.fallback_motion must use normalized distance".into());
        }
        if self.timing.join_window.0 == 0
            || self.timing.join_window.0 > self.timing.decision_window.0
            || self.timing.decision_window.0 > 5000
        {
            return fail("timing: require 0 < join_window <= decision_window <= 5000ms".into());
        }
        if self.policy.allowed_seats.is_empty()
            || self.policy.allowed_seats.iter().any(|seat| seat.is_empty())
        {
            return fail("policy.allowed_seats must contain nonempty seat identities".into());
        }
        for (id, selector) in &self.devices {
            if id.is_empty() || !selector.constrained() {
                return fail(format!(
                    "devices.{id}: selector requires identity, path or capability constraints"
                ));
            }
            if selector
                .output
                .as_ref()
                .is_some_and(|name| name.is_empty() || name.len() > 256)
            {
                return fail(format!(
                    "devices.{id}.output: use a nonempty output name of at most 256 bytes"
                ));
            }
        }
        for id in &self.policy.allowed_devices {
            if !self.devices.contains_key(id) {
                return fail(format!("policy.allowed_devices: unknown selector {id}"));
            }
        }
        let mut layers = BTreeSet::new();
        for layer in &self.layers {
            if layer.id.is_empty()
                || layer.id.len() > 256
                || layer.id.chars().any(char::is_control)
                || !layers.insert(&layer.id)
            {
                return fail(format!("duplicate/invalid layer {}", layer.id));
            }
        }
        let mut modes = BTreeSet::new();
        if self.modes.len() > MAX_MODES || self.modes.iter().filter(|mode| mode.active).count() > 1
        {
            return fail("modes: at most 64 declarations and one selected mode".into());
        }
        for mode in &self.modes {
            if mode.id.is_empty()
                || mode.id.len() > 256
                || mode.id.chars().any(char::is_control)
                || !modes.insert(&mode.id)
            {
                return fail(format!("duplicate/invalid mode {}", mode.id));
            }
        }
        let mut shapes = BTreeSet::new();
        for shape in &self.shapes {
            if !shapes.insert(&shape.name) || crate::paths::validate_template(shape).is_err() {
                return fail(format!(
                    "shapes.{}: invalid name, points or confidence bounds",
                    shape.name
                ));
            }
        }
        let mut custom = BTreeMap::new();
        for definition in &self.recognizers {
            definition
                .validate()
                .map_err(|error| ConfigError(error.to_string()))?;
            if !self.policy.allow_input_events
                || definition.selectors.iter().any(|id| {
                    !self.devices.contains_key(id) || !self.policy.allowed_devices.contains(id)
                })
                || custom.insert(&definition.name, definition).is_some()
            {
                return fail("recognizers: declarations require unique names, protected input permission and permitted selectors".into());
            }
        }
        let mut ids = BTreeSet::new();
        for rule in &self.rules {
            let prefix = format!("rules.{}", rule.id);
            for recognizer in rule.trigger.recognizers() {
                if recognizer.continuous
                    && rule.delivery != Delivery::Observe
                    && !recognizer.inputs.iter().any(|kind| {
                        matches!(
                            kind,
                            crate::recognition::InputKind::Key
                                | crate::recognition::InputKind::Touch
                        )
                    })
                {
                    return fail(format!(
                        "{prefix}: continuous consuming custom recognition requires a physical key or contact group"
                    ));
                }
                if !self.policy.allow_input_events
                    || recognizer.selectors.iter().any(|selector| {
                        !self.devices.contains_key(selector)
                            || !self.policy.allowed_devices.contains(selector)
                    })
                {
                    return fail(format!(
                        "{prefix}: custom recognizers require protected input permission and permitted selectors"
                    ));
                }
                if custom
                    .insert(&recognizer.name, recognizer)
                    .is_some_and(|old| old != recognizer)
                    || custom.len() > crate::recognition::MAX_RECOGNIZERS
                {
                    return fail(format!(
                        "{prefix}: custom recognizer declarations must be consistent and bounded"
                    ));
                }
            }
            if rule.output.as_ref().is_some_and(|name| {
                name.is_empty() || name.len() > 256 || name.chars().any(char::is_control)
            }) {
                return Err(ConfigError(format!(
                    "{prefix}.output: expected an exact output name of 1–256 bytes without control characters"
                )));
            }
            if rule.id.is_empty() || rule.id.len() > 128 || !ids.insert(&rule.id) {
                return fail(format!("{prefix}: duplicate, empty or oversized rule ID"));
            }
            if rule.require_keys.len() > 32
                || rule.require_keys.iter().collect::<BTreeSet<_>>().len()
                    != rule.require_keys.len()
            {
                return fail(format!(
                    "{prefix}.require_keys: use at most 32 distinct keys"
                ));
            }
            if matches!(rule.action, Action::Remap { .. })
                && !matches!(rule.trigger, Trigger::Key { .. })
            {
                return fail(format!(
                    "{prefix}: remap requires a key press/release trigger"
                ));
            }
            if rule.enabled
                && matches!(rule.trigger, Trigger::Shape { fingers: None, .. })
                && rule.require_keys.is_empty()
                && rule.layer.is_none()
            {
                return fail(format!(
                    "{prefix}: mouse shapes require an arming key or layer"
                ));
            }
            if rule
                .devices
                .iter()
                .any(|id| !self.policy.allowed_devices.contains(id))
                || rule
                    .seats
                    .iter()
                    .any(|seat| !self.policy.allowed_seats.contains(seat))
            {
                return fail(format!(
                    "{prefix}: device/seat permission exceeds system policy"
                ));
            }
            if rule.layer.as_ref().is_some_and(|id| !layers.contains(id)) {
                return fail(format!("{prefix}.layer: unknown layer"));
            }
            if rule
                .when_mode
                .as_ref()
                .is_some_and(|id| !modes.contains(id))
            {
                return fail(format!("{prefix}.when_mode: unknown mode"));
            }
            if rule.delivery == Delivery::Reserve
                && rule.scope == Scope::Everywhere
                && rule.trigger.contains_touch()
                && rule.screen != Some(Screen::Off)
            {
                return fail(format!(
                    "{prefix}.delivery: touch reservation requires an explicitly off display"
                ));
            }
            if let Some(region) = &rule.region {
                validate_region(region, &rule.trigger)
                    .map_err(|message| ConfigError(format!("{prefix}.region: {message}")))?;
            }
            validate_trigger(&rule.trigger, &shapes, 0)
                .map_err(|message| ConfigError(format!("{prefix}.trigger: {message}")))?;
            validate_action(&rule.action)
                .map_err(|message| ConfigError(format!("{prefix}.action: {message}")))?;
            if self.mode == Mode::Route && rule.enabled && self.policy.allowed_devices.is_empty() {
                return fail(format!(
                    "{prefix}: routing requires an explicit system device allowlist"
                ));
            }
        }
        let inactive_layers: BTreeSet<_> = self
            .layers
            .iter()
            .filter(|layer| layer.activation == LayerActivation::Inactive)
            .map(|layer| layer.id.as_str())
            .collect();
        let exclusive = |rule: &Rule| {
            rule.enabled
                && self.mode_active(rule)
                && rule.delivery != Delivery::Observe
                && rule
                    .layer
                    .as_ref()
                    .is_none_or(|id| !inactive_layers.contains(id.as_str()))
        };
        for (index, a) in self
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| exclusive(rule))
        {
            for b in self.rules[index + 1..]
                .iter()
                .filter(|rule| exclusive(rule))
            {
                if a.priority == b.priority && overlaps(a, b) {
                    return fail(format!(
                        "rules.{} and rules.{}: ambiguous exclusive scope at priority {}; assign explicit priorities",
                        a.id, b.id, a.priority
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn mode_active(&self, rule: &Rule) -> bool {
        rule.when_mode
            .as_ref()
            .is_none_or(|id| self.modes.iter().any(|mode| mode.id == *id && mode.active))
    }

    pub fn merge(&self, overlay: Overlay) -> Result<Self, ConfigError> {
        if overlay.version != self.version {
            return Err(ConfigError("user override schema version differs".into()));
        }
        let mut next = self.clone();
        for id in overlay.disabled_rules {
            let rule = next
                .rules
                .iter_mut()
                .find(|rule| rule.id == id)
                .ok_or_else(|| ConfigError(format!("disabled_rules: unknown rule {id}")))?;
            rule.enabled = false;
        }
        for rule in overlay.rules {
            if let Some(index) = next.rules.iter().position(|old| old.id == rule.id) {
                next.rules[index] = rule;
            } else {
                next.rules.push(rule);
            }
        }
        next.validate()?;
        Ok(next)
    }

    pub fn preset(name: &str) -> Result<Self, ConfigError> {
        let mut config = Self {
            recognizers: Vec::new(),
            version: 1,
            mode: Mode::Observe,
            timing: Timings::default(),
            shape_capture: ShapeCapture::default(),
            policy: Policy::default(),
            devices: BTreeMap::new(),
            layers: vec![],
            modes: vec![],
            shapes: vec![],
            rules: vec![],
        };
        match name {
            "minimal" => {}
            "desktop" => {
                config.shapes = crate::paths::demo_templates();
                config.devices.insert(
                    "keyboard".into(),
                    Selector {
                        keys: vec![Key(30)],
                        ..Selector::default()
                    },
                );
                config.policy.allowed_devices.push("keyboard".into());
                config.rules.push(Rule {
                    id: "sample_double_tap".into(),
                    enabled: false,
                    priority: 0,
                    devices: vec!["keyboard".into()],
                    seats: vec![],
                    contexts: vec![],
                    scope: Scope::Everywhere,
                    region: None,
                    output: None,
                    screen: None,
                    require_keys: vec![],
                    keys_throughout: false,
                    layer: None,
                    when_mode: None,
                    delivery: Delivery::Observe,
                    trigger: Trigger::ButtonTaps {
                        key: Key(115),
                        count: 2,
                    },
                    action: Action::Event {
                        name: "volume_double_tap".into(),
                    },
                });
            }
            "phone" => {
                config.shapes = crate::paths::demo_templates();
                config.mode = Mode::Route;
                config.devices.insert(
                    "touch".into(),
                    Selector {
                        absolute_axes: vec![ABS_MT_POSITION_X, ABS_MT_POSITION_Y],
                        properties: vec![1],
                        ..Selector::default()
                    },
                );
                config.policy.allowed_devices.push("touch".into());
            }
            _ => {
                return Err(ConfigError(format!(
                    "unknown preset {name:?}; choose minimal, desktop or phone"
                )));
            }
        }
        config.validate()?;
        Ok(config)
    }
}

pub(crate) fn validate_trigger(
    trigger: &Trigger,
    shapes: &BTreeSet<&String>,
    depth: usize,
) -> Result<(), String> {
    if depth > 8 {
        return Err("sequence nesting exceeds eight levels".into());
    }
    if trigger
        .fingers()
        .is_some_and(|count| count == 0 || count > 64)
    {
        return Err("finger count must be 1..64 and fit selected hardware".into());
    }
    match trigger {
        Trigger::Custom {
            recognizer,
            timeout,
            wake,
        } => {
            recognizer.validate().map_err(|error| error.to_string())?;
            if !(1..=10_000).contains(&timeout.0)
                || wake.is_some_and(|wake| !(5..=250).contains(&wake.0))
            {
                return Err("custom recognition requires a 1..10000 ms timeout and optional 5..250 ms wake interval".into());
            }
            Ok(())
        }
        Trigger::ButtonHold { after, repeat, .. } => {
            validate_duration(*after)?;
            if let Some(repeat) = repeat {
                validate_duration(repeat.delay)?;
                validate_duration(repeat.interval)?;
            }
            Ok(())
        }
        Trigger::TouchHold { after, .. } => validate_duration(*after),
        Trigger::Swipe {
            travel,
            fallback_travel,
            ..
        } => {
            validate_distance(*travel)?;
            validate_distance(*fallback_travel)?;
            if !matches!(fallback_travel, Distance::Normalized(_)) {
                return Err("swipe fallback_travel requires normalized distance".into());
            }
            Ok(())
        }
        Trigger::ButtonTaps { count, .. } | Trigger::TouchTaps { count, .. }
            if *count == 0 || *count > 64 =>
        {
            Err("tap count must be 1..64".into())
        }
        Trigger::Shape { name, .. } if !shapes.contains(name) => {
            Err(format!("unknown shape template {name}"))
        }
        Trigger::Shape {
            fingers,
            button: Some(button),
            ..
        } if fingers.is_some() || !matches!(button.0, 0x110..=0x117) => {
            Err("shape button requires a mouse shape and an ordinary mouse button".into())
        }
        Trigger::Chord { keys, .. }
            if keys.len() < 2
                || keys.len() > 32
                || keys.iter().collect::<BTreeSet<_>>().len() != keys.len() =>
        {
            Err("chord requires 2..32 distinct keys".into())
        }
        Trigger::Pinch { fingers, ratio }
            if *fingers < 2 || !ratio.is_finite() || *ratio <= 0.0 || *ratio >= 1.0 =>
        {
            Err("pinch requires two or more fingers and a positive ratio below one".into())
        }
        Trigger::Rotate { fingers, degrees }
            if *fingers < 2 || !degrees.is_finite() || *degrees <= 0.0 || *degrees > 360.0 =>
        {
            Err("rotation requires two or more fingers and 0 < degrees <= 360".into())
        }
        Trigger::Wheel { minimum, .. } if !minimum.is_finite() || *minimum <= 0.0 => {
            Err("wheel minimum must be positive and finite".into())
        }
        Trigger::Chord { timeout, .. } => validate_duration(*timeout),
        Trigger::Sequence { steps, timeout } => {
            validate_duration(*timeout)?;
            if steps.len() < 2 || steps.len() > 16 {
                return Err("sequence requires 2..16 bounded steps".into());
            }
            if recognizer_count(trigger) > 64 {
                return Err("sequence exceeds 64 recognizer steps".into());
            }
            for step in steps {
                validate_trigger(step, shapes, depth + 1)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
fn validate_duration(duration: Millis) -> Result<(), String> {
    if duration.0 == 0 || duration.0 > 3_600_000 {
        Err("duration must be positive and at most one hour".into())
    } else {
        Ok(())
    }
}
fn recognizer_count(trigger: &Trigger) -> usize {
    fn count(trigger: &Trigger, depth: usize) -> usize {
        if depth > 8 {
            return 4097;
        }
        match trigger {
            Trigger::Sequence { steps, .. } => steps
                .iter()
                .map(|step| count(step, depth + 1))
                .fold(0usize, usize::saturating_add),
            _ => 1,
        }
    }
    count(trigger, 0)
}
fn validate_distance(distance: Distance) -> Result<(), String> {
    if !distance.value().is_finite()
        || distance.value() <= 0.0
        || distance.value()
            > if matches!(distance, Distance::Normalized(_)) {
                2.0
            } else {
                1_000_000.0
            }
    {
        Err("distance outside supported bounds".into())
    } else {
        Ok(())
    }
}
fn validate_region(region: &Region, trigger: &Trigger) -> Result<(), String> {
    if !trigger.contains_touch() {
        return Err("a starting region applies only to touch triggers".into());
    }
    match region.depth {
        Distance::Pixels(depth) if depth > 0.0 && depth.is_finite() => {}
        Distance::Normalized(depth) if depth > 0.0 && depth <= 1.0 => {}
        _ => return Err("depth must be positive px or norm of at most 1".into()),
    }
    if !((0.0..1.0).contains(&region.from) && region.from < region.to && region.to <= 1.0) {
        return Err("from and to must satisfy 0 <= from < to <= 1".into());
    }
    Ok(())
}
pub(crate) fn validate_action(action: &Action) -> Result<(), String> {
    match action {
        Action::Lua { handler, requires }
            if !valid_handler_name(handler) || requires.is_empty() =>
        {
            Err("Lua action requires a valid name and explicit effect capabilities".into())
        }
        Action::Command { argv }
            if argv.is_empty()
                || argv.len() > 256
                || argv
                    .iter()
                    .any(|arg| arg.contains('\0') || arg.len() > 4096)
                || argv[0].is_empty() =>
        {
            Err("command requires a nonempty bounded argv array without NUL bytes".into())
        }
        Action::Keys { keys } if keys.is_empty() || keys.len() > 32 => {
            Err("key action requires 1..32 keys".into())
        }
        Action::Event { name } if name.is_empty() || name.len() > 128 => {
            Err("event action requires a bounded nonempty name".into())
        }
        _ => Ok(()),
    }
}
pub(crate) fn valid_handler_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}
fn overlaps(a: &Rule, b: &Rule) -> bool {
    let intersects = |a: &Vec<String>, b: &Vec<String>| {
        a.is_empty() || b.is_empty() || a.iter().any(|value| b.contains(value))
    };
    // Named selectors can refer to the same physical device. Their names alone
    // cannot establish disjointness; device conflicts are conservative here.
    if !intersects(&a.seats, &b.seats) {
        return false;
    }
    if !a.contexts.is_empty()
        && !b.contexts.is_empty()
        && !a
            .contexts
            .iter()
            .any(|context| b.contexts.contains(context))
    {
        return false;
    }
    if a.screen.is_some() && b.screen.is_some() && a.screen != b.screen {
        return false;
    }
    if let (Some(a), Some(b)) = (a.trigger.fingers(), b.trigger.fingers())
        && a != b
    {
        return false;
    }
    if let (Some(a), Some(b)) = (&a.region, &b.region)
        && disjoint(a, b)
    {
        return false;
    }
    true
}
fn disjoint(a: &Region, b: &Region) -> bool {
    if a.edge == b.edge {
        return a.to <= b.from || b.to <= a.from;
    }
    let opposite = matches!(
        (a.edge, b.edge),
        (Edge::Top, Edge::Bottom)
            | (Edge::Bottom, Edge::Top)
            | (Edge::Left, Edge::Right)
            | (Edge::Right, Edge::Left)
    );
    matches!((a.depth, b.depth), (Distance::Normalized(a), Distance::Normalized(b)) if opposite && a + b < 1.0)
}

/// Validation is transactional. The router must acknowledge a neutral/cancelled
/// boundary before committing this prepared generation to live ownership.
pub struct ConfigManager {
    current: Config,
    generation: u64,
}
impl ConfigManager {
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self {
            current: config,
            generation: 1,
        })
    }
    pub fn current(&self) -> &Config {
        &self.current
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn prepare(&self, text: &str) -> Result<Config, ConfigError> {
        Config::parse(text)
    }
    pub fn commit(&mut self, next: Config) -> Result<u64, ConfigError> {
        next.validate()?;
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| ConfigError("configuration generation exhausted".into()))?;
        self.current = next;
        self.generation = generation;
        Ok(generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t02_unknown_fields_and_units_are_rejected() {
        assert!(Config::parse("version=1\nunknown=true").is_err());
        assert!(Config::parse("version=1\n[timing]\ntap_max='10px'").is_err());
        assert!(Config::parse("version=1\n[timing]\nhold_after='10ms'").is_err());
        for distance in ["NaNmm", "infpx", "-1norm", "1", "0mm"] {
            assert!(Distance::parse(distance).is_err());
        }
    }
    #[test]
    fn starting_regions_are_touch_edge_strips_in_pixels_or_fractions() {
        let parse = |region: &str, trigger: &str| {
            Config::parse(&format!(
                "version = 1\n[policy]\nallowed_devices = ['touch']\n\
                 [devices.touch]\nname = 'Touch'\n[[rules]]\nid = 'edge'\n\
                 devices = ['touch']\ncontexts = ['desktop']\nregion = {region}\n\
                 trigger = {trigger}\naction = {{ kind = 'event', name = 'edge' }}\n"
            ))
        };
        let swipe = "{ kind = 'swipe', fingers = 1, direction = 'up', travel = '12mm', \
                     fallback_travel = '0.1norm' }";
        let config = parse("{ edge = 'bottom', depth = '20px' }", swipe).unwrap();
        assert_eq!(
            config.rules[0].region,
            Some(Region {
                edge: Edge::Bottom,
                depth: Distance::Pixels(20.0),
                from: 0.0,
                to: 1.0,
            })
        );
        parse("{ edge = 'top', depth = '0.05norm', from = 0.5 }", swipe).unwrap();
        for region in [
            "{ edge = 'top', depth = '3mm' }",
            "{ edge = 'top', depth = '2norm' }",
            "{ edge = 'top', depth = '20px', from = 0.5, to = 0.5 }",
            "{ edge = 'top', depth = '20px', to = 1.5 }",
            "{ edge = 'middle', depth = '20px' }",
            "{ edge = 'top', depth = '20px', width = 3 }",
        ] {
            assert!(parse(region, swipe).is_err(), "{region}");
        }
        let pair = |first: &str, second: &str| {
            let rule = |id: &str, region: &str| {
                format!(
                    "[[rules]]\nid = '{id}'\ndevices = ['touch']\ncontexts = ['desktop']\n\
                     delivery = 'buffer'\nregion = {region}\ntrigger = {swipe}\n\
                     action = {{ kind = 'event', name = '{id}' }}\n"
                )
            };
            Config::parse(&format!(
                "version = 1\n[policy]\nallowed_devices = ['touch']\n[devices.touch]\n\
                 name = 'Touch'\n{}{}",
                rule("a", first),
                rule("b", second)
            ))
        };
        pair(
            "{ edge = 'top', depth = '20px', to = 0.5 }",
            "{ edge = 'top', depth = '20px', from = 0.5 }",
        )
        .unwrap();
        pair(
            "{ edge = 'top', depth = '0.1norm' }",
            "{ edge = 'bottom', depth = '0.1norm' }",
        )
        .unwrap();
        for (first, second) in [
            (
                "{ edge = 'top', depth = '20px', to = 0.6 }",
                "{ edge = 'top', depth = '20px', from = 0.5 }",
            ),
            (
                "{ edge = 'top', depth = '20px' }",
                "{ edge = 'bottom', depth = '20px' }",
            ),
            (
                "{ edge = 'bottom', depth = '20px' }",
                "{ edge = 'left', depth = '20px' }",
            ),
        ] {
            assert!(pair(first, second).is_err(), "{first} {second}");
        }
        let key = "{ kind = 'button_taps', key = 'volume_up', count = 2 }";
        let error = parse("{ edge = 'top', depth = '20px' }", key).unwrap_err();
        assert!(
            error
                .0
                .contains("rules.edge.region: a starting region applies only to touch"),
            "{error:?}"
        );
    }
    #[test]
    fn t02_nested_unknown_fields_are_rejected() {
        assert!(Config::parse("version=1\n[devices.keyboard]\nname='test'\nextra=true").is_err());
    }
    #[test]
    fn t03_all_presets_round_trip_through_strict_schema() {
        for name in ["minimal", "desktop", "phone"] {
            let config = Config::preset(name).unwrap();
            let text = toml::to_string_pretty(&config).unwrap();
            assert_eq!(Config::parse(&text).unwrap(), config);
        }
    }
    #[test]
    fn t06_invalid_reload_keeps_last_good_generation() {
        let mut manager = ConfigManager::new(Config::preset("minimal").unwrap()).unwrap();
        assert!(manager.prepare("version=999").is_err());
        assert_eq!(manager.generation(), 1);
        manager
            .commit(manager.prepare("version=1").unwrap())
            .unwrap();
        assert_eq!(manager.generation(), 2);
    }
    #[test]
    fn t08_user_overlay_cannot_change_system_devices_or_policy() {
        assert!(toml::from_str::<Overlay>("version=1\n[policy]\nallowed_seats=['seat1']").is_err());
        let config = Config::preset("desktop").unwrap();
        let mut rule = config.rules[0].clone();
        rule.devices = vec!["another_users_keyboard".into()];
        assert!(
            config
                .merge(Overlay {
                    version: 1,
                    disabled_rules: vec![],
                    rules: vec![rule]
                })
                .is_err()
        );
    }
    #[test]
    fn output_names_round_trip_and_reject_unbounded_names() {
        let mut system = Config::preset("desktop").unwrap();
        system.rules[0].output = Some("panel".into());
        assert_eq!(
            Config::parse(&toml::to_string(&system).unwrap()).unwrap(),
            system
        );
        for bad in ["".to_owned(), "bad\noutput".to_owned(), "x".repeat(257)] {
            let mut config = system.clone();
            config.rules[0].output = Some(bad);
            assert!(config.validate().is_err());
        }
    }
    #[test]
    fn t09_overlapping_exclusive_rules_require_priority() {
        let mut config = Config::preset("desktop").unwrap();
        config.rules[0].enabled = true;
        config.rules[0].delivery = Delivery::Buffer;
        let mut duplicate = config.rules[0].clone();
        duplicate.id = "another".into();
        config.rules.push(duplicate);
        assert!(config.validate().is_err());
        config.rules[1].priority = 1;
        assert!(config.validate().is_ok());
    }
    #[test]
    fn t51_touch_reservation_cannot_capture_an_entire_awake_display() {
        let mut config = Config::preset("phone").unwrap();
        config.rules.push(Rule {
            id: "wake".into(),
            enabled: true,
            priority: 1000,
            devices: vec!["touch".into()],
            seats: vec![],
            contexts: vec![Context::Locked, Context::Greeter],
            scope: Scope::Everywhere,
            region: None,
            output: None,
            screen: Some(Screen::Off),
            require_keys: vec![],
            keys_throughout: false,
            layer: None,
            when_mode: None,
            delivery: Delivery::Reserve,
            trigger: Trigger::TouchTaps {
                fingers: 1,
                count: 2,
            },
            action: Action::Event {
                name: "wake".into(),
            },
        });
        assert!(config.validate().is_ok());
        config.rules[0].screen = Some(Screen::On);
        assert!(config.validate().is_err());
        config.rules[0].screen = None;
        config.rules[0].trigger = Trigger::Sequence {
            steps: vec![
                config.rules[0].trigger.clone(),
                config.rules[0].trigger.clone(),
            ],
            timeout: Millis(5000),
        };
        assert!(config.validate().is_err());
    }
    #[test]
    fn t66_super_meta_and_sup_share_a_key() {
        assert_eq!(Key::parse("Super").unwrap(), Key::parse("Meta").unwrap());
        assert_eq!(Key::parse("Sup").unwrap(), Key(125));
    }
    #[test]
    fn t02_direct_model_construction_cannot_bypass_numeric_validation() {
        let mut config = Config::preset("desktop").unwrap();
        config.timing.swipe_timeout = Millis(0);
        assert!(config.validate().is_err());
        config.timing.swipe_timeout = Millis(1500);
        config.timing.pan_slop = Distance::Millimetres(f64::NAN);
        assert!(config.validate().is_err());
        config.timing.pan_slop = Distance::Millimetres(3.0);
        config.rules[0].trigger = Trigger::ButtonHold {
            key: Key(115),
            after: Millis(0),
            repeat: None,
        };
        assert!(config.validate().is_err());
        config.rules[0].trigger = Trigger::ButtonHold {
            key: Key(115),
            after: Millis(600),
            repeat: Some(Repeat {
                delay: Millis(200),
                interval: Millis(0),
            }),
        };
        assert!(config.validate().is_err());
    }
    #[test]
    fn t02_shape_thresholds_and_geometry_reject_degenerate_models() {
        let mut config = Config::preset("desktop").unwrap();
        config.shapes[0].points = vec![[0.0, 0.0]; 3];
        assert!(config.validate().is_err());
        config.shapes = crate::paths::demo_templates();
        config.shapes[0].threshold = 0.0;
        assert!(config.validate().is_err());
    }
    #[test]
    fn t38_routed_mouse_shapes_need_explicit_arming_and_remaps_need_edges() {
        let mut config = Config::preset("desktop").unwrap();
        config.mode = Mode::Route;
        config.policy.allowed_devices = config.devices.keys().cloned().collect();
        config.rules[0].enabled = true;
        config.rules[0].trigger = Trigger::Shape {
            name: "v".into(),
            fingers: None,
            button: None,
        };
        assert!(config.validate().is_err());
        config.rules[0].require_keys = vec![Key(272)];
        assert!(config.validate().is_ok());
        config.rules[0].action = Action::Remap { key: Key(125) };
        assert!(config.validate().is_err());
        config.rules[0].trigger = Trigger::Key { key: Key(272) };
        assert!(config.validate().is_ok());
    }
    #[test]
    fn shape_capture_limits_and_mouse_arming_are_validated_in_observation() {
        let mut config = Config::preset("desktop").unwrap();
        config.rules[0].enabled = true;
        config.rules[0].trigger = Trigger::Shape {
            name: "v".into(),
            fingers: None,
            button: None,
        };
        assert!(config.validate().is_err());
        config.rules[0].require_keys = vec![Key(0x110)];
        assert!(config.validate().is_ok());
        config.shape_capture.max_samples = 1;
        assert!(config.validate().is_err());
        config.shape_capture.max_samples = 2048;
        config.shape_capture.mouse_minimum_size_counts = f64::NAN;
        assert!(config.validate().is_err());
        assert!(Config::parse("version=1\n[shape_capture]\nunknown=1\n").is_err());
    }
}
