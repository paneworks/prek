//! Virtual-device preflight contracts. This module performs no kernel I/O.
//!
//! A descriptor is prepared before device creation. Readiness requires evidence
//! from the actual consumer, not merely the existence of an event node. Only a
//! trusted broker may supply that evidence. Serialized descriptors and receipts
//! are copy-only observations, not descriptor access or ownership permission.

use crate::input::{
    ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, Axis, Device, EV_ABS,
    EV_KEY, Source,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

const DEADLINE: Duration = Duration::from_secs(5);
const FRESHNESS: Duration = Duration::from_millis(250);

pub(crate) struct Marker<'a> {
    pub base: &'a str,
    pub publication: Option<&'a str>,
}
impl<'a> Marker<'a> {
    pub fn parse(marker: &'a str) -> Result<Self, &'static str> {
        if marker.len() > 128 {
            return Err("device marker exceeds bounds");
        }
        let parts = marker.split('/').collect::<Vec<_>>();
        let (device, generation, publication) = match parts.as_slice() {
            ["prek", device, generation] => (*device, *generation, None),
            ["prek", "test", device, generation] => (*device, *generation, None),
            ["prek", device, generation, publication] => (*device, *generation, Some(*publication)),
            ["prek", "test", device, generation, publication] => {
                (*device, *generation, Some(*publication))
            }
            _ => return Err("invalid Prek device marker"),
        };
        let hex = |value: &str, length| {
            value.len() == length
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        };
        if !hex(device, 16)
            || !hex(generation, 16)
            || publication
                .is_some_and(|nonce| !hex(nonce, 32) || nonce.bytes().all(|byte| byte == b'0'))
        {
            return Err("device marker requires canonical nonempty hexadecimal identity");
        }
        let base = publication.map_or(marker, |nonce| &marker[..marker.len() - nonce.len() - 1]);
        Ok(Self { base, publication })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KernelIdentity {
    pub bus: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Repeat {
    pub delay_ms: u32,
    pub period_ms: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Keyboard,
    Mouse,
    Touchpad,
    Touchscreen,
    Buttons,
    Switches,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    original: Device,
    isolated: bool,
    pub source: Source,
    pub name: String,
    pub marker: String,
    pub identity: KernelIdentity,
    pub class: Class,
    #[serde(with = "crate::input::numeric_map")]
    pub capabilities: BTreeMap<u16, Vec<u16>>,
    pub properties: Vec<u16>,
    #[serde(with = "crate::input::numeric_map")]
    pub axes: BTreeMap<u16, Axis>,
    pub repeat: Option<Repeat>,
    /// Pre-add registry data. Never import arbitrary original udev properties.
    pub udev: BTreeMap<String, String>,
    /// Required consumer settings, keyed by the virtual identity. Supplied by
    /// trusted device policy; copying capability bits does not preserve these.
    pub consumer_settings: BTreeMap<String, String>,
}

fn bounded(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
}

pub(crate) fn permitted_property(key: &str) -> bool {
    matches!(
        key,
        "ID_INPUT"
            | "ID_INPUT_KEY"
            | "ID_INPUT_KEYBOARD"
            | "ID_INPUT_MOUSE"
            | "ID_INPUT_TOUCHPAD"
            | "ID_INPUT_TOUCHSCREEN"
            | "ID_INPUT_SWITCH"
            | "ID_SEAT"
            | "ID_FOR_SEAT"
            | "WL_OUTPUT"
            | "LIBINPUT_DEVICE_GROUP"
            | "LIBINPUT_CALIBRATION_MATRIX"
            | "LIBINPUT_ATTR_RESOLUTION_HINT"
            | "LIBINPUT_ATTR_SIZE_HINT"
    )
}

impl Descriptor {
    pub(crate) fn isolated_fixture(&self) -> bool {
        self.original.identity.starts_with("test:")
            && self.original.name.starts_with("Prek test ")
            && self.original.physical.starts_with("fixture/")
            && self.original.path.is_empty()
    }

    pub(crate) fn for_isolated(mut self) -> Result<Self, &'static str> {
        self.validate()?;
        self.mark_isolated()?;
        Ok(self)
    }

    pub(crate) fn is_isolated(&self) -> bool {
        self.isolated
    }

    fn mark_isolated(&mut self) -> Result<(), &'static str> {
        if !self.isolated_fixture() {
            return Err("isolated tests accept only explicit synthetic fixture descriptors");
        }
        if !self.isolated {
            self.marker = self.marker.replacen("prek/", "prek/test/", 1);
            if self.udev.contains_key("PREK_PUBLICATION") {
                self.udev
                    .insert("PREK_PUBLICATION".into(), self.marker.clone());
            }
            self.udev.insert("PREK_TEST".into(), "1".into());
            self.udev
                .insert("LIBINPUT_IGNORE_DEVICE".into(), "1".into());
            self.isolated = true;
        }
        Ok(())
    }

    pub(crate) fn bind_publication(
        mut self,
        epoch: crate::protocol::Epoch,
    ) -> Result<Self, &'static str> {
        self.validate()?;
        if Marker::parse(&self.marker)?.publication.is_some() || epoch.0 == [0, 0] {
            return Err("device publication identity cannot be reused");
        }
        self.marker = format!("{}/{:016x}{:016x}", self.marker, epoch.0[0], epoch.0[1]);
        self.udev
            .insert("PREK_PUBLICATION".into(), self.marker.clone());
        self.validate()?;
        Ok(self)
    }
    /// Revalidate a prepared descriptor at the kernel boundary. Public fields
    /// allow observations to be compared, but are not permission to alter the
    /// requested device after its policy checks have passed.
    pub fn validate(&self) -> Result<(), &'static str> {
        let mut expected = Self::mirror(
            &self.original,
            self.identity,
            self.repeat,
            &self.udev,
            self.consumer_settings.clone(),
        )?;
        if self.isolated {
            expected.mark_isolated()?;
        }
        if self.marker != expected.marker {
            let marker = Marker::parse(&self.marker)?;
            if marker.base != expected.marker || marker.publication.is_none() {
                return Err("device marker differs from its source identity");
            }
            expected.marker = self.marker.clone();
            expected
                .udev
                .insert("PREK_PUBLICATION".into(), self.marker.clone());
        }
        if expected != *self {
            return Err("descriptor was changed after preparation");
        }
        Ok(())
    }

    pub fn mirror(
        original: &Device,
        identity: KernelIdentity,
        repeat: Option<Repeat>,
        udev: &BTreeMap<String, String>,
        consumer_settings: BTreeMap<String, String>,
    ) -> Result<Self, &'static str> {
        if original.virtual_device
            || original.physical.starts_with("prek/")
            || original.name.starts_with("Prek virtual ")
        {
            return Err("virtual devices cannot be routed back into Prek");
        }
        if original.source.generation == 0
            || !bounded(&original.source.seat)
            || !bounded(&original.identity)
            || !bounded(&original.name)
        {
            return Err("invalid original device identity");
        }
        if identity.vendor != original.vendor || identity.product != original.product {
            return Err("kernel identity differs from original device");
        }
        for (&kind, codes) in &original.capabilities {
            let max = match kind {
                0 => 0,
                1 => 0x2ff,
                2 => 0xf,
                3 => 0x3f,
                4 => 7,
                5 => 0x10,
                17 => 0xf,
                20 => 1,
                _ => return Err("unsupported event/output capability; leave device ungrabbed"),
            };
            if codes.is_empty()
                || codes.windows(2).any(|pair| pair[0] >= pair[1])
                || codes.iter().any(|code| *code > max)
            {
                return Err("invalid or unsupported capability codes");
            }
        }
        let absolute = original
            .capabilities
            .get(&EV_ABS)
            .cloned()
            .unwrap_or_default();
        if original.axes.len() != absolute.len()
            || absolute
                .iter()
                .any(|code| !original.axes.contains_key(code))
            || original.axes.values().any(|axis| {
                axis.minimum > axis.maximum || axis.resolution < 0 || axis.fuzz < 0 || axis.flat < 0
            })
            || original.properties.iter().any(|property| *property > 6)
            || original
                .properties
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err("invalid axis or input-property metadata");
        }
        if original.capabilities.get(&EV_KEY).is_some_and(|codes| {
            codes
                .iter()
                .any(|code| matches!(code, 0x120..=0x144 | 0x146..=0x147 | 0x149 | 0x14b..=0x14c))
        }) {
            return Err("stylus or controller interception is not supported");
        }
        let direct = original.properties.contains(&1);
        let pointer = original.properties.contains(&0);
        let class = if direct && !pointer && !absolute.is_empty() {
            if original.touch_capacity().is_none()
                || original.axes[&ABS_MT_SLOT].minimum != 0
                || [ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_TRACKING_ID]
                    .iter()
                    .any(|code| !original.axes.contains_key(code))
            {
                return Err("touch routing requires bounded zero-based Type B slots");
            }
            Class::Touchscreen
        } else if pointer && !absolute.is_empty() {
            if original.touch_capacity().is_none()
                || original.axes[&ABS_MT_SLOT].minimum != 0
                || [ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_TRACKING_ID]
                    .iter()
                    .any(|code| !original.axes.contains_key(code))
            {
                return Err("touchpad routing requires Type B slots");
            }
            Class::Touchpad
        } else if !absolute.is_empty() {
            return Err("unclassified absolute input cannot be intercepted");
        } else if original.capabilities.contains_key(&2) {
            Class::Mouse
        } else if original.has(EV_KEY, 30) {
            Class::Keyboard
        } else if original.capabilities.contains_key(&EV_KEY) {
            Class::Buttons
        } else if original.capabilities.contains_key(&5) {
            Class::Switches
        } else {
            return Err("device has no supported input capabilities");
        };
        if original.capabilities.contains_key(&20) != repeat.is_some()
            || original
                .capabilities
                .get(&20)
                .is_some_and(|codes| codes.as_slice() != [0, 1])
            || repeat.is_some_and(|value| value.period_ms > 60_000 || value.delay_ms > 60_000)
        {
            return Err("repeat capabilities require verified repeat settings");
        }
        if consumer_settings.len() > 64
            || consumer_settings
                .iter()
                .any(|(key, value)| !bounded(key) || !bounded(value))
        {
            return Err("invalid consumer settings");
        }
        let marker = format!(
            "prek/{:016x}/{:016x}",
            original.source.device, original.source.generation
        );
        let mut properties: BTreeMap<_, _> = udev
            .iter()
            .filter(|(key, _)| permitted_property(key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if properties.values().any(|value| !bounded(value)) {
            return Err("invalid allowed udev property");
        }
        if properties
            .get("ID_SEAT")
            .is_some_and(|seat| seat != &original.source.seat)
        {
            return Err("udev seat differs from source seat");
        }
        properties.insert("ID_SEAT".into(), original.source.seat.clone());
        properties.insert("PREK_VIRTUAL".into(), "1".into());
        // Linux input names are limited to 80 bytes including the trailing NUL.
        let prefix = format!("Prek virtual {:016x} ", original.source.device);
        let mut name = prefix;
        for character in original.name.chars() {
            if name.len() + character.len_utf8() > 79 {
                break;
            }
            name.push(character);
        }
        Ok(Self {
            original: original.clone(),
            isolated: false,
            source: original.source.clone(),
            name,
            marker,
            identity,
            class,
            capabilities: original.capabilities.clone(),
            properties: original.properties.clone(),
            axes: original.axes.clone(),
            repeat,
            udev: properties,
            consumer_settings,
        })
    }

    pub fn feedback_required(&self) -> bool {
        self.capabilities.contains_key(&17) || self.repeat.is_some()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Consumer {
    Hyprland {
        pid: u32,
        start: u64,
        instance: String,
    },
    ConsoleKeyboard {
        handler: String,
    },
    /// Only for an explicitly isolated test harness, never a readiness shortcut
    /// for a compositor that has not acknowledged the device.
    IsolatedReceiver {
        pid: u32,
        start: u64,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerEvidence {
    pub consumer: Consumer,
    pub marker: String,
    pub name: String,
    pub class: Class,
    pub seat: String,
    pub settings: BTreeMap<String, String>,
    pub at: Duration,
}

#[derive(Debug)]
pub struct Readiness {
    descriptor: Descriptor,
    consumer: Consumer,
    started: Duration,
    last_at: Duration,
    metadata_at: Option<Duration>,
    consumer_at: Option<Duration>,
    feedback: bool,
    neutral_at: Option<Duration>,
    failed: bool,
}

/// Private fields prevent callers from manufacturing a preflight success.
/// The kernel backend must still bind this token to its own prepared descriptors.
#[derive(Debug)]
pub struct Ready {
    original: Device,
    source: Source,
    marker: String,
    at: Duration,
}

impl Ready {
    pub fn matches(&self, device: &Device) -> bool {
        &self.original == device
    }
    pub fn source(&self) -> &Source {
        &self.source
    }
    pub fn marker(&self) -> &str {
        &self.marker
    }
    pub fn valid_at(&self, at: Duration) -> bool {
        at >= self.at && at - self.at <= FRESHNESS
    }
}

impl Readiness {
    pub fn new(
        descriptor: Descriptor,
        consumer: Consumer,
        at: Duration,
    ) -> Result<Self, &'static str> {
        descriptor.validate()?;
        let valid = match &consumer {
            Consumer::Hyprland {
                pid,
                start,
                instance,
            } => *pid > 0 && *start > 0 && bounded(instance),
            Consumer::ConsoleKeyboard { handler } => {
                descriptor.class == Class::Keyboard
                    && bounded(handler)
                    && handler.starts_with("kbd")
            }
            Consumer::IsolatedReceiver { pid, start } => *pid > 0 && *start > 0,
        };
        if !valid {
            return Err("invalid or incompatible consumer identity");
        }
        let feedback = !descriptor.feedback_required();
        Ok(Self {
            descriptor,
            consumer,
            started: at,
            last_at: at,
            metadata_at: None,
            consumer_at: None,
            feedback,
            neutral_at: None,
            failed: false,
        })
    }

    fn advance(&mut self, at: Duration) -> Result<(), &'static str> {
        if self.failed {
            return Err("preflight already failed; discard prepared descriptors");
        }
        if at < self.last_at || at.saturating_sub(self.started) >= DEADLINE {
            self.failed = true;
            return Err("readiness deadline or monotonic timeline violated");
        }
        self.last_at = at;
        Ok(())
    }

    pub fn metadata(&mut self, observed: &Descriptor, at: Duration) -> Result<(), &'static str> {
        self.advance(at)?;
        if observed != &self.descriptor {
            self.failed = true;
            return Err("created device/processed metadata differs from prepared descriptor");
        }
        self.metadata_at = Some(at);
        Ok(())
    }

    pub fn consumer(
        &mut self,
        observed: &ConsumerEvidence,
        at: Duration,
    ) -> Result<(), &'static str> {
        self.advance(at)?;
        if observed.at > at
            || at - observed.at > FRESHNESS
            || observed.consumer != self.consumer
            || observed.marker != self.descriptor.marker
            || observed.name != self.descriptor.name
            || observed.class != self.descriptor.class
            || observed.seat != self.descriptor.source.seat
            || observed.settings != self.descriptor.consumer_settings
        {
            self.failed = true;
            return Err("consumer acknowledgement is stale or mismatched");
        }
        self.consumer_at = Some(observed.at);
        Ok(())
    }

    /// Set only after the backend has installed and verified its reverse LED /
    /// repeat path. Merely advertising EV_LED does not preserve feedback.
    pub fn feedback(&mut self, verified: bool, at: Duration) -> Result<(), &'static str> {
        self.advance(at)?;
        self.feedback = verified || !self.descriptor.feedback_required();
        Ok(())
    }

    pub fn neutral(&mut self, verified: bool, at: Duration) -> Result<(), &'static str> {
        self.advance(at)?;
        self.neutral_at = verified.then_some(at);
        Ok(())
    }

    pub fn finish(mut self, at: Duration) -> Result<Ready, &'static str> {
        self.advance(at)?;
        if self.metadata_at.is_none()
            || !self.feedback
            || self.neutral_at.is_none_or(|time| at - time > FRESHNESS)
            || self.consumer_at.is_none_or(|time| at - time > FRESHNESS)
        {
            return Err("metadata, live consumer, feedback and neutral handoff are all required");
        }
        Ok(Ready {
            original: self.descriptor.original,
            source: self.descriptor.source,
            marker: self.descriptor.marker,
            at,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::input::{ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_TRACKING_ID};

    pub(crate) fn touch() -> Device {
        let axis = Axis {
            minimum: 0,
            maximum: 4095,
            resolution: 20,
            fuzz: 2,
            flat: 0,
        };
        Device {
            identity: "usb:test:touch".into(),
            source: Source {
                device: 1,
                generation: 1,
                seat: "seat0".into(),
            },
            name: "Touché touchscreen".into(),
            path: "/dev/input/event99".into(),
            physical: "usb/test".into(),
            unique: "test".into(),
            vendor: 1,
            product: 2,
            properties: vec![1],
            capabilities: BTreeMap::from([
                (0, vec![0]),
                (1, vec![0x14a]),
                (
                    3,
                    vec![
                        ABS_MT_SLOT,
                        ABS_MT_POSITION_X,
                        ABS_MT_POSITION_Y,
                        ABS_MT_TRACKING_ID,
                    ],
                ),
            ]),
            axes: BTreeMap::from([
                (
                    ABS_MT_SLOT,
                    Axis {
                        maximum: 9,
                        resolution: 0,
                        fuzz: 0,
                        ..axis
                    },
                ),
                (ABS_MT_POSITION_X, axis),
                (ABS_MT_POSITION_Y, axis),
                (
                    ABS_MT_TRACKING_ID,
                    Axis {
                        maximum: 65535,
                        ..axis
                    },
                ),
            ]),
            virtual_device: false,
        }
    }

    fn descriptor() -> Descriptor {
        Descriptor::mirror(
            &touch(),
            KernelIdentity {
                bus: 3,
                vendor: 1,
                product: 2,
                version: 7,
            },
            None,
            &BTreeMap::from([
                ("ID_INPUT_TOUCHSCREEN".into(), "1".into()),
                ("RUN".into(), "untrusted".into()),
                ("WL_OUTPUT".into(), "DSI-1".into()),
            ]),
            BTreeMap::from([
                ("output".into(), "DSI-1".into()),
                ("rotation".into(), "90".into()),
            ]),
        )
        .unwrap()
    }

    fn consumer() -> Consumer {
        Consumer::Hyprland {
            pid: 10,
            start: 20,
            instance: "verified-instance".into(),
        }
    }
    fn evidence(spec: &Descriptor) -> ConsumerEvidence {
        ConsumerEvidence {
            consumer: consumer(),
            marker: spec.marker.clone(),
            name: spec.name.clone(),
            class: spec.class,
            seat: "seat0".into(),
            settings: spec.consumer_settings.clone(),
            at: Duration::ZERO,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_transport_messages_preserve_descriptor_and_consumer_evidence() {
        let spec = descriptor();
        let fence = crate::protocol::Fence {
            broker: crate::protocol::Epoch([1, 2]),
            configuration: 3,
            session: 4,
        };
        let message = crate::worker::Message::Transport {
            subscription: crate::worker::Subscription {
                seat: spec.source.seat.clone(),
                fence,
                context: None,
                capabilities: Default::default(),
                consumer: Some(consumer()),
            },
            descriptor: Box::new(spec.clone()),
        };
        let encoded = serde_json::to_vec(&message).unwrap();
        let crate::worker::Message::Transport { descriptor, .. } =
            serde_json::from_slice(&encoded).unwrap()
        else {
            panic!("private message kind changed");
        };
        assert_eq!(*descriptor, spec);
        descriptor.validate().unwrap();

        let expected = evidence(&spec);
        let command = crate::configuration::Command::Transport {
            admission: None,
            fence,
            source: spec.source.clone(),
            evidence: expected.clone(),
        };
        let encoded = serde_json::to_vec(&command).unwrap();
        let crate::configuration::Command::Transport { evidence, .. } =
            serde_json::from_slice(&encoded).unwrap()
        else {
            panic!("private command kind changed");
        };
        assert_eq!(evidence, expected);
    }

    #[test]
    fn deserialized_descriptors_still_require_mirror_validation() {
        let mut encoded = serde_json::to_value(descriptor()).unwrap();
        encoded["name"] = "unverified replacement".into();
        let changed: Descriptor = serde_json::from_value(encoded.clone()).unwrap();
        assert!(changed.validate().is_err());
        encoded["unrecognized_authority"] = true.into();
        assert!(serde_json::from_value::<Descriptor>(encoded).is_err());
    }

    #[test]
    fn mirror_preserves_direct_touch_axes_identity_and_allowed_metadata() {
        let spec = descriptor();
        assert_eq!(spec.axes, touch().axes);
        assert_eq!(spec.capabilities, touch().capabilities);
        assert_eq!(spec.identity.version, 7);
        assert_eq!(spec.class, Class::Touchscreen);
        assert_eq!(spec.properties, vec![1]);
        assert_eq!(spec.udev["WL_OUTPUT"], "DSI-1");
        assert_eq!(spec.udev["PREK_VIRTUAL"], "1");
        assert!(!spec.udev.contains_key("RUN"));
    }

    #[test]
    fn readiness_cannot_accept_a_descriptor_mutated_after_preparation() {
        let mut changed = descriptor();
        changed.name = "another device".into();
        assert!(Readiness::new(changed, consumer(), Duration::ZERO).is_err());
    }

    #[test]
    fn feedback_and_unsupported_classes_are_not_silently_discarded() {
        let mut device = touch();
        let id = descriptor().identity;
        device.capabilities.insert(21, vec![0x50]);
        assert!(Descriptor::mirror(&device, id, None, &BTreeMap::new(), BTreeMap::new()).is_err());
        device.capabilities.remove(&21);
        device.capabilities.get_mut(&EV_KEY).unwrap().push(0x14b);
        assert!(Descriptor::mirror(&device, id, None, &BTreeMap::new(), BTreeMap::new()).is_err());
        device.capabilities.insert(EV_KEY, vec![30]);
        device.capabilities.insert(17, vec![0]);
        let spec =
            Descriptor::mirror(&device, id, None, &BTreeMap::new(), BTreeMap::new()).unwrap();
        assert!(spec.feedback_required());
        let mut gate = Readiness::new(spec.clone(), consumer(), Duration::ZERO).unwrap();
        gate.metadata(&spec, Duration::ZERO).unwrap();
        gate.consumer(&evidence(&spec), Duration::ZERO).unwrap();
        gate.neutral(true, Duration::ZERO).unwrap();
        assert!(gate.finish(Duration::ZERO).is_err());
    }

    #[test]
    fn virtual_devices_and_invalid_axes_refuse_preflight() {
        let mut device = touch();
        device.physical = "prek/already-virtual".into();
        assert!(
            Descriptor::mirror(
                &device,
                descriptor().identity,
                None,
                &BTreeMap::new(),
                BTreeMap::new()
            )
            .is_err()
        );
        device.physical.clear();
        device.axes.remove(&ABS_MT_POSITION_X);
        assert!(
            Descriptor::mirror(
                &device,
                descriptor().identity,
                None,
                &BTreeMap::new(),
                BTreeMap::new()
            )
            .is_err()
        );
    }

    #[test]
    fn event_node_alone_and_wrong_compositor_never_authorize_grab() {
        let spec = descriptor();
        let mut gate = Readiness::new(spec.clone(), consumer(), Duration::ZERO).unwrap();
        gate.metadata(&spec, Duration::ZERO).unwrap();
        gate.neutral(true, Duration::ZERO).unwrap();
        assert!(gate.finish(Duration::ZERO).is_err());
        let mut gate = Readiness::new(spec.clone(), consumer(), Duration::ZERO).unwrap();
        let mut ack = evidence(&spec);
        ack.consumer = Consumer::Hyprland {
            pid: 10,
            start: 21,
            instance: "verified-instance".into(),
        };
        assert!(gate.consumer(&ack, Duration::ZERO).is_err());
        assert!(gate.metadata(&spec, Duration::ZERO).is_err());
    }

    #[test]
    fn output_mapping_and_created_axes_must_match_exactly() {
        let spec = descriptor();
        let mut gate = Readiness::new(spec.clone(), consumer(), Duration::ZERO).unwrap();
        let mut ack = evidence(&spec);
        ack.settings.insert("output".into(), "other-output".into());
        assert!(gate.consumer(&ack, Duration::ZERO).is_err());
        let mut gate = Readiness::new(spec.clone(), consumer(), Duration::ZERO).unwrap();
        let mut observed = spec.clone();
        observed
            .axes
            .get_mut(&ABS_MT_POSITION_X)
            .unwrap()
            .resolution = 0;
        assert!(gate.metadata(&observed, Duration::ZERO).is_err());
    }

    #[test]
    fn neutral_handoff_fresh_ack_and_five_second_deadline_are_required() {
        let spec = descriptor();
        let mut gate = Readiness::new(spec.clone(), consumer(), Duration::ZERO).unwrap();
        gate.metadata(&spec, Duration::ZERO).unwrap();
        gate.consumer(&evidence(&spec), Duration::ZERO).unwrap();
        gate.neutral(true, Duration::ZERO).unwrap();
        let ready = gate.finish(Duration::from_millis(250)).unwrap();
        assert_eq!(ready.source(), &touch().source);
        assert_eq!(ready.marker(), spec.marker);
        let mut gate = Readiness::new(spec.clone(), consumer(), Duration::ZERO).unwrap();
        gate.metadata(&spec, Duration::ZERO).unwrap();
        gate.consumer(&evidence(&spec), Duration::ZERO).unwrap();
        gate.neutral(true, Duration::ZERO).unwrap();
        assert!(gate.finish(Duration::from_millis(251)).is_err());
        let mut gate = Readiness::new(spec, consumer(), Duration::ZERO).unwrap();
        assert!(gate.neutral(true, Duration::from_secs(5)).is_err());
    }

    #[test]
    fn touchpad_tool_flags_are_preserved_and_names_remain_valid_utf8() {
        let mut device = touch();
        device.name = "é".repeat(80);
        device.properties = vec![0];
        device
            .capabilities
            .insert(EV_KEY, vec![0x145, 0x148, 0x14a, 0x14d, 0x14e, 0x14f]);
        let spec = Descriptor::mirror(
            &device,
            descriptor().identity,
            None,
            &BTreeMap::new(),
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(spec.class, Class::Touchpad);
        assert_eq!(spec.capabilities[&EV_KEY], device.capabilities[&EV_KEY]);
        assert!(spec.name.len() <= 79);
        assert!(spec.name.ends_with('é'));
    }

    #[test]
    fn repeat_settings_require_capability_and_reverse_path_verification() {
        let mut device = touch();
        device.capabilities.insert(20, vec![0, 1]);
        let repeat = Repeat {
            delay_ms: 400,
            period_ms: 30,
        };
        assert!(
            Descriptor::mirror(
                &device,
                descriptor().identity,
                None,
                &BTreeMap::new(),
                BTreeMap::new()
            )
            .is_err()
        );
        let spec = Descriptor::mirror(
            &device,
            descriptor().identity,
            Some(repeat),
            &BTreeMap::new(),
            BTreeMap::new(),
        )
        .unwrap();
        let mut gate = Readiness::new(spec.clone(), consumer(), Duration::ZERO).unwrap();
        gate.metadata(&spec, Duration::ZERO).unwrap();
        gate.consumer(&evidence(&spec), Duration::ZERO).unwrap();
        gate.neutral(true, Duration::ZERO).unwrap();
        gate.feedback(true, Duration::ZERO).unwrap();
        let ready = gate.finish(Duration::ZERO).unwrap();
        assert!(ready.matches(&device));
        assert!(!ready.valid_at(Duration::from_millis(251)));
        device.axes.get_mut(&ABS_MT_POSITION_X).unwrap().maximum += 1;
        assert!(!ready.matches(&device));
    }

    #[test]
    fn repeat_capabilities_require_both_implicit_settings() {
        let mut device = touch();
        for codes in [vec![0], vec![1]] {
            device.capabilities.insert(20, codes);
            for repeat in [
                Repeat {
                    delay_ms: 0,
                    period_ms: 0,
                },
                Repeat {
                    delay_ms: 250,
                    period_ms: 33,
                },
            ] {
                assert!(
                    Descriptor::mirror(
                        &device,
                        descriptor().identity,
                        Some(repeat),
                        &BTreeMap::new(),
                        BTreeMap::new()
                    )
                    .is_err()
                );
            }
        }
        device.capabilities.insert(20, vec![0, 1]);
        for (delay_ms, period_ms, allowed) in [
            (0, 0, true),
            (0, 33, true),
            (250, 0, true),
            (60_000, 60_000, true),
            (60_001, 33, false),
            (250, 60_001, false),
        ] {
            assert_eq!(
                Descriptor::mirror(
                    &device,
                    descriptor().identity,
                    Some(Repeat {
                        delay_ms,
                        period_ms
                    }),
                    &BTreeMap::new(),
                    BTreeMap::new()
                )
                .is_ok(),
                allowed
            );
        }
    }
}
