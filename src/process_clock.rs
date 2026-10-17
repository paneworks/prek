//! Monotonic timestamps within one suspend-sensitive process authority epoch.
use nix::time::{ClockId, clock_gettime};
use std::{io, sync::Mutex, time::Duration};

static PROCESS: Mutex<Epoch> = Mutex::new(Epoch::new());

struct Epoch {
    offset: Option<(i128, i128)>,
    last: Option<Duration>,
    failed: bool,
}
impl Epoch {
    const fn new() -> Self {
        Self {
            offset: None,
            last: None,
            failed: false,
        }
    }
    fn observe(
        &mut self,
        before: Duration,
        boot: Duration,
        after: Duration,
    ) -> io::Result<Duration> {
        if self.failed || after < before || self.last.is_some_and(|last| before < last) {
            self.failed = true;
            return Err(invalidated());
        }
        let low = boot.as_nanos() as i128 - after.as_nanos() as i128;
        let high = boot.as_nanos() as i128 - before.as_nanos() as i128;
        let (low, high) = self.offset.map_or((low, high), |(old_low, old_high)| {
            (old_low.max(low), old_high.min(high))
        });
        if low > high {
            self.failed = true;
            return Err(invalidated());
        }
        self.offset = Some((low, high));
        self.last = Some(after);
        Ok(after)
    }
}
fn invalidated() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "process clock epoch invalidated; restart and re-establish session/input authority",
    )
}
fn read(clock: ClockId) -> io::Result<Duration> {
    let time = clock_gettime(clock).map_err(io::Error::from)?;
    let seconds = u64::try_from(time.tv_sec()).map_err(|_| invalidated())?;
    let nanos = u32::try_from(time.tv_nsec())
        .ok()
        .filter(|nanos| *nanos < 1_000_000_000)
        .ok_or_else(invalidated)?;
    Ok(Duration::new(seconds, nanos))
}
fn sample(
    epoch: &Mutex<Epoch>,
    mut read: impl FnMut(ClockId) -> io::Result<Duration>,
) -> io::Result<Duration> {
    let mut state = epoch.lock().map_err(|_| invalidated())?;
    if state.failed {
        return Err(invalidated());
    }
    let result = (|| {
        let before = read(ClockId::CLOCK_MONOTONIC)?;
        let boot = read(ClockId::CLOCK_BOOTTIME)?;
        let after = read(ClockId::CLOCK_MONOTONIC)?;
        state.observe(before, boot, after)
    })();
    result.map_err(|error| {
        state.failed = true;
        if error.kind() == io::ErrorKind::ConnectionAborted {
            error
        } else {
            io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("process clock unavailable; restart and re-establish authority: {error}"),
            )
        }
    })
}
pub(crate) fn monotonic() -> io::Result<Duration> {
    sample(&PROCESS, read)
}

#[cfg(test)]
mod tests;
