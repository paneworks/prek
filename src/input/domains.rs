use super::*;

/// Indivisible non-key event families within an original report.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum TailDomain {
    Relative = 1,
    Wheel = 2,
    Absolute = 4,
    Switch = 8,
    Touch = 16,
    HorizontalWheel = 32,
    RelativeOther = 64,
    Opaque = 128,
}
impl TailDomain {
    pub(crate) const ALL: [Self; 8] = [
        Self::Relative,
        Self::Wheel,
        Self::Absolute,
        Self::Switch,
        Self::Touch,
        Self::HorizontalWheel,
        Self::RelativeOther,
        Self::Opaque,
    ];

    pub(crate) fn mask(self) -> u8 {
        self as u8
    }
    pub(crate) fn at(events: &[RawEvent], index: usize) -> Option<Self> {
        let event = events[index];
        if independent_event(events, index) || event.kind == EV_SYN && event.code == SYN_REPORT {
            return None;
        }
        Some(match event.kind {
            EV_REL if matches!(event.code, REL_WHEEL | REL_WHEEL_HI_RES) => Self::Wheel,
            EV_REL if matches!(event.code, REL_HWHEEL | REL_HWHEEL_HI_RES) => Self::HorizontalWheel,
            EV_REL if matches!(event.code, 0 | 1) => Self::Relative,
            EV_REL => Self::RelativeOther,
            EV_ABS if event.code == ABS_MT_SLOT || (0x30..=0x3d).contains(&event.code) => {
                Self::Touch
            }
            EV_ABS => Self::Absolute,
            EV_SW => Self::Switch,
            EV_KEY if matches!(event.code, 0x145 | 0x14a | 0x14d..=0x14f) => Self::Touch,
            _ => Self::Opaque,
        })
    }
}
