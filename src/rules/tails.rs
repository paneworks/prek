use super::*;
use crate::{input::*, input_events::Event as Input, recognition::InputKind};

const RELATIVE: u8 = 1;
const WHEEL: u8 = 2;
const ABSOLUTE: u8 = 4;
const SWITCH: u8 = 8;
const TOUCH: u8 = 16;
const HWHEEL: u8 = 32;
const OTHER_RELATIVE: u8 = 64;
const OPAQUE: u8 = 128;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Report {
    pub source: Source,
    pub position: InputPosition,
    domains: u8,
    observed: u8,
    covered: u8,
}
pub(super) type Set = presses::Set<Report>;

impl presses::Entry for Report {
    fn source(&self) -> &Source {
        &self.source
    }
    fn position(&self) -> InputPosition {
        self.position
    }
    fn valid(&self) -> bool {
        self.observed != 0 && self.observed & OPAQUE == 0 && self.covered & !self.domains == 0
    }
    fn combine(&self, other: &Self) -> Result<Self, &'static str> {
        if self.source != other.source
            || self.position != other.position
            || self.domains != other.domains
        {
            return Err("conflicting original report tail");
        }
        let mut combined = self.clone();
        combined.observed |= other.observed;
        combined.covered |= other.covered;
        Ok(combined)
    }
}
impl Report {
    #[cfg(test)]
    pub(crate) fn complete(&self) -> bool {
        self.domains == self.covered
    }
    pub(crate) fn observes(&self, kind: InputKind) -> bool {
        let mask = match kind {
            InputKind::Key => return false,
            InputKind::Relative => RELATIVE | OTHER_RELATIVE,
            InputKind::Wheel => WHEEL | HWHEEL,
            InputKind::Absolute => ABSOLUTE,
            InputKind::Switch => SWITCH,
            InputKind::Touch => TOUCH,
        };
        self.observed & mask != 0
    }
    pub(super) fn declared_by(&self, inputs: &BTreeSet<InputKind>) -> bool {
        [
            InputKind::Relative,
            InputKind::Wheel,
            InputKind::Absolute,
            InputKind::Switch,
            InputKind::Touch,
        ]
        .iter()
        .all(|kind| !self.observes(*kind) || inputs.contains(kind))
    }
    fn from_frame(
        frame: &NormalizedFrame,
        watermark: u64,
        projected: &Input,
    ) -> Result<Option<Self>, &'static str> {
        let Input::Frame {
            source,
            sequence,
            at,
            changes,
            contacts,
            contacts_started,
            contacts_ended,
            ..
        } = projected
        else {
            return Err("non-key provenance requires an original frame");
        };
        if watermark == 0
            || *source != frame.raw.source
            || *sequence != frame.raw.sequence
            || *at != frame.raw.at
            || !frame.raw.events.last().is_some_and(|event| {
                event.kind == EV_SYN && event.code == SYN_REPORT && event.value == 0
            })
        {
            return Err("non-key provenance differs from original frame");
        }
        let mut domains = 0;
        for (index, event) in frame
            .raw
            .events
            .iter()
            .enumerate()
            .take(frame.raw.events.len() - 1)
        {
            if independent_event(&frame.raw.events, index) {
                continue;
            }
            domains |= match event.kind {
                EV_REL if matches!(event.code, REL_WHEEL | REL_WHEEL_HI_RES) => WHEEL,
                EV_REL if matches!(event.code, REL_HWHEEL | REL_HWHEEL_HI_RES) => HWHEEL,
                EV_REL if matches!(event.code, 0 | 1) => RELATIVE,
                EV_REL => OTHER_RELATIVE,
                EV_ABS if event.code == ABS_MT_SLOT || (0x30..=0x3d).contains(&event.code) => TOUCH,
                EV_ABS => ABSOLUTE,
                EV_SW => SWITCH,
                EV_KEY if matches!(event.code, 0x145 | 0x14a | 0x14d..=0x14f) => TOUCH,
                _ => OPAQUE,
            };
        }
        let mut observed = 0;
        let mut covered = 0;
        for change in changes {
            let family = match change {
                Change::Key { .. } => 0,
                Change::Relative { code: 0 | 1, .. } => RELATIVE,
                Change::Relative { .. } => OTHER_RELATIVE,
                Change::Wheel {
                    horizontal: false, ..
                } => WHEEL,
                Change::Wheel {
                    horizontal: true, ..
                } => HWHEEL,
                Change::Absolute { .. } => ABSOLUTE,
                Change::Switch { .. } => SWITCH,
            };
            observed |= family;
            if !matches!(change, Change::Absolute { code, .. }
                if *code == ABS_MT_SLOT || (0x30..=0x3d).contains(code))
            {
                covered |= family;
            }
        }
        if !contacts.is_empty() || !contacts_started.is_empty() || !contacts_ended.is_empty() {
            observed |= TOUCH;
            covered |= TOUCH;
        }
        covered &= domains;
        Ok((domains != 0 && observed != 0).then(|| Self {
            source: source.clone(),
            position: InputPosition {
                frame: watermark,
                event: frame.raw.events.len() - 1,
            },
            domains,
            observed,
            covered,
        }))
    }
}

pub(super) fn record(
    worker: &mut Worker,
    state: &DeviceState,
    frame: &NormalizedFrame,
    projected: &Input,
    events: &mut Vec<Event>,
) -> bool {
    if !worker.routing || !worker.candidates.contains_key(&frame.raw.source) {
        return true;
    }
    let result = Report::from_frame(frame, state.input_watermark, projected).and_then(|report| {
        if let Some(report) = report
            && let Some(candidate) = worker.candidates.get_mut(&frame.raw.source)
        {
            candidate.tails.insert(report)?;
            candidate
                .source_inputs
                .insert(frame.raw.source.clone(), state.input_watermark);
        }
        Ok(())
    });
    if let Err(reason) = result {
        cancel_candidate(worker, &frame.raw.source, frame.raw.at, reason, events);
        false
    } else {
        true
    }
}

pub(super) fn native(
    worker: &mut Worker,
    state: &DeviceState,
    frame: &NormalizedFrame,
    events: &mut Vec<Event>,
) -> bool {
    if !worker.routing || !worker.candidates.contains_key(&frame.raw.source) {
        return true;
    }
    let touch = worker.rule.trigger.contains_touch();
    let changes = frame
        .changes
        .iter()
        .filter(|change| match (&worker.rule.trigger, change) {
            (
                Trigger::Wheel { horizontal, .. },
                Change::Wheel {
                    horizontal: axis, ..
                },
            ) => horizontal == axis,
            (Trigger::Shape { fingers: None, .. }, Change::Relative { code: 0 | 1, .. }) => true,
            _ => false,
        })
        .cloned()
        .collect();
    let projected = Input::Frame {
        source: frame.raw.source.clone(),
        at: frame.raw.at,
        sequence: frame.raw.sequence,
        selectors: Vec::new(),
        changes,
        contacts: if touch {
            frame.contacts.clone()
        } else {
            Vec::new()
        },
        contacts_started: if touch {
            frame.contacts_started.clone()
        } else {
            Vec::new()
        },
        contacts_ended: if touch {
            frame.contacts_ended.clone()
        } else {
            Vec::new()
        },
        ended_contacts: if touch {
            frame.ended_contacts.clone()
        } else {
            Vec::new()
        },
    };
    record(worker, state, frame, &projected, events)
}
