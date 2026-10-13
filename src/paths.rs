//! Armed, bounded trajectory capture and ordered template matching.
//! These primitives observe input; they never claim it or execute an action.

use crate::{
    config::{Distance, ShapeTemplate},
    gestures::{Point, Position, Space},
    input::{ContactId, Source},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, time::Duration};

pub const RESAMPLE_POINTS: usize = 64;
pub const MAX_TEMPLATES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureIdentity {
    pub source: Source,
    pub mapping_generation: u64,
    /// Empty for an armed mouse trajectory; touch uses the frozen contact IDs.
    pub contacts: BTreeSet<ContactId>,
}
#[derive(Clone, Debug)]
pub struct CaptureLimits {
    pub max_samples: usize,
    pub timeout: Duration,
    pub minimum_length: Distance,
    pub minimum_size: Distance,
    pub fallback_length: Distance,
    pub fallback_size: Distance,
    /// Activation is sampled at begin, or required throughout capture.
    pub armed_throughout: bool,
}
impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_samples: 2048,
            timeout: Duration::from_secs(5),
            minimum_length: Distance::Millimetres(20.0),
            minimum_size: Distance::Millimetres(12.0),
            fallback_length: Distance::Normalized(0.1),
            fallback_size: Distance::Normalized(0.04),
            armed_throughout: true,
        }
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureError {
    Unarmed,
    Cancelled,
    SourceChanged,
    MappingChanged,
    ContactsChanged,
    BackwardTime,
    Timeout,
    SampleLimit,
    InvalidGeometry,
    TooSmall,
}
impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for CaptureError {}

/// Constructed only by a successfully completed capture, including size checks.
#[derive(Clone, Debug)]
pub struct Stroke {
    pub identity: CaptureIdentity,
    pub started: Duration,
    pub finished: Duration,
    pub space: Space,
    points: Vec<Point>,
}
impl Stroke {
    pub fn points(&self) -> &[Point] {
        &self.points
    }
    /// Export one reusable template. This does not create or enable a rule.
    pub fn template(
        &self,
        name: String,
        threshold: f64,
        margin: f64,
    ) -> Result<ShapeTemplate, &'static str> {
        let template = ShapeTemplate {
            name,
            points: resample(&self.points)?
                .into_iter()
                .map(|p| [p.x, p.y])
                .collect(),
            threshold,
            margin,
            rotation_invariant: false,
            reflection_invariant: false,
        };
        validate_template(&template)?;
        Ok(template)
    }
}
pub struct PathCapture {
    identity: CaptureIdentity,
    limits: CaptureLimits,
    started: Duration,
    last_at: Duration,
    samples: Vec<Position>,
    error: Option<CaptureError>,
}
impl PathCapture {
    pub fn new(
        identity: CaptureIdentity,
        started: Duration,
        limits: CaptureLimits,
        armed: bool,
    ) -> Result<Self, CaptureError> {
        if !armed {
            return Err(CaptureError::Unarmed);
        }
        if identity.source.seat.is_empty()
            || identity.contacts.len() > 64
            || !(2..=65536).contains(&limits.max_samples)
            || limits.timeout.is_zero()
            || limits.timeout > Duration::from_secs(3600)
            || !valid_distance(limits.minimum_length)
            || !valid_distance(limits.minimum_size)
            || !valid_fallback(limits.fallback_length)
            || !valid_fallback(limits.fallback_size)
        {
            return Err(CaptureError::InvalidGeometry);
        }
        Ok(Self {
            identity,
            limits,
            started,
            last_at: started,
            samples: Vec::new(),
            error: None,
        })
    }
    pub fn sample(
        &mut self,
        identity: &CaptureIdentity,
        at: Duration,
        position: Position,
        armed: bool,
    ) -> Result<(), CaptureError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let error = if identity.source != self.identity.source {
            Some(CaptureError::SourceChanged)
        } else if identity.mapping_generation != self.identity.mapping_generation {
            Some(CaptureError::MappingChanged)
        } else if identity.contacts != self.identity.contacts {
            Some(CaptureError::ContactsChanged)
        } else if self.limits.armed_throughout && !armed {
            Some(CaptureError::Unarmed)
        } else if at < self.last_at {
            Some(CaptureError::BackwardTime)
        } else if at.saturating_sub(self.started) > self.limits.timeout {
            Some(CaptureError::Timeout)
        } else if !valid_position(position)
            || self.samples.first().is_some_and(|first| {
                first.local.is_some() != position.local.is_some()
                    || first.millimetres.is_some() != position.millimetres.is_some()
            })
        {
            Some(CaptureError::InvalidGeometry)
        } else {
            None
        };
        if let Some(error) = error {
            self.error = Some(error);
            return Err(error);
        }
        self.last_at = at;
        if self.samples.last() == Some(&position) {
            return Ok(());
        }
        if self.samples.len() >= self.limits.max_samples {
            self.error = Some(CaptureError::SampleLimit);
            return Err(CaptureError::SampleLimit);
        }
        self.samples.push(position);
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.error = Some(CaptureError::Cancelled);
        self.samples.clear();
    }
    /// Multi-finger shapes follow the centroid of a fixed contact set. A lift
    /// or replacement cancels; paths from unrelated fingers never get spliced.
    pub fn sample_contacts(
        &mut self,
        source: &Source,
        mapping_generation: u64,
        at: Duration,
        contacts: &std::collections::BTreeMap<ContactId, Position>,
        armed: bool,
    ) -> Result<(), CaptureError> {
        if contacts.is_empty() || contacts.values().any(|p| !valid_position(*p)) {
            self.error = Some(CaptureError::InvalidGeometry);
            return Err(CaptureError::InvalidGeometry);
        }
        let first = *contacts.values().next().expect("nonempty contacts");
        if contacts.values().any(|p| {
            p.local.is_some() != first.local.is_some()
                || p.millimetres.is_some() != first.millimetres.is_some()
                || p.global.is_some() != first.global.is_some()
        }) {
            self.error = Some(CaptureError::InvalidGeometry);
            return Err(CaptureError::InvalidGeometry);
        }
        let mean = |points: Vec<Point>| Point {
            x: points.iter().map(|p| p.x).sum::<f64>() / points.len() as f64,
            y: points.iter().map(|p| p.y).sum::<f64>() / points.len() as f64,
        };
        let position = Position {
            normalized: mean(contacts.values().map(|p| p.normalized).collect()),
            local: first
                .local
                .map(|_| mean(contacts.values().filter_map(|p| p.local).collect())),
            millimetres: first
                .millimetres
                .map(|_| mean(contacts.values().filter_map(|p| p.millimetres).collect())),
            global: first
                .global
                .map(|_| mean(contacts.values().filter_map(|p| p.global).collect())),
        };
        self.sample(
            &CaptureIdentity {
                source: source.clone(),
                mapping_generation,
                contacts: contacts.keys().copied().collect(),
            },
            at,
            position,
            armed,
        )
    }
    pub fn deadline(&self) -> Duration {
        self.started.saturating_add(self.limits.timeout)
    }
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }
    pub fn finish(self, at: Duration) -> Result<Stroke, CaptureError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if at < self.last_at {
            return Err(CaptureError::BackwardTime);
        }
        if at.saturating_sub(self.started) > self.limits.timeout {
            return Err(CaptureError::Timeout);
        }
        if self.samples.len() < 2 {
            return Err(CaptureError::TooSmall);
        }
        let length = |points: Vec<Point>| {
            points
                .windows(2)
                .map(|p| p[1].minus(p[0]).length())
                .sum::<f64>()
        };
        let size = |points: Vec<Point>| extent(&points).x.max(extent(&points).y);
        let measure = |distance: Distance,
                       fallback: Distance,
                       metric: &dyn Fn(Vec<Point>) -> f64|
         -> Result<bool, CaptureError> {
            let (points, threshold) = match distance {
                Distance::Millimetres(v) if self.samples[0].millimetres.is_some() => (
                    self.samples.iter().filter_map(|p| p.millimetres).collect(),
                    v,
                ),
                Distance::Pixels(v) if self.samples[0].local.is_some() => {
                    (self.samples.iter().filter_map(|p| p.local).collect(), v)
                }
                Distance::Pixels(_) => return Err(CaptureError::InvalidGeometry),
                Distance::Normalized(v) => (self.samples.iter().map(|p| p.normalized).collect(), v),
                Distance::Millimetres(_) => (
                    self.samples.iter().map(|p| p.normalized).collect(),
                    fallback.value(),
                ),
            };
            Ok(metric(points) >= threshold)
        };
        if !measure(
            self.limits.minimum_length,
            self.limits.fallback_length,
            &length,
        )? || !measure(self.limits.minimum_size, self.limits.fallback_size, &size)?
        {
            return Err(CaptureError::TooSmall);
        }
        Ok(Stroke {
            identity: self.identity,
            started: self.started,
            finished: at,
            space: if self.samples[0].local.is_some() {
                Space::Logical
            } else {
                Space::Normalized
            },
            points: self.samples.iter().map(|p| p.point()).collect(),
        })
    }
}
fn valid_distance(distance: Distance) -> bool {
    distance.value().is_finite()
        && distance.value() > 0.0
        && distance.value()
            <= if matches!(distance, Distance::Normalized(_)) {
                2.0
            } else {
                1_000_000.0
            }
}
fn valid_fallback(distance: Distance) -> bool {
    matches!(distance, Distance::Normalized(_)) && valid_distance(distance)
}
fn valid_position(position: Position) -> bool {
    std::iter::once(position.normalized)
        .chain(position.millimetres)
        .chain(position.local)
        .chain(position.global)
        .all(|p| p.x.is_finite() && p.y.is_finite() && p.x.abs() <= 1e9 && p.y.abs() <= 1e9)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ShapeMatch {
    pub name: String,
    pub confidence: f64,
    pub margin: f64,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MatchOutcome {
    Matched {
        shape: ShapeMatch,
    },
    Rejected {
        best: Option<ShapeMatch>,
    },
    Ambiguous {
        best: ShapeMatch,
        runner_up: ShapeMatch,
    },
}
struct Compiled {
    template: ShapeTemplate,
    points: Vec<Point>,
}
pub struct Matcher {
    templates: Vec<Compiled>,
}
impl Matcher {
    pub fn new(templates: &[ShapeTemplate]) -> Result<Self, &'static str> {
        if templates.len() > MAX_TEMPLATES {
            return Err("too many shape templates");
        }
        let mut names = BTreeSet::new();
        let mut compiled = Vec::new();
        for template in templates {
            validate_template(template)?;
            if !names.insert(&template.name) {
                return Err("duplicate shape name");
            }
            compiled.push(Compiled {
                template: template.clone(),
                points: normalize(
                    &template
                        .points
                        .iter()
                        .map(|p| Point { x: p[0], y: p[1] })
                        .collect::<Vec<_>>(),
                )?,
            });
        }
        Ok(Self {
            templates: compiled,
        })
    }
    pub fn classify(&self, stroke: &Stroke) -> MatchOutcome {
        let Ok(points) = normalize(&stroke.points) else {
            return MatchOutcome::Rejected { best: None };
        };
        let mut ranked: Vec<_> = self
            .templates
            .iter()
            .map(|template| (template, confidence(&points, template)))
            .collect();
        ranked.sort_by(|(a, x), (b, y)| {
            y.total_cmp(x)
                .then_with(|| a.template.name.cmp(&b.template.name))
        });
        let Some((best, score)) = ranked.first() else {
            return MatchOutcome::Rejected { best: None };
        };
        let runner = ranked.get(1).map_or(0.0, |(_, score)| *score);
        let result = ShapeMatch {
            name: best.template.name.clone(),
            confidence: *score,
            margin: score - runner,
        };
        if *score < best.template.threshold {
            MatchOutcome::Rejected { best: Some(result) }
        } else if score - runner < best.template.margin {
            let (second, confidence) = ranked[1];
            MatchOutcome::Ambiguous {
                best: result,
                runner_up: ShapeMatch {
                    name: second.template.name.clone(),
                    confidence,
                    margin: 0.0,
                },
            }
        } else {
            MatchOutcome::Matched { shape: result }
        }
    }
}
pub fn validate_template(template: &ShapeTemplate) -> Result<(), &'static str> {
    if template.name.is_empty()
        || template.name.len() > 128
        || template.name.contains('\0')
        || !(2..=2048).contains(&template.points.len())
        || !template.threshold.is_finite()
        || !(0.0..=1.0).contains(&template.threshold)
        || template.threshold == 0.0
        || !template.margin.is_finite()
        || !(0.0..=1.0).contains(&template.margin)
        || template.margin == 0.0
        || template
            .points
            .iter()
            .flatten()
            .any(|p| !p.is_finite() || p.abs() > 1e9)
    {
        return Err("invalid shape name, bounds or acceptance thresholds");
    }
    normalize(
        &template
            .points
            .iter()
            .map(|p| Point { x: p[0], y: p[1] })
            .collect::<Vec<_>>(),
    )?;
    Ok(())
}

/// Demonstration templates carry no action bindings.
pub fn demo_templates() -> Vec<ShapeTemplate> {
    let template = |name: &str, points| ShapeTemplate {
        name: name.into(),
        points,
        threshold: 0.94,
        margin: 0.04,
        rotation_invariant: false,
        reflection_invariant: false,
    };
    vec![
        template("v", vec![[0.0, 0.0], [0.5, 1.0], [1.0, 0.0]]),
        template(
            "circle",
            (0..=64)
                .map(|index| {
                    let angle = index as f64 * std::f64::consts::TAU / 64.0;
                    [angle.cos(), angle.sin()]
                })
                .collect(),
        ),
        template(
            "zigzag",
            vec![[0.0, 0.0], [0.25, 1.0], [0.5, 0.0], [0.75, 1.0], [1.0, 0.0]],
        ),
    ]
}
/// Arc-length resampling ignores drawing speed while preserving point order.
fn resample(points: &[Point]) -> Result<Vec<Point>, &'static str> {
    if points.len() < 2 || points.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
        return Err("invalid path");
    }
    let mut cumulative = vec![0.0];
    for pair in points.windows(2) {
        cumulative
            .push(cumulative.last().expect("initial length") + pair[1].minus(pair[0]).length());
    }
    let length = *cumulative.last().expect("path length");
    if !length.is_finite() || length <= 1e-12 {
        return Err("degenerate path");
    }
    let mut segment = 1;
    let mut sampled = Vec::with_capacity(RESAMPLE_POINTS);
    for index in 0..RESAMPLE_POINTS {
        let target = length * index as f64 / (RESAMPLE_POINTS - 1) as f64;
        while segment < points.len() - 1 && cumulative[segment] < target {
            segment += 1;
        }
        let distance = cumulative[segment] - cumulative[segment - 1];
        let fraction = if distance > 0.0 {
            (target - cumulative[segment - 1]) / distance
        } else {
            0.0
        };
        let delta = points[segment].minus(points[segment - 1]).scale(fraction);
        sampled.push(Point {
            x: points[segment - 1].x + delta.x,
            y: points[segment - 1].y + delta.y,
        });
    }
    // Duplicate trailing samples must not displace the actual final endpoint.
    sampled[RESAMPLE_POINTS - 1] = *points.last().expect("endpoint");
    Ok(sampled)
}
fn extent(points: &[Point]) -> Point {
    let (mut min, mut max) = (
        Point {
            x: f64::INFINITY,
            y: f64::INFINITY,
        },
        Point {
            x: f64::NEG_INFINITY,
            y: f64::NEG_INFINITY,
        },
    );
    for point in points {
        min.x = min.x.min(point.x);
        min.y = min.y.min(point.y);
        max.x = max.x.max(point.x);
        max.y = max.y.max(point.y);
    }
    max.minus(min)
}
fn normalize(points: &[Point]) -> Result<Vec<Point>, &'static str> {
    let points = resample(points)?;
    let size = extent(&points);
    let size = size.x.max(size.y);
    if size < 1e-12 || !size.is_finite() {
        return Err("degenerate shape extent");
    }
    let centre = Point {
        x: points.iter().map(|p| p.x).sum::<f64>() / points.len() as f64,
        y: points.iter().map(|p| p.y).sum::<f64>() / points.len() as f64,
    };
    Ok(points
        .iter()
        .map(|p| p.minus(centre).scale(1.0 / size))
        .collect())
}
fn confidence(input: &[Point], template: &Compiled) -> f64 {
    let mut best: f64 = 0.0;
    let reflections: &[(f64, f64)] = if template.template.reflection_invariant {
        &[(1.0, 1.0), (-1.0, 1.0), (1.0, -1.0)]
    } else {
        &[(1.0, 1.0)]
    };
    for &(x, y) in reflections {
        let mut points: Vec<_> = input
            .iter()
            .map(|p| Point {
                x: p.x * x,
                y: p.y * y,
            })
            .collect();
        if template.template.rotation_invariant {
            let (dot, cross) = points
                .iter()
                .zip(&template.points)
                .fold((0.0, 0.0), |(dot, cross), (a, b)| {
                    (dot + a.x * b.x + a.y * b.y, cross + a.x * b.y - a.y * b.x)
                });
            let angle = cross.atan2(dot);
            let (sin, cos) = angle.sin_cos();
            for point in &mut points {
                *point = Point {
                    x: point.x * cos - point.y * sin,
                    y: point.x * sin + point.y * cos,
                };
            }
            // Rotation changes axis-aligned extent; recover the template's
            // size convention before comparing points, preserving aspect ratio.
            let size = extent(&points);
            let size = size.x.max(size.y);
            if size > 1e-12 {
                for point in &mut points {
                    *point = point.scale(1.0 / size);
                }
            }
        }
        let rms = (points
            .iter()
            .zip(&template.points)
            .map(|(a, b)| {
                let d = a.minus(*b);
                d.x * d.x + d.y * d.y
            })
            .sum::<f64>()
            / RESAMPLE_POINTS as f64)
            .sqrt();
        best = best.max((1.0 - rms / std::f64::consts::SQRT_2).clamp(0.0, 1.0));
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> CaptureIdentity {
        CaptureIdentity {
            source: Source {
                device: 42,
                generation: 1,
                seat: "seat0".into(),
            },
            mapping_generation: 1,
            contacts: BTreeSet::new(),
        }
    }
    fn position(x: f64, y: f64) -> Position {
        Position {
            normalized: Point { x, y },
            millimetres: Some(Point {
                x: x * 100.0,
                y: y * 100.0,
            }),
            local: None,
            global: None,
        }
    }
    fn stroke(points: &[[f64; 2]], scale: f64, offset: Point) -> Stroke {
        let mut capture =
            PathCapture::new(identity(), Duration::ZERO, CaptureLimits::default(), true).unwrap();
        for (index, point) in points.iter().enumerate() {
            capture
                .sample(
                    &identity(),
                    Duration::from_millis(index as u64 * 10),
                    position(point[0] * scale + offset.x, point[1] * scale + offset.y),
                    true,
                )
                .unwrap();
        }
        capture
            .finish(Duration::from_millis(points.len() as u64 * 10))
            .unwrap()
    }
    fn matched(outcome: MatchOutcome, name: &str) {
        assert!(
            matches!(outcome, MatchOutcome::Matched { ref shape } if shape.name == name),
            "{outcome:?}"
        );
    }
    #[test]
    fn t33_v_sizes_translation_and_uneven_sampling_match() {
        let templates = demo_templates();
        let matcher = Matcher::new(&templates).unwrap();
        for scale in [0.25, 0.5, 1.0, 2.0] {
            matched(
                matcher.classify(&stroke(
                    &templates[0].points,
                    scale,
                    Point { x: 0.1, y: 0.2 },
                )),
                "v",
            );
            matched(
                matcher.classify(&stroke(
                    &[
                        [0.0, 0.0],
                        [0.01, 0.02],
                        [0.25, 0.5],
                        [0.5, 1.0],
                        [0.95, 0.1],
                        [1.0, 0.0],
                    ],
                    scale,
                    Point::default(),
                )),
                "v",
            );
        }
    }
    #[test]
    fn t33_circle_and_zigzag_demonstrations_are_distinct() {
        let templates = demo_templates();
        let matcher = Matcher::new(&templates).unwrap();
        for template in &templates {
            matched(
                matcher.classify(&stroke(&template.points, 1.0, Point::default())),
                &template.name,
            );
        }
    }
    #[test]
    fn t34_inverted_v_straight_line_and_tiny_capture_are_rejected() {
        let matcher = Matcher::new(&demo_templates()).unwrap();
        for points in [
            vec![[0.0, 1.0], [0.5, 0.0], [1.0, 1.0]],
            vec![[0.0, 0.0], [1.0, 0.0]],
        ] {
            assert!(matches!(
                matcher.classify(&stroke(&points, 1.0, Point::default())),
                MatchOutcome::Rejected { .. }
            ));
        }
        let mut capture =
            PathCapture::new(identity(), Duration::ZERO, CaptureLimits::default(), true).unwrap();
        for (index, p) in [[0.0, 0.0], [0.001, 0.002], [0.002, 0.0]]
            .into_iter()
            .enumerate()
        {
            capture
                .sample(
                    &identity(),
                    Duration::from_millis(index as u64),
                    position(p[0], p[1]),
                    true,
                )
                .unwrap();
        }
        assert_eq!(
            capture.finish(Duration::from_millis(3)).unwrap_err(),
            CaptureError::TooSmall
        );
    }
    #[test]
    fn t34_ambiguous_templates_never_pick_an_action() {
        let mut templates = vec![demo_templates()[0].clone()];
        let mut duplicate = templates[0].clone();
        duplicate.name = "similar-v".into();
        templates.push(duplicate);
        let matcher = Matcher::new(&templates).unwrap();
        assert!(matches!(
            matcher.classify(&stroke(&templates[0].points, 1.0, Point::default())),
            MatchOutcome::Ambiguous { .. }
        ));
    }
    #[test]
    fn t35_cancel_and_export_have_no_rule_side_effect() {
        let mut capture =
            PathCapture::new(identity(), Duration::ZERO, CaptureLimits::default(), true).unwrap();
        capture.cancel();
        assert_eq!(
            capture.finish(Duration::ZERO).unwrap_err(),
            CaptureError::Cancelled
        );
        let stroke = stroke(&demo_templates()[0].points, 0.5, Point { x: 0.25, y: 0.1 });
        let template = stroke.template("learned".into(), 0.95, 0.05).unwrap();
        assert_eq!(template.points.len(), RESAMPLE_POINTS);
        let text = toml::to_string(&template).unwrap();
        let restored: ShapeTemplate = toml::from_str(&text).unwrap();
        matched(
            Matcher::new(&[restored]).unwrap().classify(&stroke),
            "learned",
        );
    }
    #[test]
    fn t36_bounds_and_timeout_cancel_capture() {
        let limits = CaptureLimits {
            max_samples: 3,
            ..CaptureLimits::default()
        };
        let mut capture = PathCapture::new(identity(), Duration::ZERO, limits, true).unwrap();
        for index in 0..3 {
            capture
                .sample(
                    &identity(),
                    Duration::from_millis(index),
                    position(index as f64 / 10.0, 0.0),
                    true,
                )
                .unwrap();
        }
        assert_eq!(
            capture.sample(
                &identity(),
                Duration::from_millis(3),
                position(0.5, 0.0),
                true
            ),
            Err(CaptureError::SampleLimit)
        );
        assert_eq!(capture.sample_count(), 3);
        assert_eq!(
            capture.finish(Duration::from_millis(4)).unwrap_err(),
            CaptureError::SampleLimit
        );
        let mut capture =
            PathCapture::new(identity(), Duration::ZERO, CaptureLimits::default(), true).unwrap();
        assert_eq!(
            capture.sample(
                &identity(),
                Duration::from_secs(6),
                position(0.0, 0.0),
                true
            ),
            Err(CaptureError::Timeout)
        );
    }
    #[test]
    fn t37_rotation_and_reflection_are_explicit() {
        let template = demo_templates()[0].clone();
        let angle: f64 = 0.7;
        let (sin, cos) = angle.sin_cos();
        let rotated: Vec<_> = template
            .points
            .iter()
            .map(|p| [p[0] * cos - p[1] * sin, p[0] * sin + p[1] * cos])
            .collect();
        let rotated = stroke(&rotated, 0.5, Point::default());
        assert!(matches!(
            Matcher::new(std::slice::from_ref(&template))
                .unwrap()
                .classify(&rotated),
            MatchOutcome::Rejected { .. }
        ));
        let mut invariant = template.clone();
        invariant.rotation_invariant = true;
        matched(Matcher::new(&[invariant]).unwrap().classify(&rotated), "v");
        let flipped = stroke(&[[0.0, 1.0], [0.5, 0.0], [1.0, 1.0]], 0.5, Point::default());
        let mut invariant = template;
        invariant.reflection_invariant = true;
        matched(Matcher::new(&[invariant]).unwrap().classify(&flipped), "v");
    }
    #[test]
    fn t38_arming_may_be_sampled_at_start_or_required_throughout() {
        assert!(matches!(
            PathCapture::new(identity(), Duration::ZERO, CaptureLimits::default(), false),
            Err(CaptureError::Unarmed)
        ));
        for throughout in [true, false] {
            let limits = CaptureLimits {
                armed_throughout: throughout,
                ..CaptureLimits::default()
            };
            let mut capture = PathCapture::new(identity(), Duration::ZERO, limits, true).unwrap();
            capture
                .sample(&identity(), Duration::ZERO, position(0.0, 0.0), true)
                .unwrap();
            assert_eq!(
                capture
                    .sample(
                        &identity(),
                        Duration::from_millis(1),
                        position(0.5, 1.0),
                        false
                    )
                    .is_err(),
                throughout
            );
            assert_eq!(
                capture
                    .sample(
                        &identity(),
                        Duration::from_millis(2),
                        position(1.0, 0.0),
                        true
                    )
                    .is_err(),
                throughout
            );
        }
    }
    #[test]
    fn t39_sources_generations_and_contacts_cannot_be_spliced() {
        for change in 0..4 {
            let mut capture =
                PathCapture::new(identity(), Duration::ZERO, CaptureLimits::default(), true)
                    .unwrap();
            capture
                .sample(&identity(), Duration::ZERO, position(0.0, 0.0), true)
                .unwrap();
            let mut next = identity();
            match change {
                0 => next.source.device += 1,
                1 => next.source.generation += 1,
                2 => next.mapping_generation += 1,
                _ => {
                    next.contacts.insert(ContactId {
                        slot: 0,
                        tracking: 1,
                        generation: 1,
                    });
                }
            }
            assert!(
                capture
                    .sample(&next, Duration::from_millis(1), position(1.0, 1.0), true)
                    .is_err()
            );
            assert!(capture.finish(Duration::from_millis(2)).is_err());
        }
    }
    #[test]
    fn t36_touch_paths_use_a_fixed_centroid_membership() {
        let contacts = BTreeSet::from([
            ContactId {
                slot: 0,
                tracking: 1,
                generation: 1,
            },
            ContactId {
                slot: 1,
                tracking: 2,
                generation: 2,
            },
        ]);
        let mut identity = identity();
        identity.contacts = contacts.clone();
        let mut capture = PathCapture::new(
            identity.clone(),
            Duration::ZERO,
            CaptureLimits::default(),
            true,
        )
        .unwrap();
        for (index, p) in demo_templates()[0].points.iter().enumerate() {
            let points = contacts
                .iter()
                .enumerate()
                .map(|(i, id)| (*id, position(p[0] + i as f64 * 0.1, p[1])))
                .collect();
            capture
                .sample_contacts(
                    &identity.source,
                    1,
                    Duration::from_millis(index as u64),
                    &points,
                    true,
                )
                .unwrap();
        }
        matched(
            Matcher::new(&demo_templates())
                .unwrap()
                .classify(&capture.finish(Duration::from_millis(4)).unwrap()),
            "v",
        );
    }
    #[test]
    fn t36_degenerate_and_nonfinite_templates_are_invalid() {
        let mut template = demo_templates()[0].clone();
        template.points = vec![[0.0, 0.0]; 3];
        assert!(Matcher::new(&[template.clone()]).is_err());
        template.points[1][0] = f64::NAN;
        assert!(Matcher::new(&[template]).is_err());
        assert_eq!(
            resample(&[
                Point::default(),
                Point::default(),
                Point { x: 1.0, y: 0.0 },
                Point { x: 1.0, y: 0.0 }
            ])
            .unwrap()
            .last(),
            Some(&Point { x: 1.0, y: 0.0 })
        );
    }
}
