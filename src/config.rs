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
    AppContent,
    Keyboard,
    ContentBottom,
    PhysicalBottom,
    TopLeft,
    TopRight,
    SideLeft,
    SideRight,
    ClockBackground,
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
    /// Protected system permission for output-scoped user-session DPMS effects.
    pub allow_power_actions: bool,
    /// Protected system permission for targeted stock Hyprland window effects.
    pub allow_hyprland_actions: bool,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            allowed_devices: Vec::new(),
            allowed_seats: vec!["seat0".into()],
            allow_runtime_rules: false,
            allow_input_events: false,
            allow_power_actions: false,
            allow_hyprland_actions: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trigger {
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
    pub fn contains_touch(&self) -> bool {
        self.fingers().is_some()
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
    Morf {
        action: MorfAction,
    },
    Hyprland {
        action: WindowAction,
    },
    Power {
        action: PowerAction,
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
    HyprlandActions,
    MorfActions,
    PowerActions,
}
impl Action {
    pub fn requires_hyprland(&self) -> bool {
        matches!(self, Self::Hyprland { .. })
            || matches!(self, Self::Lua { requires, .. } if requires.contains(&EffectKind::HyprlandActions))
    }
    pub fn requires_power(&self) -> bool {
        matches!(self, Self::Power { .. })
            || matches!(self,Self::Lua {requires,..} if requires.contains(&EffectKind::PowerActions))
    }
    pub fn effect_kind(&self) -> Option<EffectKind> {
        Some(match self {
            Self::Lua { .. } => return None,
            Self::Event { .. } => EffectKind::Observations,
            Self::Command { .. } => EffectKind::Commands,
            Self::Keys { .. } | Self::Remap { .. } => EffectKind::SyntheticInput,
            Self::Hyprland { .. } => EffectKind::HyprlandActions,
            Self::Morf { .. } => EffectKind::MorfActions,
            Self::Power { .. } => EffectKind::PowerActions,
        })
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MorfAction {
    Dashboard,
    Notifications,
    QuickSettings,
    WorkspacePreview,
    KeyboardShow,
    KeyboardHide,
    KeyboardToggleLayout,
    Apps,
    Web,
    RevealClockForm,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowAction {
    ReorderUp,
    ReorderDown,
    MoveLeftAndFollow,
    MoveRightAndFollow,
    Close,
    ScrollStack,
}
impl WindowAction {
    pub fn supported_session(self) -> bool {
        matches!(
            self,
            Self::Close | Self::MoveLeftAndFollow | Self::MoveRightAndFollow
        )
    }
    pub fn workspace_destination(self, from: i64) -> Option<i64> {
        let delta = match self {
            Self::MoveLeftAndFollow => -1,
            Self::MoveRightAndFollow => 1,
            _ => return None,
        };
        from.checked_add(delta)
            .filter(|id| (1..=i64::from(i32::MAX)).contains(id))
    }
}
/// Exact bounded stock address plus lifetime identity; never a dispatcher selector.
pub fn hyprland_window_address(id: &str) -> Option<&str> {
    let (address, stable) = id.split_once(':')?;
    let hex = |value: &str| {
        !value.is_empty()
            && value.len() <= 16
            && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            && u64::from_str_radix(value, 16).is_ok_and(|value| value != 0)
    };
    (address.starts_with("0x") && hex(&address[2..]) && hex(stable)).then_some(address)
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    LockAndBlank,
    Wake,
    Blank,
}
pub fn valid_power_output(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
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
    pub output: Option<String>,
    #[serde(default)]
    pub screen: Option<Screen>,
    #[serde(default)]
    pub keyboard_visible: Option<bool>,
    #[serde(default)]
    pub require_keys: Vec<Key>,
    #[serde(default)]
    pub keys_throughout: bool,
    #[serde(default)]
    pub layer: Option<String>,
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
    pub shapes: Vec<ShapeTemplate>,
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
        if self.rules.len() > 1024
            || self.devices.len() > 256
            || self.layers.len() > 64
            || self.shapes.len() > 128
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
        let mut shapes = BTreeSet::new();
        for shape in &self.shapes {
            if !shapes.insert(&shape.name) || crate::paths::validate_template(shape).is_err() {
                return fail(format!(
                    "shapes.{}: invalid name, points or confidence bounds",
                    shape.name
                ));
            }
        }
        let mut ids = BTreeSet::new();
        for rule in &self.rules {
            let prefix = format!("rules.{}", rule.id);
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
            if rule.delivery == Delivery::Reserve
                && matches!(
                    rule.scope,
                    Scope::AppContent
                        | Scope::Keyboard
                        | Scope::ContentBottom
                        | Scope::PhysicalBottom
                        | Scope::TopLeft
                        | Scope::TopRight
                )
            {
                return fail(format!(
                    "{prefix}.delivery: interactive shell/keyboard regions require buffer or observe"
                ));
            }
            if rule.delivery == Delivery::Reserve
                && rule.scope == Scope::Everywhere
                && rule.trigger.contains_touch()
                && rule.screen != Some(Screen::Off)
            {
                return fail(format!(
                    "{prefix}.delivery: touch reservation needs a noninteractive region or an explicitly off display"
                ));
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
            version: 1,
            mode: Mode::Observe,
            timing: Timings::default(),
            shape_capture: ShapeCapture::default(),
            policy: Policy::default(),
            devices: BTreeMap::new(),
            layers: vec![],
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
                    output: None,
                    screen: None,
                    keyboard_visible: None,
                    require_keys: vec![],
                    keys_throughout: false,
                    layer: None,
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
                let definitions = [
                    (
                        "G01",
                        Scope::ContentBottom,
                        1,
                        Direction::Up,
                        Action::Morf {
                            action: MorfAction::Dashboard,
                        },
                    ),
                    (
                        "G02",
                        Scope::TopLeft,
                        1,
                        Direction::Down,
                        Action::Morf {
                            action: MorfAction::Notifications,
                        },
                    ),
                    (
                        "G03",
                        Scope::TopRight,
                        1,
                        Direction::Down,
                        Action::Morf {
                            action: MorfAction::QuickSettings,
                        },
                    ),
                    (
                        "G04",
                        Scope::ContentBottom,
                        1,
                        Direction::Horizontal,
                        Action::Morf {
                            action: MorfAction::WorkspacePreview,
                        },
                    ),
                    (
                        "G05",
                        Scope::PhysicalBottom,
                        2,
                        Direction::Up,
                        Action::Morf {
                            action: MorfAction::KeyboardShow,
                        },
                    ),
                    (
                        "G06",
                        Scope::Keyboard,
                        2,
                        Direction::Down,
                        Action::Morf {
                            action: MorfAction::KeyboardHide,
                        },
                    ),
                    (
                        "G07",
                        Scope::Keyboard,
                        2,
                        Direction::Up,
                        Action::Morf {
                            action: MorfAction::KeyboardToggleLayout,
                        },
                    ),
                    (
                        "G08_up",
                        Scope::AppContent,
                        2,
                        Direction::Up,
                        Action::Hyprland {
                            action: WindowAction::ReorderUp,
                        },
                    ),
                    (
                        "G08_down",
                        Scope::AppContent,
                        2,
                        Direction::Down,
                        Action::Hyprland {
                            action: WindowAction::ReorderDown,
                        },
                    ),
                    (
                        "G09_left",
                        Scope::AppContent,
                        2,
                        Direction::Left,
                        Action::Hyprland {
                            action: WindowAction::MoveLeftAndFollow,
                        },
                    ),
                    (
                        "G09_right",
                        Scope::AppContent,
                        2,
                        Direction::Right,
                        Action::Hyprland {
                            action: WindowAction::MoveRightAndFollow,
                        },
                    ),
                    (
                        "G10",
                        Scope::AppContent,
                        3,
                        Direction::Vertical,
                        Action::Hyprland {
                            action: WindowAction::Close,
                        },
                    ),
                    (
                        "G11_left",
                        Scope::SideLeft,
                        1,
                        Direction::Vertical,
                        Action::Hyprland {
                            action: WindowAction::ScrollStack,
                        },
                    ),
                    (
                        "G11_right",
                        Scope::SideRight,
                        1,
                        Direction::Vertical,
                        Action::Hyprland {
                            action: WindowAction::ScrollStack,
                        },
                    ),
                    (
                        "G14_left",
                        Scope::SideLeft,
                        1,
                        Direction::Right,
                        Action::Morf {
                            action: MorfAction::Apps,
                        },
                    ),
                    (
                        "G14_right",
                        Scope::SideRight,
                        1,
                        Direction::Left,
                        Action::Morf {
                            action: MorfAction::Web,
                        },
                    ),
                ];
                for (index, (id, scope, fingers, direction, action)) in
                    definitions.into_iter().enumerate()
                {
                    let continuous = matches!(
                        action,
                        Action::Morf {
                            action: MorfAction::Dashboard
                                | MorfAction::Notifications
                                | MorfAction::QuickSettings
                                | MorfAction::WorkspacePreview
                        } | Action::Hyprland {
                            action: WindowAction::ScrollStack
                        }
                    );
                    config.rules.push(Rule {
                        id: id.into(),
                        enabled: true,
                        priority: 100 + index as i32,
                        devices: vec!["touch".into()],
                        seats: vec![],
                        contexts: if matches!(scope, Scope::Keyboard | Scope::PhysicalBottom) {
                            vec![Context::Desktop, Context::Locked, Context::Greeter]
                        } else {
                            vec![Context::Desktop]
                        },
                        scope,
                        output: None,
                        screen: Some(Screen::On),
                        keyboard_visible: if scope == Scope::Keyboard {
                            Some(true)
                        } else if id == "G05" {
                            Some(false)
                        } else {
                            None
                        },
                        require_keys: vec![],
                        keys_throughout: false,
                        layer: None,
                        delivery: Delivery::Buffer,
                        trigger: Trigger::Swipe {
                            fingers,
                            direction,
                            travel: Distance::Millimetres(12.0),
                            fallback_travel: Distance::Normalized(0.04),
                            continuous,
                        },
                        action,
                    });
                }
                for (id, scope, screen, action) in [
                    ("G16", Scope::Everywhere, Screen::Off, PowerAction::Wake),
                    (
                        "G17",
                        Scope::ClockBackground,
                        Screen::On,
                        PowerAction::Blank,
                    ),
                ] {
                    config.rules.push(Rule {
                        id: id.into(),
                        enabled: true,
                        priority: 1000,
                        devices: vec!["touch".into()],
                        seats: vec![],
                        contexts: vec![Context::Locked, Context::Greeter],
                        scope,
                        output: None,
                        screen: Some(screen),
                        keyboard_visible: None,
                        require_keys: vec![],
                        keys_throughout: false,
                        layer: None,
                        delivery: Delivery::Reserve,
                        trigger: Trigger::TouchTaps {
                            fingers: 1,
                            count: 2,
                        },
                        action: Action::Power { action },
                    });
                }
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

fn validate_trigger(
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
    if a.screen.is_some() && b.screen.is_some() && a.screen != b.screen
        || a.keyboard_visible.is_some()
            && b.keyboard_visible.is_some()
            && a.keyboard_visible != b.keyboard_visible
    {
        return false;
    }
    // Physical layers can be held together, and overrides can activate both.
    // Different layer names cannot establish disjoint exclusive scopes.
    if matches!(
        (a.scope, b.scope),
        (Scope::AppContent, Scope::Keyboard)
            | (Scope::Keyboard, Scope::AppContent)
            | (Scope::TopLeft, Scope::TopRight)
            | (Scope::TopRight, Scope::TopLeft)
            | (Scope::SideLeft, Scope::SideRight)
            | (Scope::SideRight, Scope::SideLeft)
    ) {
        return false;
    }
    if let (Some(a), Some(b)) = (a.trigger.fingers(), b.trigger.fingers())
        && a != b
    {
        return false;
    }
    true
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
    fn output_names_and_power_policy_round_trip_without_user_policy_expansion() {
        let mut system = Config::preset("desktop").unwrap();
        system.policy.allow_power_actions = true;
        system.policy.allow_hyprland_actions = true;
        system.rules[0].output = Some("panel".into());
        assert_eq!(
            Config::parse(&toml::to_string(&system).unwrap()).unwrap(),
            system
        );
        assert!(
            toml::from_str::<Overlay>("version=1\n[policy]\nallow_power_actions=true").is_err()
        );
        assert!(
            toml::from_str::<Overlay>("version=1\n[policy]\nallow_hyprland_actions=true").is_err()
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
    fn t51_tappable_keyboard_cannot_be_unconditionally_reserved() {
        let mut config = Config::preset("phone").unwrap();
        let rule = config
            .rules
            .iter_mut()
            .find(|rule| rule.id == "G06")
            .unwrap();
        rule.delivery = Delivery::Reserve;
        assert!(config.validate().is_err());
    }
    #[test]
    fn t51_touch_reservation_cannot_capture_an_entire_awake_display() {
        let mut config = Config::preset("phone").unwrap();
        let rule = config
            .rules
            .iter_mut()
            .find(|rule| rule.id == "G16")
            .unwrap();
        rule.screen = Some(Screen::On);
        assert!(config.validate().is_err());
        let rule = config
            .rules
            .iter_mut()
            .find(|rule| rule.id == "G16")
            .unwrap();
        rule.screen = Some(Screen::Off);
        assert!(config.validate().is_ok());
        let rule = config
            .rules
            .iter_mut()
            .find(|rule| rule.id == "G16")
            .unwrap();
        rule.screen = None;
        rule.trigger = Trigger::Sequence {
            steps: vec![rule.trigger.clone(), rule.trigger.clone()],
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
