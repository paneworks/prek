//! Pinned logind sleep notifications and delay-only pre-sleep lifecycle barriers.
mod bus;
use bus::{Api, Callback, Connection, Message, Reply, Slot, checked, invalid};
use nix::libc;
use std::{
    cell::Cell,
    ffi::{CStr, CString},
    io,
    os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd},
    path::Path,
    ptr::NonNull,
    time::Duration,
};

const LOGIN: &CStr = c"org.freedesktop.login1";
const OBJECT: &CStr = c"/org/freedesktop/login1";
const MANAGER: &CStr = c"org.freedesktop.login1.Manager";
const SYSTEM_BUS: &CStr = c"unix:path=/run/dbus/system_bus_socket";

/// Process ID the system bus recorded when the unique connection `name` connected.
pub(crate) fn connection_pid(library: &crate::session::Library, name: &CStr) -> io::Result<u32> {
    Connection::open(Api::load(library)?, SYSTEM_BUS)?.pid(name)
}
/// One retained system-bus connection for logind lock-hint queries that counts
/// the property changes logind announces for its session objects.
pub(crate) struct LockHints {
    slots: Vec<NonNull<Slot>>,
    bus: Connection,
    state: Box<Notices>,
    fd: Option<OwnedFd>,
}
impl LockHints {
    pub(crate) fn open(library: &crate::session::Library) -> io::Result<Self> {
        let api = Api::load(library)?;
        let bus = Connection::open(api, SYSTEM_BUS)?;
        let owner = bus.string(c"GetNameOwner", LOGIN)?;
        if !owner.to_bytes().starts_with(b":") || bus.uid(&owner)? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "lock-hint provider has no authorized unique bus owner",
            ));
        }
        let mut hints = Self {
            slots: Vec::new(),
            bus,
            state: Box::new(Notices {
                api,
                owner,
                changes: Cell::new(0),
                failed: Cell::new(false),
            }),
            fd: None,
        };
        let data = (&raw const *hints.state).cast_mut().cast();
        let rule = CString::new(format!(
            "type='signal',sender='{}',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged',path_namespace='/org/freedesktop/login1/session',arg0='org.freedesktop.login1.Session'",
            hints.state.owner.to_str().map_err(|_| invalid("invalid provider bus name"))?
        ))
        .unwrap();
        hints
            .slots
            .push(add_match(&hints.bus, &rule, session_changed, data)?);
        hints
            .slots
            .push(add_match(&hints.bus, LOGIN_OWNER, provider_replaced, data)?);
        if hints.bus.string(c"GetNameOwner", LOGIN)? != hints.state.owner {
            return Err(invalid("lock-hint provider changed during subscription"));
        }
        hints.fd = Some(pinned(&hints.bus)?);
        Ok(hints)
    }
    pub(crate) fn session_path(&self, session: &CStr) -> io::Result<CString> {
        self.bus.session_path(session)
    }
    pub(crate) fn locked(&self, path: &CStr) -> io::Result<bool> {
        self.bus.locked_hint(path)
    }
    /// Dispatches queued messages and returns how many session property
    /// changes arrived so far; a replaced provider is an error.
    pub(crate) fn changes(&mut self) -> io::Result<u64> {
        for _ in 0..64 {
            let result = checked(unsafe {
                (self.bus.api.process)(self.bus.ptr.as_ptr(), std::ptr::null_mut())
            })?;
            if self.state.failed.get() {
                return Err(invalid("lock-hint provider was replaced"));
            }
            if result == 0 {
                return Ok(self.state.changes.get());
            }
        }
        Err(invalid("lock-hint notification queue exceeds work bound"))
    }
    pub(crate) fn events(&self) -> io::Result<i16> {
        Ok(checked(unsafe { (self.bus.api.events)(self.bus.ptr.as_ptr()) })? as i16)
    }
}
impl AsFd for LockHints {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_ref().unwrap().as_fd()
    }
}
impl Drop for LockHints {
    fn drop(&mut self) {
        for slot in self.slots.drain(..) {
            unsafe {
                (self.bus.api.slot_unref)(slot.as_ptr());
            }
        }
    }
}
/// Session property changes announced by one logind bus owner.
struct Notices {
    api: Api,
    owner: CString,
    changes: Cell<u64>,
    failed: Cell<bool>,
}
unsafe extern "C" fn session_changed(
    message: *mut Message,
    data: *mut libc::c_void,
    _error: *mut libc::c_void,
) -> i32 {
    // SAFETY: the retained slot points to these pinned notices until slot removal.
    let notices = unsafe { &*data.cast::<Notices>() };
    let sender = unsafe { (notices.api.sender)(message) };
    match notices.changes.get().checked_add(1) {
        Some(changes)
            if !sender.is_null()
                && unsafe { CStr::from_ptr(sender) } == notices.owner.as_c_str() =>
        {
            notices.changes.set(changes)
        }
        _ => notices.failed.set(true),
    }
    1
}
unsafe extern "C" fn provider_replaced(
    _message: *mut Message,
    data: *mut libc::c_void,
    _error: *mut libc::c_void,
) -> i32 {
    let notices = unsafe { &*data.cast::<Notices>() };
    notices.failed.set(true);
    1
}

const LOGIN_OWNER: &CStr = c"type='signal',sender='org.freedesktop.DBus',path='/org/freedesktop/DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.freedesktop.login1'";

/// Installs a signal match whose callback receives `data`.
fn add_match(
    bus: &Connection,
    rule: &CStr,
    callback: Callback,
    data: *mut libc::c_void,
) -> io::Result<NonNull<Slot>> {
    let mut slot = std::ptr::null_mut();
    checked(unsafe {
        (bus.api.add_match)(
            bus.ptr.as_ptr(),
            &mut slot,
            rule.as_ptr(),
            Some(callback),
            data,
        )
    })?;
    NonNull::new(slot).ok_or_else(|| invalid("missing bus subscription"))
}

/// A close-on-exec duplicate of the bus descriptor for polling.
fn pinned(bus: &Connection) -> io::Result<OwnedFd> {
    let fd = checked(unsafe { (bus.api.fd)(bus.ptr.as_ptr()) })?;
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || flags & libc::FD_CLOEXEC == 0 {
        return Err(invalid("system bus descriptor is inheritable"));
    }
    let pin = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if pin < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(pin) })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Snapshot {
    pub preparing: bool,
    pub generation: u64,
}
#[derive(Default)]
struct Lifecycle {
    value: Cell<Snapshot>,
    failed: Cell<bool>,
    events: Cell<u64>,
}
impl Lifecycle {
    fn observe(&self, preparing: bool) {
        if self.failed.get() {
            return;
        }
        let Some(events) = self.events.get().checked_add(1) else {
            self.failed.set(true);
            return;
        };
        let mut value = self.value.get();
        if preparing {
            let Some(generation) = value.generation.checked_add(1) else {
                self.failed.set(true);
                return;
            };
            value.generation = generation;
        }
        value.preparing = preparing;
        self.events.set(events);
        self.value.set(value);
    }
}
struct Mailbox {
    api: Api,
    owner: CString,
    lifecycle: Lifecycle,
}
unsafe extern "C" fn prepare(
    message: *mut Message,
    data: *mut libc::c_void,
    _error: *mut libc::c_void,
) -> i32 {
    // SAFETY: the retained slot points to this pinned mailbox until slot removal.
    let state = unsafe { &*data.cast::<Mailbox>() };
    let mut value = 0i32;
    let sender = unsafe { (state.api.sender)(message) };
    let valid = !sender.is_null()
        && unsafe { CStr::from_ptr(sender) } == state.owner.as_c_str()
        && unsafe { (state.api.signature)(message, c"b".as_ptr()) } == 1
        && unsafe {
            (state.api.read)(
                message,
                b'b' as libc::c_char,
                (&mut value as *mut i32).cast(),
            )
        } == 1
        && unsafe { (state.api.end)(message, 1) } == 1
        && matches!(value, 0 | 1);
    if valid {
        state.lifecycle.observe(value != 0);
    } else {
        state.lifecycle.failed.set(true);
    }
    1
}
unsafe extern "C" fn owner_changed(
    _message: *mut Message,
    data: *mut libc::c_void,
    _error: *mut libc::c_void,
) -> i32 {
    let state = unsafe { &*data.cast::<Mailbox>() };
    state.lifecycle.failed.set(true);
    1
}

/// Delay inhibitor retained only while the broker has workers to quiesce.
/// Dropping this value releases its sole local duplicate; it conveys no input proof.
pub struct Delay {
    _fd: OwnedFd,
}

/// Owner and worker teardown precede release of the delay descriptor.
pub struct Inhibited<O, W> {
    owner: O,
    worker: W,
    _delay: Delay,
}
impl<O, W> Inhibited<O, W> {
    pub fn new(owner: O, worker: W, delay: Delay) -> Self {
        Self {
            owner,
            worker,
            _delay: delay,
        }
    }
    pub fn parts(&mut self) -> (&mut O, &mut W) {
        (&mut self.owner, &mut self.worker)
    }
}
pub struct Watch {
    slots: Vec<NonNull<Slot>>,
    bus: Connection,
    state: Box<Mailbox>,
    fd: Option<OwnedFd>,
    _library: crate::session::Library,
}
impl Watch {
    pub fn open(library: &Path) -> io::Result<Self> {
        Self::open_inner(crate::session::Library::open(library)?, SYSTEM_BUS, 0)
    }
    fn open_inner(library: crate::session::Library, address: &CStr, uid: u32) -> io::Result<Self> {
        let api = Api::load(&library)?;
        let bus = Connection::open(api, address)?;
        let owner = bus.string(c"GetNameOwner", LOGIN)?;
        if !owner.to_bytes().starts_with(b":") || bus.uid(&owner)? != uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "sleep provider has no authorized unique bus owner",
            ));
        }
        let state = Box::new(Mailbox {
            api,
            owner,
            lifecycle: Lifecycle::default(),
        });
        let mut watch = Self {
            slots: Vec::new(),
            bus,
            state,
            fd: None,
            _library: library,
        };
        let rule=CString::new(format!("type='signal',sender='{}',path='/org/freedesktop/login1',interface='org.freedesktop.login1.Manager',member='PrepareForSleep'",watch.state.owner.to_str().map_err(|_|invalid("invalid provider bus name"))?)).unwrap();
        watch.subscribe(&rule, prepare)?;
        watch.subscribe(LOGIN_OWNER, owner_changed)?;
        if watch.bus.string(c"GetNameOwner", LOGIN)? != watch.state.owner
            || watch.bus.uid(&watch.state.owner)? != uid
        {
            return Err(invalid("sleep provider changed during subscription"));
        }
        let events = watch.state.lifecycle.events.get();
        let preparing = watch.property(c"PreparingForSleep", b'b')?;
        if !matches!(preparing, 0 | 1) {
            return Err(invalid("invalid PreparingForSleep value"));
        }
        watch.step()?;
        let mut value = watch.state.lifecycle.value.get();
        if preparing != 0 {
            value.generation = value
                .generation
                .checked_add(1)
                .ok_or_else(|| invalid("sleep generation exhausted"))?;
        }
        if watch.state.lifecycle.events.get() == events {
            value.preparing = preparing != 0;
        }
        watch.state.lifecycle.value.set(value);
        watch.fd = Some(pinned(&watch.bus)?);
        Ok(watch)
    }
    fn subscribe(&mut self, rule: &CStr, callback: Callback) -> io::Result<()> {
        let data = (&raw const *self.state).cast_mut().cast();
        self.slots.push(add_match(&self.bus, rule, callback, data)?);
        Ok(())
    }
    fn property(&self, name: &CStr, kind: u8) -> io::Result<i32> {
        let mut value = 0i32;
        checked(unsafe {
            (self.bus.api.property)(
                self.bus.ptr.as_ptr(),
                self.state.owner.as_ptr(),
                OBJECT.as_ptr(),
                MANAGER.as_ptr(),
                name.as_ptr(),
                std::ptr::null_mut(),
                kind as libc::c_char,
                (&mut value as *mut i32).cast(),
            )
        })?;
        Ok(value)
    }
    pub fn step(&mut self) -> io::Result<Snapshot> {
        if self.state.lifecycle.failed.get() {
            return Err(invalid("sleep provider is permanently retired"));
        }
        for _ in 0..64 {
            let result = checked(unsafe {
                (self.bus.api.process)(self.bus.ptr.as_ptr(), std::ptr::null_mut())
            })
            .inspect_err(|_| self.state.lifecycle.failed.set(true))?;
            if self.state.lifecycle.failed.get() {
                return Err(invalid(
                    "sleep provider retired or sent invalid lifecycle data",
                ));
            }
            if result == 0 {
                return Ok(self.state.lifecycle.value.get());
            }
        }
        self.state.lifecycle.failed.set(true);
        Err(invalid("sleep notification queue exceeds work bound"))
    }
    pub fn events(&self) -> io::Result<i16> {
        Ok(
            checked(unsafe { (self.bus.api.events)(self.bus.ptr.as_ptr()) })
                .inspect_err(|_| self.state.lifecycle.failed.set(true))? as i16,
        )
    }
    pub fn wait(&mut self, maximum: Duration) -> io::Result<()> {
        let result = unsafe {
            (self.bus.api.wait)(
                self.bus.ptr.as_ptr(),
                maximum.as_micros().min(1_000_000) as u64,
            )
        };
        if result != -libc::EINTR {
            checked(result).inspect_err(|_| self.state.lifecycle.failed.set(true))?;
        }
        Ok(())
    }
    pub fn delay(&mut self) -> io::Result<Delay> {
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only the root broker may acquire a sleep delay",
            ));
        }
        self.delay_inner()
    }
    fn delay_inner(&mut self) -> io::Result<Delay> {
        if self.step()?.preparing {
            return Err(invalid("cannot arm active input while sleep is preparing"));
        }
        let mut reply = std::ptr::null_mut();
        let result = unsafe {
            (self.bus.api.call)(
                self.bus.ptr.as_ptr(),
                self.state.owner.as_ptr(),
                OBJECT.as_ptr(),
                MANAGER.as_ptr(),
                c"Inhibit".as_ptr(),
                std::ptr::null_mut(),
                &mut reply,
                c"ssss".as_ptr(),
                c"sleep".as_ptr(),
                c"Prek".as_ptr(),
                c"Quiesce input workers before sleep".as_ptr(),
                c"delay".as_ptr(),
            )
        };
        let reply = Reply::result(result, reply, self.bus.api)
            .inspect_err(|_| self.state.lifecycle.failed.set(true))?;
        let mut fd = -1i32;
        reply.basic(c"h", b'h', (&mut fd as *mut i32).cast())?;
        // SAFETY: duplicate the reply-owned inhibitor without inheriting it at exec.
        let owned = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if owned < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Delay {
            _fd: unsafe { OwnedFd::from_raw_fd(owned) },
        })
    }
}
impl AsFd for Watch {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_ref().unwrap().as_fd()
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        for slot in self.slots.drain(..) {
            unsafe {
                (self.bus.api.slot_unref)(slot.as_ptr());
            }
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod native_probe;
