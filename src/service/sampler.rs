//! Desktop context from the Wayland protocol, the kernel and logind only.

use crate::{
    config::Screen,
    context::{OutputState, SessionKind, Snapshot, State},
    gestures::{Output, Point, Transform},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::Path,
    time::Instant,
};

const MAX_HISTORY: usize = 4096;

/// Assigns each distinct output geometry a generation that is never reused,
/// so a removed and re-added output cannot match an older permit.
#[derive(Default)]
pub(super) struct Sampler {
    revision: u64,
    history: BTreeMap<String, (Output, u64)>,
    active: BTreeSet<String>,
}
impl Sampler {
    pub(super) fn sample(
        &mut self,
        socket: &Path,
        uid: u32,
        generation: u64,
        kind: SessionKind,
        locked: bool,
        until: Instant,
    ) -> io::Result<Snapshot> {
        let captured_at = crate::ipc::monotonic()?;
        let listed = crate::fsuid::with_fsuid(uid, || crate::display::outputs(socket, until))?;
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("context revision exhausted"))?;
        let mut history = self.history.clone();
        let mut outputs = Vec::with_capacity(listed.len());
        for output in &listed {
            let geometry = geometry(output)?;
            let geometry_generation = match self.history.get(&geometry.name) {
                Some((old, generation))
                    if *old == geometry && self.active.contains(&geometry.name) =>
                {
                    *generation
                }
                Some((_, generation)) => generation
                    .checked_add(1)
                    .ok_or_else(|| invalid("output geometry generation exhausted"))?,
                None => 1,
            };
            history.insert(
                geometry.name.clone(),
                (geometry.clone(), geometry_generation),
            );
            let screen = match crate::display::screen_on(&geometry.name)? {
                Some(false) => Screen::Off,
                _ => Screen::On,
            };
            outputs.push(OutputState {
                geometry,
                geometry_generation,
                screen,
            });
        }
        if history.len() > MAX_HISTORY {
            return Err(invalid("output identity history exhausted"));
        }
        let state = match (kind, locked) {
            (_, true) => State::Locked,
            (SessionKind::User, false) => State::Desktop,
            (SessionKind::Greeter, false) => State::Greeter,
        };
        let snapshot = Snapshot {
            generation,
            revision,
            captured_at,
            state,
            outputs,
        };
        snapshot.validate().map_err(io::Error::other)?;
        self.active = listed.iter().map(|output| output.name.clone()).collect();
        self.history = history;
        self.revision = revision;
        Ok(snapshot)
    }
}

fn geometry(output: &crate::display::Output) -> io::Result<Output> {
    let transform = match output.transform {
        0 => Transform::Normal,
        1 => Transform::Rotate90,
        2 => Transform::Rotate180,
        3 => Transform::Rotate270,
        4 => Transform::Flipped,
        5 => Transform::Flipped90,
        6 => Transform::Flipped180,
        7 => Transform::Flipped270,
        _ => return Err(invalid("unknown output transform")),
    };
    let swapped = transform.swapped();
    let horizontal = if swapped {
        output.pixel_height
    } else {
        output.pixel_width
    };
    if output.width <= 0 || output.height <= 0 || horizontal <= 0 {
        return Err(invalid("output has no usable size"));
    }
    Ok(Output {
        name: output.name.clone(),
        pixel_width: f64::from(output.pixel_width),
        pixel_height: f64::from(output.pixel_height),
        scale: f64::from(horizontal) / f64::from(output.width),
        origin: Point {
            x: f64::from(output.x),
            y: f64::from(output.y),
        },
        transform,
    })
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
