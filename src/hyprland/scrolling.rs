//! Numerical planning for a continuous vertical scrolling-layout gesture.
//!
//! This module opens no IPC connection and grants no execution authority.
//! Callers must independently verify the layout, bounds, frozen target and
//! per-attempt permission. Production `ScrollStack` remains unavailable.
use std::{io, time::Duration};

pub const UPDATE_INTERVAL: Duration = Duration::from_millis(16);
const MAX_COORDINATE: f64 = 1_000_000.;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Down,
    Up,
}

/// Native camera offset and effective logical extents, including layout rules.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    direction: Direction,
    offset: f64,
    min: f64,
    max: f64,
}
impl Viewport {
    pub fn new(
        direction: Direction,
        viewport_extent: f64,
        content_extent: f64,
        offset: f64,
    ) -> io::Result<Self> {
        if [viewport_extent, content_extent]
            .iter()
            .any(|value| !value.is_finite() || *value <= 0. || *value > MAX_COORDINATE)
            || !offset.is_finite()
        {
            return Err(invalid("invalid native viewport extents"));
        }
        let excess = content_extent - viewport_extent;
        let (min, max) = if excess < 0. {
            let center = (excess / 2.).round();
            (center, center)
        } else {
            (0., excess)
        };
        if !(min..=max).contains(&offset) {
            return Err(invalid("native viewport is outside content bounds"));
        }
        Ok(Self {
            direction,
            offset,
            min,
            max,
        })
    }
    pub fn bounds(self) -> (f64, f64) {
        (self.min, self.max)
    }
    pub fn offset(self) -> f64 {
        self.offset
    }
    fn desired(self, total_y: f64) -> io::Result<f64> {
        if !total_y.is_finite() || total_y.abs() > MAX_COORDINATE {
            return Err(invalid("invalid cumulative logical displacement"));
        }
        let change = match self.direction {
            Direction::Down => -total_y,
            Direction::Up => total_y,
        };
        Ok((self.offset + change).clamp(self.min, self.max))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Update,
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Active,
    Ending,
    Finished,
    Cancelled,
    Uncertain,
}

/// One non-retryable numerical mutation, not an IPC command or grant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Attempt {
    gesture: u64,
    sequence: u64,
    ordinal: u64,
    from: f64,
    to: f64,
}
impl Attempt {
    pub fn ordinal(self) -> u64 {
        self.ordinal
    }
    pub fn from(self) -> f64 {
        self.from
    }
    pub fn to(self) -> f64 {
        self.to
    }
    /// Numeric `layoutmsg move` negates the native camera-offset change.
    pub fn layout_move(self) -> f64 {
        self.from - self.to
    }
}

/// One immutable viewport, one in-flight attempt and one coalesced destination.
/// The caller supplies a unique, nonzero gesture ID and monotonic times.
pub struct Motion {
    gesture: u64,
    viewport: Viewport,
    desired: f64,
    confirmed: f64,
    ordinal: u64,
    sequence: u64,
    in_flight: Option<Attempt>,
    state: State,
    last_time: Duration,
    next_attempt: Duration,
}
impl Motion {
    /// Begin has ordinal one; displacement includes recognition's initial travel.
    pub fn begin(
        gesture: u64,
        viewport: Viewport,
        total_y: f64,
        now: Duration,
    ) -> io::Result<Self> {
        if gesture == 0 {
            return Err(invalid("gesture identity is zero"));
        }
        Ok(Self {
            gesture,
            viewport,
            desired: viewport.desired(total_y)?,
            confirmed: viewport.offset,
            ordinal: 1,
            sequence: 0,
            in_flight: None,
            state: State::Active,
            last_time: now,
            next_attempt: now,
        })
    }
    pub fn state(&self) -> State {
        self.state
    }
    pub fn confirmed_offset(&self) -> f64 {
        self.confirmed
    }
    pub fn in_flight(&self) -> Option<Attempt> {
        self.in_flight
    }
    /// Ordinal gaps are allowed. Repeated, reordered and post-terminal progress
    /// is rejected without replacing the current coalesced destination.
    pub fn progress(
        &mut self,
        ordinal: u64,
        phase: Phase,
        total_y: f64,
        now: Duration,
    ) -> io::Result<()> {
        self.check_time(now)?;
        if self.state != State::Active || ordinal <= self.ordinal {
            return Err(invalid("retired or reordered scrolling progress"));
        }
        let desired = self.viewport.desired(total_y)?;
        self.ordinal = ordinal;
        self.desired = desired;
        self.last_time = now;
        if phase == Phase::End {
            self.state = State::Ending;
        }
        self.finish_if_settled();
        Ok(())
    }
    /// Pacing applies to end too. A blocked attempt is never replaced or retried.
    pub fn take(&mut self, now: Duration) -> io::Result<Option<Attempt>> {
        self.check_time(now)?;
        if !matches!(self.state, State::Active | State::Ending)
            || self.in_flight.is_some()
            || now < self.next_attempt
            || self.desired == self.confirmed
        {
            self.last_time = now;
            return Ok(None);
        }
        let next_attempt = now
            .checked_add(UPDATE_INTERVAL)
            .ok_or_else(|| invalid("scrolling update clock exhausted"))?;
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("scrolling attempt identity exhausted"))?;
        let attempt = Attempt {
            gesture: self.gesture,
            sequence,
            ordinal: self.ordinal,
            from: self.confirmed,
            to: self.desired,
        };
        self.sequence = sequence;
        self.in_flight = Some(attempt);
        self.next_attempt = next_attempt;
        self.last_time = now;
        Ok(Some(attempt))
    }
    /// Only an exact observed native offset confirms a mutation. Missing,
    /// mismatched or nonfinite observations retire the motion without retry.
    pub fn settle(
        &mut self,
        attempt: Attempt,
        observed_offset: Option<f64>,
        now: Duration,
    ) -> io::Result<()> {
        self.check_time(now)?;
        if self.in_flight != Some(attempt) {
            return Err(invalid("unknown or settled scrolling attempt"));
        }
        self.in_flight = None;
        self.last_time = now;
        if observed_offset == Some(attempt.to) {
            self.confirmed = attempt.to;
            self.finish_if_settled();
        } else {
            self.state = State::Uncertain;
            self.desired = self.confirmed;
        }
        Ok(())
    }
    /// Discard pending progress without a compensating viewport movement.
    /// An in-flight attempt still requires settlement or an uncertain receipt.
    pub fn cancel(&mut self, now: Duration) -> io::Result<()> {
        self.check_time(now)?;
        if matches!(self.state, State::Active | State::Ending) {
            self.state = State::Cancelled;
            self.desired = self.confirmed;
        }
        self.last_time = now;
        Ok(())
    }
    fn check_time(&self, now: Duration) -> io::Result<()> {
        if now < self.last_time {
            return Err(invalid("scrolling clock moved backwards"));
        }
        Ok(())
    }
    fn finish_if_settled(&mut self) {
        if self.state == State::Ending && self.in_flight.is_none() && self.desired == self.confirmed
        {
            self.state = State::Finished;
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
