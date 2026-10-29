//! Contact grouping and geometry, independent of device access and rendering.
use crate::{
    config::{Direction, Distance, Timings},
    input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, Axis, CancelReason, ContactId, Device,
        NormalizedFrame, Source,
    },
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}
impl Point {
    pub fn length(self) -> f64 {
        self.x.hypot(self.y)
    }
    pub fn minus(self, other: Self) -> Self {
        Self {
            x: self.x - other.x,
            y: self.y - other.y,
        }
    }
    pub fn scale(self, value: f64) -> Self {
        Self {
            x: self.x * value,
            y: self.y * value,
        }
    }
    fn finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transform {
    #[default]
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}
impl Transform {
    fn apply(self, p: Point) -> Point {
        match self {
            Self::Normal => p,
            Self::Rotate90 => Point {
                x: 1.0 - p.y,
                y: p.x,
            },
            Self::Rotate180 => Point {
                x: 1.0 - p.x,
                y: 1.0 - p.y,
            },
            Self::Rotate270 => Point {
                x: p.y,
                y: 1.0 - p.x,
            },
            Self::Flipped => Point {
                x: 1.0 - p.x,
                y: p.y,
            },
            Self::Flipped90 => Point {
                x: 1.0 - p.y,
                y: 1.0 - p.x,
            },
            Self::Flipped180 => Point {
                x: p.x,
                y: 1.0 - p.y,
            },
            Self::Flipped270 => Point { x: p.y, y: p.x },
        }
    }
    pub(crate) fn swapped(self) -> bool {
        matches!(
            self,
            Self::Rotate90 | Self::Rotate270 | Self::Flipped90 | Self::Flipped270
        )
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    pub name: String,
    /// Untransformed native pixel dimensions; rotation is applied exactly once.
    pub pixel_width: f64,
    pub pixel_height: f64,
    pub scale: f64,
    pub origin: Point,
    pub transform: Transform,
}
impl Output {
    pub fn logical_size(&self) -> Point {
        let (width, height) = if self.transform.swapped() {
            (self.pixel_height, self.pixel_width)
        } else {
            (self.pixel_width, self.pixel_height)
        };
        Point {
            x: width / self.scale,
            y: height / self.scale,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub generation: u64,
    pub x: Axis,
    pub y: Axis,
    pub calibration: [[f64; 3]; 2],
    /// None permits generic normalized observation, never pixel-based actions.
    pub output: Option<Output>,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Position {
    pub normalized: Point,
    pub millimetres: Option<Point>,
    pub local: Option<Point>,
    pub global: Option<Point>,
}
impl Position {
    /// Logical output coordinates when mapped, otherwise normalized device coordinates.
    pub fn point(self) -> Point {
        self.local.unwrap_or(self.normalized)
    }
}
impl Mapping {
    /// The unmapped generation-0 mapping of a device's contact axes; any output
    /// mapping, whose generation is its geometry generation, advances it.
    pub fn from_device(device: &Device) -> Result<Self, &'static str> {
        let mapping = Self {
            generation: 0,
            x: *device
                .axes
                .get(&ABS_MT_POSITION_X)
                .ok_or("touch X axis metadata unavailable")?,
            y: *device
                .axes
                .get(&ABS_MT_POSITION_Y)
                .ok_or("touch Y axis metadata unavailable")?,
            calibration: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            output: None,
        };
        mapping.validate()?;
        Ok(mapping)
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.x.maximum <= self.x.minimum
            || self.y.maximum <= self.y.minimum
            || self
                .calibration
                .iter()
                .flatten()
                .any(|value| !value.is_finite())
        {
            return Err("invalid axis range or calibration");
        }
        if let Some(output) = &self.output
            && (output.name.is_empty()
                || !output.origin.finite()
                || !output.pixel_width.is_finite()
                || !output.pixel_height.is_finite()
                || !output.scale.is_finite()
                || output.pixel_width <= 0.0
                || output.pixel_height <= 0.0
                || output.scale <= 0.0
                || !output.logical_size().finite()
                || output.logical_size().x > 1_000_000.0
                || output.logical_size().y > 1_000_000.0
                || output.origin.x.abs() > 1_000_000.0
                || output.origin.y.abs() > 1_000_000.0)
        {
            return Err("invalid output dimensions, origin or scale");
        }
        if self
            .calibration
            .iter()
            .flatten()
            .any(|value| value.abs() > 16.0)
        {
            return Err("calibration coefficients exceed supported bounds");
        }
        let determinant = self.calibration[0][0] * self.calibration[1][1]
            - self.calibration[0][1] * self.calibration[1][0];
        if !determinant.is_finite() || determinant.abs() < 1e-12 {
            return Err("singular calibration matrix");
        }
        Ok(())
    }
    pub fn map(&self, x: i32, y: i32) -> Result<Position, &'static str> {
        if !(self.x.minimum..=self.x.maximum).contains(&x)
            || !(self.y.minimum..=self.y.maximum).contains(&y)
        {
            return Err("touch position outside reported axis bounds");
        }
        let raw = Point {
            x: (f64::from(x) - f64::from(self.x.minimum))
                / (f64::from(self.x.maximum) - f64::from(self.x.minimum)),
            y: (f64::from(y) - f64::from(self.y.minimum))
                / (f64::from(self.y.maximum) - f64::from(self.y.minimum)),
        };
        let calibrated = Point {
            x: self.calibration[0][0] * raw.x
                + self.calibration[0][1] * raw.y
                + self.calibration[0][2],
            y: self.calibration[1][0] * raw.x
                + self.calibration[1][1] * raw.y
                + self.calibration[1][2],
        };
        if !calibrated.finite() {
            return Err("calibration overflow");
        }
        let millimetres = if self.x.resolution > 0 && self.y.resolution > 0 {
            Some(Point {
                x: (f64::from(x) - f64::from(self.x.minimum)) / f64::from(self.x.resolution),
                y: (f64::from(y) - f64::from(self.y.minimum)) / f64::from(self.y.resolution),
            })
        } else {
            None
        };
        let (normalized, local, global) = if let Some(output) = &self.output {
            let normalized = output.transform.apply(calibrated);
            let size = output.logical_size();
            // Device axes include their maximum; compositor rectangles are
            // half-open. Keep the exact hardware edge just inside that output
            // without clamping deliberately out-of-output calibration values.
            let project = |value: f64, origin: f64, length: f64| {
                let coordinate = origin + value * length;
                if value == 1.0 {
                    coordinate.min((origin + length).next_down())
                } else {
                    coordinate
                }
            };
            let local = Point {
                x: project(normalized.x, 0.0, size.x),
                y: project(normalized.y, 0.0, size.y),
            };
            (
                normalized,
                Some(local),
                Some(Point {
                    x: project(normalized.x, output.origin.x, size.x),
                    y: project(normalized.y, output.origin.y, size.y),
                }),
            )
        } else {
            (calibrated, None, None)
        };
        Ok(Position {
            normalized,
            millimetres,
            local,
            global,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Begin,
    Update,
    End,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Space {
    Normalized,
    Logical,
    Millimetres,
    Relative,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TouchEvent {
    pub source: Source,
    pub id: u64,
    pub at: Duration,
    pub space: Space,
    pub gesture: TouchGesture,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TouchGesture {
    Taps {
        fingers: u16,
        count: u32,
        position: Point,
    },
    Hold {
        fingers: u16,
        position: Point,
    },
    Pan {
        fingers: u16,
        phase: Phase,
        direction: Direction,
        total: Point,
        delta: Point,
        velocity: Point,
        centroid: Point,
        swipe: bool,
    },
    Pinch {
        fingers: u16,
        phase: Phase,
        ratio: f64,
    },
    Rotate {
        fingers: u16,
        phase: Phase,
        degrees: f64,
    },
    /// Cancels every outstanding progress kind for this group ID.
    Cancelled {
        reason: CancelReason,
    },
}

#[derive(Clone, Debug)]
pub struct TouchSettings {
    pub timing: Timings,
    pub swipe_travel: Distance,
    pub fallback_travel: Distance,
    pub tap_location: Distance,
    pub fallback_location: Distance,
    pub pinch_slop: f64,
    pub rotation_slop: f64,
}
impl Default for TouchSettings {
    fn default() -> Self {
        Self {
            timing: Timings::default(),
            swipe_travel: Distance::Millimetres(12.0),
            fallback_travel: Distance::Normalized(0.04),
            tap_location: Distance::Millimetres(12.0),
            fallback_location: Distance::Normalized(0.04),
            pinch_slop: 0.05,
            rotation_slop: 5.0,
        }
    }
}
impl TouchSettings {
    fn validate(&self, mapping: &Mapping) -> Result<(), &'static str> {
        for duration in [
            self.timing.tap_max,
            self.timing.tap_gap,
            self.timing.hold_after,
            self.timing.join_window,
            self.timing.decision_window,
            self.timing.swipe_timeout,
        ] {
            if duration.0 == 0 || duration.0 > 3_600_000 {
                return Err("touch duration outside supported bounds");
            }
        }
        for distance in [
            self.timing.tap_motion,
            self.timing.fallback_motion,
            self.timing.pan_slop,
            self.swipe_travel,
            self.fallback_travel,
            self.tap_location,
            self.fallback_location,
        ] {
            let maximum = if matches!(distance, Distance::Normalized(_)) {
                2.0
            } else {
                1_000_000.0
            };
            if !distance.value().is_finite()
                || distance.value() <= 0.0
                || distance.value() > maximum
            {
                return Err("touch distance outside supported bounds");
            }
            if matches!(distance, Distance::Pixels(_)) && mapping.output.is_none() {
                return Err("pixel thresholds require an output mapping");
            }
        }
        if [
            self.timing.fallback_motion,
            self.fallback_travel,
            self.fallback_location,
        ]
        .iter()
        .any(|distance| !matches!(distance, Distance::Normalized(_)))
        {
            return Err("touch fallbacks must use explicit normalized distances");
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Group {
    id: u64,
    started: Duration,
    last_join: Duration,
    initial: BTreeMap<ContactId, Position>,
    current: BTreeMap<ContactId, Position>,
    active: usize,
    peak: usize,
    settled: bool,
    eligible: bool,
    cancelled: bool,
    moved: bool,
    held: bool,
    pan: Option<Direction>,
    pinching: bool,
    rotating: bool,
    last_total: Point,
    last_motion: Duration,
    ratio: f64,
    rotation: f64,
    last_angle: f64,
}
#[derive(Debug)]
struct TapSeries {
    id: u64,
    fingers: u16,
    count: u32,
    released: Duration,
    position: Position,
}

/// One recognizer per connected touchscreen. It does not own input or choose
/// app targets; its observations are proposals for the separate router.
pub struct TouchRecognizer {
    source: Source,
    mapping: Mapping,
    settings: TouchSettings,
    group: Option<Group>,
    taps: Option<TapSeries>,
    next_id: u64,
    last_at: Option<Duration>,
    blocked: bool,
}
impl TouchRecognizer {
    pub fn new(
        source: Source,
        mapping: Mapping,
        settings: TouchSettings,
    ) -> Result<Self, &'static str> {
        mapping.validate()?;
        settings.validate(&mapping)?;
        if settings.timing.join_window.0 == 0
            || !settings.pinch_slop.is_finite()
            || settings.pinch_slop <= 0.0
            || !settings.rotation_slop.is_finite()
            || settings.rotation_slop <= 0.0
        {
            return Err("invalid touch timing or motion thresholds");
        }
        Ok(Self {
            source,
            mapping,
            settings,
            group: None,
            taps: None,
            next_id: 0,
            last_at: None,
            blocked: false,
        })
    }
    pub fn deadline(&self) -> Option<Duration> {
        if let Some(group) = &self.group {
            if group.cancelled {
                return None;
            }
            let join = (!group.settled).then(|| {
                group
                    .started
                    .saturating_add(self.settings.timing.join_window.duration())
            });
            let hold = (!group.held && !group.moved && group.eligible).then(|| {
                group
                    .last_join
                    .saturating_add(self.settings.timing.hold_after.duration())
                    .max(
                        group
                            .started
                            .saturating_add(self.settings.timing.join_window.duration()),
                    )
            });
            join.into_iter().chain(hold).min()
        } else {
            self.taps.as_ref().map(|taps| {
                taps.released
                    .saturating_add(self.settings.timing.tap_gap.duration())
            })
        }
    }
    /// Rebase a native-owned held group without contact births or input replay.
    pub(crate) fn continue_contacts(
        &mut self,
        source: &Source,
        mapping: &Mapping,
        contacts: &[crate::input::Contact],
        at: Duration,
    ) -> Result<(), &'static str> {
        if *source != self.source
            || *mapping != self.mapping
            || contacts.is_empty()
            || contacts.len() > 64
            || self.last_at.is_some_and(|last| at < last)
        {
            return Err("invalid native touch continuation");
        }
        let points = contacts
            .iter()
            .map(|contact| {
                self.mapping
                    .map(contact.x, contact.y)
                    .map(|position| (contact.id, position))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        if points.len() != contacts.len() {
            return Err("duplicate native continuation contacts");
        }
        let id = self.next_id.checked_add(1).ok_or("touch IDs exhausted")?;
        self.group = Some(Group {
            id,
            started: at,
            last_join: at,
            initial: points.clone(),
            current: points,
            active: contacts.len(),
            peak: contacts.len(),
            settled: true,
            eligible: true,
            cancelled: false,
            moved: false,
            held: false,
            pan: None,
            pinching: false,
            rotating: false,
            last_total: Point::default(),
            last_motion: at,
            ratio: 1.0,
            rotation: 0.0,
            last_angle: 0.0,
        });
        self.taps = None;
        self.next_id = id;
        self.last_at = Some(at);
        self.blocked = false;
        Ok(())
    }
    pub fn replace_mapping(
        &mut self,
        mapping: Mapping,
        at: Duration,
    ) -> Result<Vec<TouchEvent>, &'static str> {
        mapping.validate()?;
        self.settings.validate(&mapping)?;
        if mapping == self.mapping {
            return Ok(Vec::new());
        }
        if mapping.generation <= self.mapping.generation {
            return Err("changed mapping must advance its generation");
        }
        let events = self.cancel(at, CancelReason::MappingChanged);
        self.mapping = mapping;
        Ok(events)
    }
    pub fn cancel(&mut self, at: Duration, reason: CancelReason) -> Vec<TouchEvent> {
        let group = self.group.take();
        self.blocked |= group.as_ref().is_some_and(|group| group.active > 0);
        let id = group
            .map(|group| group.id)
            .or_else(|| self.taps.as_ref().map(|taps| taps.id));
        self.taps = None;
        id.map(|id| vec![self.event(id, at, TouchGesture::Cancelled { reason })])
            .unwrap_or_default()
    }
    /// Reset from a kernel snapshot. Held contacts must reach neutral first.
    pub fn reconcile(&mut self, at: Duration, contacts_held: bool) -> Vec<TouchEvent> {
        let events = self.cancel(at, CancelReason::EventLoss);
        self.blocked = contacts_held;
        self.last_at = Some(at);
        events
    }
    pub fn advance(&mut self, at: Duration) -> Vec<TouchEvent> {
        if self.last_at.is_some_and(|last| at < last) {
            self.last_at = Some(at);
            return self.cancel(at, CancelReason::BackwardTime);
        }
        self.last_at = Some(at);
        let mut events = Vec::new();
        if let Some(mut group) = self.group.take() {
            let motion = self.update_motion(&mut group, at, false);
            if group.moved {
                events.extend(self.finish_taps(at));
            }
            events.extend(motion);
            if group.settled
                && group.eligible
                && !group.cancelled
                && !group.moved
                && !group.held
                && at.saturating_sub(group.last_join) >= self.settings.timing.hold_after.duration()
            {
                group.held = true;
                events.extend(self.finish_taps(at));
                events.push(self.event(
                    group.id,
                    at,
                    TouchGesture::Hold {
                        fingers: group.peak as u16,
                        position: centre(group.current.values().map(|point| point.point())),
                    },
                ));
            }
            self.group = Some(group);
        } else if self.taps.as_ref().is_some_and(|taps| {
            at.saturating_sub(taps.released) >= self.settings.timing.tap_gap.duration()
        }) {
            events.extend(self.finish_taps(at));
        }
        events
    }
    pub fn process(&mut self, frame: &NormalizedFrame) -> Vec<TouchEvent> {
        let at = frame.raw.at;
        if frame.raw.source != self.source {
            return self.cancel(at, CancelReason::SourceChanged);
        }
        if self.last_at.is_some_and(|last| at < last) {
            self.last_at = Some(at);
            return self.cancel(at, CancelReason::BackwardTime);
        }
        let mut events = self.advance(at);
        if self.blocked {
            if frame.contacts.is_empty() {
                self.blocked = false;
            }
            return events;
        }
        let map = |contact: &crate::input::Contact| {
            self.mapping
                .map(contact.x, contact.y)
                .map(|position| (contact.id, position))
        };
        let points: Vec<_> = match frame
            .contacts
            .iter()
            .chain(&frame.ended_contacts)
            .map(map)
            .collect::<Result<_, _>>()
        {
            Ok(points) => points,
            Err(_) => {
                events.extend(self.cancel(at, CancelReason::InvalidFrame));
                return events;
            }
        };
        let mut group = if let Some(group) = self.group.take() {
            group
        } else {
            if frame.contacts.is_empty() {
                return events;
            }
            let Some(id) = self.next_id.checked_add(1) else {
                return self.cancel(at, CancelReason::InvalidFrame);
            };
            self.next_id = id;
            Group {
                id,
                started: at,
                last_join: at,
                initial: BTreeMap::new(),
                current: BTreeMap::new(),
                active: 0,
                peak: 0,
                settled: false,
                eligible: true,
                cancelled: false,
                moved: false,
                held: false,
                pan: None,
                pinching: false,
                rotating: false,
                last_total: Point::default(),
                last_motion: at,
                ratio: 1.0,
                rotation: 0.0,
                last_angle: 0.0,
            }
        };
        let was_settled = group.settled;
        for contact in &frame.contacts {
            let position = points
                .iter()
                .find(|(id, _)| *id == contact.id)
                .expect("mapped contact")
                .1;
            if !group.initial.contains_key(&contact.id) {
                if was_settled {
                    group.eligible = false;
                }
                group.initial.insert(contact.id, position);
                group.last_join = at;
                if !contact.fresh {
                    group.eligible = false;
                }
            }
        }
        for (id, position) in points {
            if group.initial.contains_key(&id) {
                group.current.insert(id, position);
            }
        }
        group.active = frame.contacts.len();
        group.peak = group.peak.max(group.active);
        if group.active > 64 || group.initial.len() > 64 {
            group.eligible = false;
        }
        if group.active == 0 {
            group.settled = true;
        }
        if group.settled && group.initial.len() != group.peak {
            group.eligible = false;
        }
        group.moved |= group.initial.iter().any(|(id, start)| {
            group.current.get(id).is_some_and(|end| {
                exceeds(
                    *start,
                    *end,
                    self.settings.timing.tap_motion,
                    self.settings.timing.fallback_motion,
                )
            })
        });
        if group.moved {
            events.extend(self.finish_taps(at));
        }
        if !group.eligible && !group.cancelled {
            group.cancelled = true;
            self.taps = None;
            events.push(self.event(
                group.id,
                at,
                TouchGesture::Cancelled {
                    reason: CancelReason::InvalidFrame,
                },
            ));
        }
        if group.active > 0
            && group.active < group.peak
            && (group.pan.is_some() || group.pinching || group.rotating)
            && !group.cancelled
        {
            group.cancelled = true;
            events.push(self.event(
                group.id,
                at,
                TouchGesture::Cancelled {
                    reason: CancelReason::InvalidFrame,
                },
            ));
        }
        let motion = self.update_motion(&mut group, at, true);
        if group.moved {
            events.extend(self.finish_taps(at));
        }
        events.extend(motion);
        if group.active == 0 {
            if !group.cancelled && group.eligible {
                let fingers = group.peak as u16;
                let total = displacement(&group);
                if let Some(direction) = group.pan {
                    let swipe = at.saturating_sub(group.started)
                        <= self.settings.timing.swipe_timeout.duration()
                        && direction_for(total) == Some(direction)
                        && group.initial.iter().all(|(id, start)| {
                            group.current.get(id).is_some_and(|end| {
                                reached(
                                    *start,
                                    *end,
                                    self.settings.swipe_travel,
                                    self.settings.fallback_travel,
                                )
                            })
                        });
                    events.push(self.event(
                        group.id,
                        at,
                        TouchGesture::Pan {
                            fingers,
                            phase: Phase::End,
                            direction,
                            total,
                            delta: Point::default(),
                            velocity: Point::default(),
                            centroid: centre(group.current.values().map(|point| point.point())),
                            swipe,
                        },
                    ));
                }
                if group.pinching {
                    events.push(self.event(
                        group.id,
                        at,
                        TouchGesture::Pinch {
                            fingers,
                            phase: Phase::End,
                            ratio: group.ratio,
                        },
                    ));
                }
                if group.rotating {
                    events.push(self.event(
                        group.id,
                        at,
                        TouchGesture::Rotate {
                            fingers,
                            phase: Phase::End,
                            degrees: group.rotation,
                        },
                    ));
                }
                if !group.moved
                    && !group.held
                    && at.saturating_sub(group.started) <= self.settings.timing.tap_max.duration()
                {
                    let position = mean_position(group.current.values().copied());
                    let compatible = self.taps.as_ref().is_some_and(|taps| {
                        taps.fingers == fingers
                            && !exceeds(
                                taps.position,
                                position,
                                self.settings.tap_location,
                                self.settings.fallback_location,
                            )
                    });
                    if !compatible {
                        events.extend(self.finish_taps(at));
                    }
                    let taps = self.taps.get_or_insert(TapSeries {
                        id: group.id,
                        fingers,
                        count: 0,
                        released: at,
                        position,
                    });
                    taps.count = taps.count.saturating_add(1);
                    taps.released = at;
                    taps.position = position;
                } else {
                    events.extend(self.finish_taps(at));
                }
            }
        } else {
            self.group = Some(group);
        }
        events
    }
    fn update_motion(&self, group: &mut Group, at: Duration, changed: bool) -> Vec<TouchEvent> {
        if at.saturating_sub(group.started) >= self.settings.timing.join_window.duration() {
            group.settled = true;
        }
        if !group.settled || !group.eligible || group.cancelled || group.initial.is_empty() {
            return Vec::new();
        }
        let fingers = group.peak as u16;
        let mut events = Vec::new();
        let total = displacement(group);
        let coherent = coherent_direction(
            group,
            self.settings.timing.pan_slop,
            self.settings.timing.fallback_motion,
        );
        if group.pan.is_none()
            && let Some(direction) = coherent
        {
            group.pan = Some(direction);
            self.pan_event(group, at, Phase::Begin, total, &mut events);
        } else if group.pan.is_some() && changed && total != group.last_total {
            self.pan_event(group, at, Phase::Update, total, &mut events);
        }
        if fingers >= 2 {
            let (ratio, angle) = deformation(group);
            group.ratio = ratio;
            let step = (angle - group.last_angle + 180.0).rem_euclid(360.0) - 180.0;
            group.rotation += step;
            group.last_angle = angle;
            if !group.pinching && (ratio - 1.0).abs() >= self.settings.pinch_slop {
                group.pinching = true;
                events.push(self.event(
                    group.id,
                    at,
                    TouchGesture::Pinch {
                        fingers,
                        phase: Phase::Begin,
                        ratio,
                    },
                ));
            } else if group.pinching && changed {
                events.push(self.event(
                    group.id,
                    at,
                    TouchGesture::Pinch {
                        fingers,
                        phase: Phase::Update,
                        ratio,
                    },
                ));
            }
            if !group.rotating && group.rotation.abs() >= self.settings.rotation_slop {
                group.rotating = true;
                events.push(self.event(
                    group.id,
                    at,
                    TouchGesture::Rotate {
                        fingers,
                        phase: Phase::Begin,
                        degrees: group.rotation,
                    },
                ));
            } else if group.rotating && changed {
                events.push(self.event(
                    group.id,
                    at,
                    TouchGesture::Rotate {
                        fingers,
                        phase: Phase::Update,
                        degrees: group.rotation,
                    },
                ));
            }
        }
        group.moved |= group.pan.is_some() || group.pinching || group.rotating;
        events
    }
    fn pan_event(
        &self,
        group: &mut Group,
        at: Duration,
        phase: Phase,
        total: Point,
        events: &mut Vec<TouchEvent>,
    ) {
        let delta = total.minus(group.last_total);
        let elapsed = at.saturating_sub(group.last_motion).as_secs_f64();
        let velocity = if elapsed > 0.0 {
            delta.scale(1.0 / elapsed)
        } else {
            Point::default()
        };
        events.push(self.event(
            group.id,
            at,
            TouchGesture::Pan {
                fingers: group.peak as u16,
                phase,
                direction: group.pan.expect("pan started"),
                total,
                delta,
                velocity,
                centroid: centre(group.current.values().map(|point| point.point())),
                swipe: false,
            },
        ));
        group.last_total = total;
        group.last_motion = at;
    }
    fn event(&self, id: u64, at: Duration, gesture: TouchGesture) -> TouchEvent {
        TouchEvent {
            source: self.source.clone(),
            id,
            at,
            space: if self.mapping.output.is_some() {
                Space::Logical
            } else {
                Space::Normalized
            },
            gesture,
        }
    }
    fn finish_taps(&mut self, at: Duration) -> Vec<TouchEvent> {
        self.taps
            .take()
            .map(|taps| {
                vec![self.event(
                    taps.id,
                    at,
                    TouchGesture::Taps {
                        fingers: taps.fingers,
                        count: taps.count,
                        position: taps.position.point(),
                    },
                )]
            })
            .unwrap_or_default()
    }
}

fn centre(points: impl Iterator<Item = Point>) -> Point {
    let (sum, count) = points.fold((Point::default(), 0usize), |(sum, count), point| {
        (
            Point {
                x: sum.x + point.x,
                y: sum.y + point.y,
            },
            count + 1,
        )
    });
    if count == 0 {
        Point::default()
    } else {
        sum.scale(1.0 / count as f64)
    }
}
fn mean_position(points: impl Iterator<Item = Position>) -> Position {
    let points: Vec<_> = points.collect();
    Position {
        normalized: centre(points.iter().map(|p| p.normalized)),
        millimetres: points
            .iter()
            .all(|p| p.millimetres.is_some())
            .then(|| centre(points.iter().filter_map(|p| p.millimetres))),
        local: points
            .iter()
            .all(|p| p.local.is_some())
            .then(|| centre(points.iter().filter_map(|p| p.local))),
        global: points
            .iter()
            .all(|p| p.global.is_some())
            .then(|| centre(points.iter().filter_map(|p| p.global))),
    }
}
fn measurement(a: Position, b: Position, distance: Distance, fallback: Distance) -> (f64, f64) {
    match distance {
        Distance::Millimetres(value) => {
            if let (Some(a), Some(b)) = (a.millimetres, b.millimetres) {
                (b.minus(a).length(), value)
            } else {
                // Settings validation requires an explicit normalized fallback.
                (b.normalized.minus(a.normalized).length(), fallback.value())
            }
        }
        Distance::Pixels(value) => {
            if let (Some(a), Some(b)) = (a.local, b.local) {
                (b.minus(a).length(), value)
            } else {
                (0.0, value)
            }
        }
        Distance::Normalized(value) => (b.normalized.minus(a.normalized).length(), value),
    }
}
fn exceeds(a: Position, b: Position, distance: Distance, fallback: Distance) -> bool {
    let (measured, threshold) = measurement(a, b, distance, fallback);
    measured > threshold
}
fn reached(a: Position, b: Position, distance: Distance, fallback: Distance) -> bool {
    let (measured, threshold) = measurement(a, b, distance, fallback);
    measured >= threshold
}
fn displacement(group: &Group) -> Point {
    centre(group.current.values().map(|p| p.point()))
        .minus(centre(group.initial.values().map(|p| p.point())))
}
fn direction_for(total: Point) -> Option<Direction> {
    if total.x.abs() > 1.2 * total.y.abs() {
        Some(if total.x < 0.0 {
            Direction::Left
        } else {
            Direction::Right
        })
    } else if total.y.abs() > 1.2 * total.x.abs() {
        Some(if total.y < 0.0 {
            Direction::Up
        } else {
            Direction::Down
        })
    } else {
        None
    }
}
fn coherent_direction(group: &Group, slop: Distance, fallback: Distance) -> Option<Direction> {
    let direction = direction_for(displacement(group))?;
    group
        .initial
        .iter()
        .all(|(id, start)| {
            group.current.get(id).is_some_and(|end| {
                exceeds(*start, *end, slop, fallback)
                    && direction_for(end.point().minus(start.point())) == Some(direction)
            })
        })
        .then_some(direction)
}
fn deformation(group: &Group) -> (f64, f64) {
    let initial = centre(group.initial.values().map(|p| p.point()));
    let current = centre(group.current.values().map(|p| p.point()));
    let (mut old_radius, mut new_radius, mut dot, mut cross) = (0.0, 0.0, 0.0, 0.0);
    for (id, start) in &group.initial {
        let Some(end) = group.current.get(id) else {
            continue;
        };
        let a = start.point().minus(initial);
        let b = end.point().minus(current);
        old_radius += a.x * a.x + a.y * a.y;
        new_radius += b.x * b.x + b.y * b.y;
        dot += a.x * b.x + a.y * b.y;
        cross += a.x * b.y - a.y * b.x;
    }
    if old_radius < 1e-12 {
        (1.0, 0.0)
    } else {
        (
            (new_radius / old_radius).sqrt(),
            cross.atan2(dot).to_degrees(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Millis, input::*};

    fn axis(maximum: i32, resolution: i32) -> Axis {
        Axis {
            minimum: 0,
            maximum,
            resolution,
            fuzz: 0,
            flat: 0,
        }
    }
    fn device() -> Device {
        Device {
            identity: "test-touch".into(),
            source: Source {
                device: 1,
                generation: 1,
                seat: "seat0".into(),
            },
            name: "Test touch".into(),
            path: "/dev/input/test".into(),
            physical: "test".into(),
            unique: "test-touch".into(),
            vendor: 1,
            product: 1,
            properties: vec![1],
            virtual_device: false,
            capabilities: BTreeMap::from([(
                EV_ABS,
                vec![
                    ABS_MT_SLOT,
                    ABS_MT_TRACKING_ID,
                    ABS_MT_POSITION_X,
                    ABS_MT_POSITION_Y,
                ],
            )]),
            axes: BTreeMap::from([
                (ABS_MT_SLOT, axis(9, 0)),
                (ABS_MT_POSITION_X, axis(1000, 10)),
                (ABS_MT_POSITION_Y, axis(2000, 10)),
            ]),
        }
    }
    fn mapping() -> Mapping {
        Mapping::from_device(&device()).unwrap()
    }
    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }
    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }
    struct Trial {
        decoder: Normalizer,
        recognizer: TouchRecognizer,
        down: BTreeMap<u16, (i32, i32, i32)>,
        sequence: u64,
    }
    impl Trial {
        fn new(settings: TouchSettings) -> Self {
            let device = device();
            Self {
                decoder: Normalizer::new(&device),
                recognizer: TouchRecognizer::new(device.source, mapping(), settings).unwrap(),
                down: BTreeMap::new(),
                sequence: 0,
            }
        }
        fn send(&mut self, at: u64, contacts: &[(u16, i32, i32, i32)]) -> Vec<TouchEvent> {
            let mut events = Vec::new();
            let mut add = |code, value| {
                events.push(RawEvent {
                    kind: EV_ABS,
                    code,
                    value,
                })
            };
            for slot in self
                .down
                .keys()
                .filter(|slot| !contacts.iter().any(|c| c.0 == **slot))
            {
                add(ABS_MT_SLOT, i32::from(*slot));
                add(ABS_MT_TRACKING_ID, -1);
            }
            for &(slot, tracking, x, y) in contacts {
                add(ABS_MT_SLOT, i32::from(slot));
                if self.down.get(&slot).is_none_or(|c| c.0 != tracking) {
                    add(ABS_MT_TRACKING_ID, tracking);
                }
                add(ABS_MT_POSITION_X, x);
                add(ABS_MT_POSITION_Y, y);
            }
            events.push(RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            });
            self.sequence += 1;
            let frame = self
                .decoder
                .process(Frame {
                    source: device().source,
                    at: ms(at),
                    sequence: self.sequence,
                    events,
                })
                .unwrap();
            self.down = contacts
                .iter()
                .map(|&(slot, id, x, y)| (slot, (id, x, y)))
                .collect();
            self.recognizer.process(&frame)
        }
        fn tick(&mut self, at: u64) -> Vec<TouchEvent> {
            self.recognizer.advance(ms(at))
        }
    }
    fn has(events: &[TouchEvent], predicate: impl Fn(&TouchGesture) -> bool) -> bool {
        events.iter().any(|event| predicate(&event.gesture))
    }
    fn two(x: i32, y: i32) -> [(u16, i32, i32, i32); 2] {
        [(0, 10, x, y), (1, 11, x + 200, y)]
    }

    #[test]
    fn native_owned_held_contacts_rebase_atomically_without_births_or_old_motion() {
        let mut trial = Trial::new(TouchSettings::default());
        trial.send(0, &two(100, 200));
        let old = trial.recognizer.group.as_ref().unwrap().id;
        let contacts: Vec<_> = trial
            .recognizer
            .group
            .as_ref()
            .unwrap()
            .initial
            .keys()
            .map(|id| crate::input::Contact {
                id: *id,
                x: 100 + i32::from(id.slot) * 200,
                y: 200,
                pressure: None,
                fresh: false,
            })
            .collect();
        let source = device().source;
        for bad in 0..5 {
            let mut source = source.clone();
            let mut mapping = mapping();
            let mut contacts = contacts.clone();
            match bad {
                0 => source.generation += 1,
                1 => mapping.generation += 1,
                2 => contacts.clear(),
                3 => contacts[1] = contacts[0].clone(),
                _ => contacts[0].x = -1,
            }
            assert!(
                trial
                    .recognizer
                    .continue_contacts(&source, &mapping, &contacts, ms(20))
                    .is_err()
            );
            assert_eq!(trial.recognizer.group.as_ref().unwrap().id, old);
        }
        trial.tick(10);
        assert!(
            trial
                .recognizer
                .continue_contacts(&source, &mapping(), &contacts, ms(9))
                .is_err()
        );
        trial
            .recognizer
            .continue_contacts(&source, &mapping(), &contacts, ms(20))
            .unwrap();
        assert!(trial.recognizer.group.as_ref().unwrap().id > old);
        assert!(trial.tick(21).is_empty());
        assert!(has(&trial.send(30, &two(200, 200)), |gesture| matches!(
            gesture,
            TouchGesture::Pan {
                phase: Phase::Begin,
                ..
            }
        )));
        let mut extra = two(200, 200).to_vec();
        extra.push((2, 12, 600, 200));
        assert!(has(&trial.send(40, &extra), |gesture| matches!(
            gesture,
            TouchGesture::Cancelled { .. }
        )));
    }

    #[test]
    fn t18_transforms_scales_and_origin_apply_exactly_once() {
        let table = [
            (Transform::Normal, (0.2, 0.3)),
            (Transform::Rotate90, (0.7, 0.2)),
            (Transform::Rotate180, (0.8, 0.7)),
            (Transform::Rotate270, (0.3, 0.8)),
            (Transform::Flipped, (0.8, 0.3)),
            (Transform::Flipped90, (0.7, 0.8)),
            (Transform::Flipped180, (0.2, 0.7)),
            (Transform::Flipped270, (0.3, 0.2)),
        ];
        for (transform, (x, y)) in table {
            for scale in [1.0, 1.5, 1.8, 2.0] {
                let mut mapping = mapping();
                mapping.output = Some(Output {
                    name: "phone".into(),
                    pixel_width: 1000.0,
                    pixel_height: 2000.0,
                    scale,
                    origin: Point {
                        x: -800.0,
                        y: 100.0,
                    },
                    transform,
                });
                mapping.validate().unwrap();
                let position = mapping.map(200, 600).unwrap();
                close(position.normalized.x, x);
                close(position.normalized.y, y);
                let (width, height) = if transform.swapped() {
                    (2000.0, 1000.0)
                } else {
                    (1000.0, 2000.0)
                };
                close(position.local.unwrap().x, x * width / scale);
                close(position.local.unwrap().y, y * height / scale);
                close(position.global.unwrap().x, x * width / scale - 800.0);
                close(position.global.unwrap().y, y * height / scale + 100.0);
                assert_eq!(position.millimetres, Some(Point { x: 20.0, y: 60.0 }));
            }
        }
    }
    #[test]
    fn t18_hardware_axis_endpoints_remain_inside_half_open_output_geometry() {
        for transform in [
            Transform::Normal,
            Transform::Rotate90,
            Transform::Rotate180,
            Transform::Rotate270,
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            for origin in [
                Point {
                    x: -800.0,
                    y: 100.0,
                },
                Point {
                    x: 1024.0,
                    y: 2048.0,
                },
            ] {
                let mut mapped = mapping();
                mapped.output = Some(Output {
                    name: "test".into(),
                    pixel_width: 1000.0,
                    pixel_height: 2000.0,
                    scale: 1.8,
                    origin,
                    transform,
                });
                let size = mapped.output.as_ref().unwrap().logical_size();
                let rect = crate::context::Rect { origin, size };
                for x in [mapped.x.minimum, mapped.x.maximum] {
                    for y in [mapped.y.minimum, mapped.y.maximum] {
                        let position = mapped.map(x, y).unwrap();
                        let local = position.local.unwrap();
                        assert!(local.x >= 0.0 && local.x < size.x);
                        assert!(local.y >= 0.0 && local.y < size.y);
                        assert!(rect.contains(position.global.unwrap()));
                    }
                }
            }
        }
        let mut mapped = mapping();
        mapped.output = Some(Output {
            name: "test".into(),
            pixel_width: 1000.0,
            pixel_height: 2000.0,
            scale: 1.0,
            origin: Point { x: 0.0, y: 0.0 },
            transform: Transform::Normal,
        });
        mapped.calibration[0][2] = 0.1;
        let position = mapped.map(mapped.x.maximum, mapped.y.maximum).unwrap();
        assert!(position.local.unwrap().x > 1000.0);
    }

    #[test]
    fn t18_calibration_and_invalid_geometry() {
        let mut mapped = mapping();
        mapped.calibration = [[0.5, 0.0, 0.1], [0.0, 0.5, 0.2]];
        let p = mapped.map(200, 600).unwrap();
        close(p.normalized.x, 0.2);
        close(p.normalized.y, 0.35);
        assert!(mapped.map(-1, 0).is_err());
        mapped.calibration[0][0] = f64::MAX;
        assert!(mapped.validate().is_err());
        mapped = mapping();
        mapped.output = Some(Output {
            name: "bad".into(),
            pixel_width: f64::MAX,
            pixel_height: 1.0,
            scale: f64::MIN_POSITIVE,
            origin: Point::default(),
            transform: Transform::Normal,
        });
        assert!(mapped.validate().is_err());
    }
    #[test]
    fn t19_identical_refresh_preserves_but_changed_mapping_cancels() {
        let mut trial = Trial::new(TouchSettings::default());
        trial.send(0, &two(200, 200));
        assert!(
            trial
                .recognizer
                .replace_mapping(mapping(), ms(50))
                .unwrap()
                .is_empty()
        );
        let mut next = mapping();
        next.generation = 2;
        next.calibration[0][2] = 0.1;
        assert!(has(
            &trial
                .recognizer
                .replace_mapping(next.clone(), ms(60))
                .unwrap(),
            |g| matches!(
                g,
                TouchGesture::Cancelled {
                    reason: CancelReason::MappingChanged
                }
            )
        ));
        assert!(trial.send(70, &two(250, 200)).is_empty());
        assert!(trial.send(80, &[]).is_empty());
        assert!(trial.send(90, &two(200, 200)).is_empty());
        trial.send(110, &[]);
        assert!(has(&trial.tick(410), |g| matches!(
            g,
            TouchGesture::Taps {
                fingers: 2,
                count: 1,
                ..
            }
        )));
        next.generation = 1;
        assert!(trial.recognizer.replace_mapping(next, ms(500)).is_err());
    }
    #[test]
    fn t19_idle_mapping_change_does_not_drop_next_touch() {
        let mut trial = Trial::new(TouchSettings::default());
        let mut next = mapping();
        next.generation = 2;
        trial.recognizer.replace_mapping(next, ms(0)).unwrap();
        trial.send(10, &two(200, 200));
        trial.send(30, &[]);
        assert!(has(&trial.tick(330), |g| matches!(
            g,
            TouchGesture::Taps { count: 1, .. }
        )));
    }
    #[test]
    fn t27_multifinger_double_tap_is_one_settled_result() {
        for fingers in [2, 3] {
            let mut trial = Trial::new(TouchSettings::default());
            let contacts: Vec<_> = (0..fingers)
                .map(|slot| (slot, i32::from(slot) + 1, 200 + i32::from(slot) * 100, 400))
                .collect();
            assert!(trial.send(0, &contacts).is_empty());
            assert!(trial.send(50, &[]).is_empty());
            assert!(trial.tick(100).is_empty());
            assert!(trial.send(150, &contacts).is_empty());
            assert!(trial.send(200, &[]).is_empty());
            let events = trial.tick(500);
            assert_eq!(events.len(), 1);
            assert!(
                matches!(events[0].gesture, TouchGesture::Taps { fingers: f, count: 2, .. } if f == fingers)
            );
            assert!(trial.tick(1000).is_empty());
        }
    }
    #[test]
    fn t28_join_deadline_is_fixed_and_late_finger_cancels() {
        for join in [119, 120, 121] {
            let mut trial = Trial::new(TouchSettings::default());
            trial.send(0, &[(0, 10, 200, 200)]);
            let events = trial.send(join, &two(200, 200));
            assert_eq!(
                has(&events, |g| matches!(g, TouchGesture::Cancelled { .. })),
                join >= 120
            );
            trial.send(150, &[]);
            let events = trial.tick(450);
            assert_eq!(
                has(&events, |g| matches!(
                    g,
                    TouchGesture::Taps { fingers: 2, .. }
                )),
                join < 120
            );
        }
    }
    #[test]
    fn t28_serial_contacts_do_not_become_two_finger_tap() {
        let mut trial = Trial::new(TouchSettings::default());
        trial.send(0, &[(0, 1, 200, 200)]);
        let events = trial.send(50, &[(1, 2, 400, 200)]);
        assert!(events.is_empty());
        assert!(has(&trial.send(100, &[]), |g| matches!(
            g,
            TouchGesture::Cancelled { .. }
        )));
        assert!(trial.tick(400).is_empty());
    }
    #[test]
    fn t29_jitter_is_tap_but_motion_is_not() {
        for movement in [30, 31] {
            let mut trial = Trial::new(TouchSettings::default());
            trial.send(0, &two(200, 200));
            trial.send(50, &two(200 + movement, 200));
            trial.send(100, &[]);
            assert_eq!(
                has(&trial.tick(400), |g| matches!(g, TouchGesture::Taps { .. })),
                movement <= 30
            );
        }
    }
    #[test]
    fn t23_hold_uses_last_join_and_runs_without_more_input() {
        let mut settings = TouchSettings::default();
        settings.timing.hold_after = Millis(2000);
        let mut trial = Trial::new(settings);
        trial.send(0, &[(0, 10, 200, 200)]);
        trial.send(100, &two(200, 200));
        assert!(trial.tick(2099).is_empty());
        let events = trial.tick(2100);
        assert!(has(&events, |g| matches!(
            g,
            TouchGesture::Hold { fingers: 2, .. }
        )));
        assert!(trial.tick(2200).is_empty());
        assert!(trial.send(2300, &[]).is_empty());
        assert!(trial.tick(2600).is_empty());
    }
    #[test]
    fn t25_tap_then_hold_keeps_both_observations_in_order() {
        let mut trial = Trial::new(TouchSettings::default());
        trial.send(0, &two(200, 200));
        trial.send(50, &[]);
        trial.send(150, &two(200, 200));
        let events = trial.tick(750);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].gesture,
            TouchGesture::Taps { count: 1, .. }
        ));
        assert!(matches!(
            events[1].gesture,
            TouchGesture::Hold { fingers: 2, .. }
        ));
    }
    #[test]
    fn t30_coherent_swipe_starts_after_join_and_includes_exact_threshold() {
        let mut trial = Trial::new(TouchSettings::default());
        trial.send(0, &two(200, 200));
        assert!(trial.send(50, &two(231, 200)).is_empty());
        assert!(has(&trial.tick(120), |g| matches!(
            g,
            TouchGesture::Pan {
                phase: Phase::Begin,
                direction: Direction::Right,
                ..
            }
        )));
        assert!(has(&trial.send(200, &two(320, 200)), |g| matches!(
            g,
            TouchGesture::Pan {
                phase: Phase::Update,
                ..
            }
        )));
        assert!(has(&trial.send(250, &[]), |g| matches!(
            g,
            TouchGesture::Pan {
                phase: Phase::End,
                swipe: true,
                ..
            }
        )));
    }
    #[test]
    fn t30_fast_swipe_can_complete_before_join_deadline() {
        let mut trial = Trial::new(TouchSettings::default());
        trial.send(0, &two(200, 200));
        trial.send(50, &two(400, 200));
        let events = trial.send(100, &[]);
        assert!(has(&events, |g| matches!(
            g,
            TouchGesture::Pan {
                phase: Phase::Begin,
                ..
            }
        )));
        assert!(has(&events, |g| matches!(
            g,
            TouchGesture::Pan {
                phase: Phase::End,
                swipe: true,
                ..
            }
        )));
    }
    #[test]
    fn t30_pinch_and_rotation_do_not_become_directional_swipes() {
        let mut trial = Trial::new(TouchSettings::default());
        trial.send(0, &two(200, 500));
        let events = trial.send(150, &[(0, 10, 100, 500), (1, 11, 500, 500)]);
        assert!(has(
            &events,
            |g| matches!(g, TouchGesture::Pinch { phase: Phase::Begin, ratio, .. } if *ratio > 1.9)
        ));
        assert!(!has(&events, |g| matches!(g, TouchGesture::Pan { .. })));
        let mut trial = Trial::new(TouchSettings::default());
        trial.send(0, &two(200, 500));
        let events = trial.send(150, &[(0, 10, 300, 300), (1, 11, 300, 700)]);
        assert!(has(&events, |g| matches!(
            g,
            TouchGesture::Rotate {
                phase: Phase::Begin,
                ..
            }
        )));
        assert!(!has(&events, |g| matches!(g, TouchGesture::Pan { .. })));
    }
    #[test]
    fn t31_extra_or_partial_finger_after_pan_cancels_once() {
        for extra in [false, true] {
            let mut trial = Trial::new(TouchSettings::default());
            trial.send(0, &two(200, 200));
            trial.send(150, &two(350, 200));
            let contacts = if extra {
                vec![(0, 10, 350, 200), (1, 11, 550, 200), (2, 12, 600, 200)]
            } else {
                vec![(0, 10, 350, 200)]
            };
            assert!(has(&trial.send(160, &contacts), |g| matches!(
                g,
                TouchGesture::Cancelled { .. }
            )));
            assert!(trial.send(170, &contacts).is_empty());
            assert!(trial.send(180, &[]).is_empty());
            assert!(trial.tick(500).is_empty());
        }
    }
    #[test]
    fn t32_partial_drag_and_return_ends_without_swipe() {
        let mut trial = Trial::new(TouchSettings::default());
        trial.send(0, &two(200, 200));
        trial.send(150, &two(350, 200));
        assert!(has(
            &trial.send(200, &two(200, 200)),
            |g| matches!(g, TouchGesture::Pan { phase: Phase::Update, total, .. } if total.length() == 0.0)
        ));
        assert!(has(&trial.send(220, &[]), |g| matches!(
            g,
            TouchGesture::Pan {
                phase: Phase::End,
                swipe: false,
                ..
            }
        )));
        assert!(trial.tick(520).is_empty());
    }
    #[test]
    fn t29_reconciled_held_contacts_never_tap_or_hold() {
        let device = device();
        let mut decoder = Normalizer::new(&device);
        decoder
            .reconcile(
                Default::default(),
                vec![Contact {
                    id: ContactId {
                        slot: 0,
                        tracking: 5,
                        generation: 1,
                    },
                    x: 200,
                    y: 200,
                    pressure: None,
                    fresh: false,
                }],
            )
            .unwrap();
        let mut recognizer =
            TouchRecognizer::new(device.source.clone(), mapping(), TouchSettings::default())
                .unwrap();
        let frame = decoder
            .process(Frame {
                source: device.source.clone(),
                at: ms(0),
                sequence: 1,
                events: vec![RawEvent {
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                }],
            })
            .unwrap();
        assert!(has(&recognizer.process(&frame), |g| matches!(
            g,
            TouchGesture::Cancelled { .. }
        )));
        assert!(recognizer.advance(ms(1000)).is_empty());
    }
    #[test]
    fn t18_missing_metadata_has_explicit_fallback_and_pixels_require_output() {
        let mut map = mapping();
        map.x.resolution = 0;
        map.y.resolution = 0;
        let settings = TouchSettings::default();
        let a = map.map(0, 0).unwrap();
        let b = map.map(40, 0).unwrap();
        assert!(reached(
            a,
            b,
            settings.swipe_travel,
            settings.fallback_travel
        ));
        let mut settings = settings;
        settings.swipe_travel = Distance::Pixels(12.0);
        assert!(TouchRecognizer::new(device().source, map, settings).is_err());
    }
    #[test]
    fn t15_source_generation_change_cancels_without_merging() {
        let mut trial = Trial::new(TouchSettings::default());
        trial.send(0, &two(200, 200));
        let mut frame = NormalizedFrame {
            raw: Frame {
                source: device().source,
                at: ms(50),
                sequence: 2,
                events: vec![],
            },
            changes: vec![],
            contacts: vec![],
            contacts_started: vec![],
            contacts_ended: vec![],
            ended_contacts: vec![],
        };
        frame.raw.source.generation = 2;
        assert!(has(&trial.recognizer.process(&frame), |g| matches!(
            g,
            TouchGesture::Cancelled {
                reason: CancelReason::SourceChanged
            }
        )));
        assert!(trial.tick(1000).is_empty());
    }
    #[test]
    fn t31_recognized_pan_or_pinch_cannot_later_be_a_hold_or_tap() {
        let mut settings = TouchSettings::default();
        settings.timing.pan_slop = Distance::Millimetres(1.0);
        let mut trial = Trial::new(settings);
        trial.send(0, &two(200, 200));
        assert!(has(&trial.send(150, &two(220, 200)), |g| matches!(
            g,
            TouchGesture::Pan {
                phase: Phase::Begin,
                ..
            }
        )));
        assert!(!has(&trial.tick(600), |g| matches!(
            g,
            TouchGesture::Hold { .. }
        )));
        trial.send(650, &[]);
        assert!(!has(&trial.tick(950), |g| matches!(
            g,
            TouchGesture::Taps { .. }
        )));
        let mut trial = Trial::new(TouchSettings::default());
        trial.send(0, &two(200, 200));
        assert!(has(
            &trial.send(150, &[(0, 10, 194, 200), (1, 11, 406, 200)]),
            |g| matches!(
                g,
                TouchGesture::Pinch {
                    phase: Phase::Begin,
                    ..
                }
            )
        ));
        assert!(!has(&trial.tick(600), |g| matches!(
            g,
            TouchGesture::Hold { .. }
        )));
        trial.send(650, &[]);
        assert!(!has(&trial.tick(950), |g| matches!(
            g,
            TouchGesture::Taps { .. }
        )));
    }
}
