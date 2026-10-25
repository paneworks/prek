//! Filesystem credentials of the calling thread.
use nix::libc;
use std::io;

fn set(uid: u32) -> io::Result<()> {
    // SAFETY: setfsuid changes only this thread's filesystem uid; a second call
    // with the same value returns the value now in effect.
    let applied = unsafe {
        libc::setfsuid(uid);
        libc::setfsuid(uid)
    };
    if applied as u32 != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "filesystem uid change was refused",
        ));
    }
    Ok(())
}

/// Runs `action` with this thread's filesystem uid set to `uid`, so path
/// permission checks match that user, then restores the previous value.
pub(crate) fn with_fsuid<T>(uid: u32, action: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    // SAFETY: reading the current value through an identity change is side-effect free.
    let previous = unsafe { libc::setfsuid(u32::MAX) } as u32;
    set(uid)?;
    let result = action();
    set(previous)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_own_uid_is_accepted_and_restored() {
        let uid = unsafe { libc::geteuid() };
        assert_eq!(with_fsuid(uid, || Ok(7)).unwrap(), 7);
        assert_eq!(unsafe { libc::setfsuid(u32::MAX) } as u32, uid);
    }

    #[test]
    fn errors_from_the_action_still_restore_the_previous_uid() {
        let uid = unsafe { libc::geteuid() };
        assert!(with_fsuid(uid, || Err::<(), _>(io::Error::other("x"))).is_err());
        assert_eq!(unsafe { libc::setfsuid(u32::MAX) } as u32, uid);
    }
}
