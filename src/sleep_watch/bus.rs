use crate::session::Library;
use nix::libc;
use std::{
    ffi::{CStr, CString},
    io,
    ptr::NonNull,
};

#[repr(C)]
pub(super) struct Bus {
    _opaque: [u8; 0],
}
#[repr(C)]
pub(super) struct Message {
    _opaque: [u8; 0],
}
#[repr(C)]
pub(super) struct Slot {
    _opaque: [u8; 0],
}
pub(super) type Callback =
    unsafe extern "C" fn(*mut Message, *mut libc::c_void, *mut libc::c_void) -> i32;
macro_rules! api {
    ($($field:ident: $ty:ty = $name:literal),+ $(,)?) => {
        #[derive(Clone,Copy)]
        pub(super) struct Api { $(pub $field:$ty),+ }
        impl Api {
            pub fn load(library:&Library)->io::Result<Self> {
                Ok(Self { $($field: {
                    let name=CStr::from_bytes_with_nul(concat!($name,"\0").as_bytes()).unwrap();
                    // SAFETY: the retained policy library implements this exact sd-bus ABI.
                    unsafe { std::mem::transmute::<*mut libc::c_void,$ty>(library.symbol(name)?) }
                }),+ })
            }
        }
    }
}
api! {
    new:unsafe extern "C" fn(*mut *mut Bus)->i32 = "sd_bus_new",
    address:unsafe extern "C" fn(*mut Bus,*const libc::c_char)->i32 = "sd_bus_set_address",
    client:unsafe extern "C" fn(*mut Bus,i32)->i32 = "sd_bus_set_bus_client",
    interactive:unsafe extern "C" fn(*mut Bus,i32)->i32 = "sd_bus_set_allow_interactive_authorization",
    exit_on_disconnect:unsafe extern "C" fn(*mut Bus,i32)->i32 = "sd_bus_set_exit_on_disconnect",
    call_timeout:unsafe extern "C" fn(*mut Bus,u64)->i32 = "sd_bus_set_method_call_timeout",
    start:unsafe extern "C" fn(*mut Bus)->i32 = "sd_bus_start",
    close:unsafe extern "C" fn(*mut Bus)->*mut Bus = "sd_bus_close_unref",
    fd:unsafe extern "C" fn(*mut Bus)->i32 = "sd_bus_get_fd",
    events:unsafe extern "C" fn(*mut Bus)->i32 = "sd_bus_get_events",
    process:unsafe extern "C" fn(*mut Bus,*mut *mut Message)->i32 = "sd_bus_process",
    wait:unsafe extern "C" fn(*mut Bus,u64)->i32 = "sd_bus_wait",
    add_match:unsafe extern "C" fn(*mut Bus,*mut *mut Slot,*const libc::c_char,Option<Callback>,*mut libc::c_void)->i32 = "sd_bus_add_match",
    slot_unref:unsafe extern "C" fn(*mut Slot)->*mut Slot = "sd_bus_slot_unref",
    message_unref:unsafe extern "C" fn(*mut Message)->*mut Message = "sd_bus_message_unref",
    signature:unsafe extern "C" fn(*mut Message,*const libc::c_char)->i32 = "sd_bus_message_has_signature",
    sender:unsafe extern "C" fn(*mut Message)->*const libc::c_char = "sd_bus_message_get_sender",
    read:unsafe extern "C" fn(*mut Message,libc::c_char,*mut libc::c_void)->i32 = "sd_bus_message_read_basic",
    end:unsafe extern "C" fn(*mut Message,i32)->i32 = "sd_bus_message_at_end",
    property:unsafe extern "C" fn(*mut Bus,*const libc::c_char,*const libc::c_char,*const libc::c_char,*const libc::c_char,*mut libc::c_void,libc::c_char,*mut libc::c_void)->i32 = "sd_bus_get_property_trivial",
    call:unsafe extern "C" fn(*mut Bus,*const libc::c_char,*const libc::c_char,*const libc::c_char,*const libc::c_char,*mut libc::c_void,*mut *mut Message,*const libc::c_char,...)->i32 = "sd_bus_call_method",
}
pub(super) fn checked(result: i32) -> io::Result<i32> {
    if result < 0 {
        Err(io::Error::from_raw_os_error(-result))
    } else {
        Ok(result)
    }
}
pub(super) fn invalid(text: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text)
}
pub(super) struct Connection {
    pub ptr: NonNull<Bus>,
    pub api: Api,
}
impl Connection {
    pub fn open(api: Api, address: &CStr) -> io::Result<Self> {
        let mut ptr = std::ptr::null_mut();
        checked(unsafe { (api.new)(&mut ptr) })?;
        let value = Self {
            ptr: NonNull::new(ptr).ok_or_else(|| invalid("sd-bus returned no connection"))?,
            api,
        };
        for result in unsafe {
            [
                (api.address)(ptr, address.as_ptr()),
                (api.client)(ptr, 1),
                (api.interactive)(ptr, 0),
                (api.exit_on_disconnect)(ptr, 0),
                (api.call_timeout)(ptr, 1_000_000),
            ]
        } {
            checked(result)?;
        }
        checked(unsafe { (api.start)(ptr) })?;
        Ok(value)
    }
    pub fn string(&self, method: &CStr, input: &CStr) -> io::Result<CString> {
        let mut reply = std::ptr::null_mut();
        let result = unsafe {
            (self.api.call)(
                self.ptr.as_ptr(),
                c"org.freedesktop.DBus".as_ptr(),
                c"/org/freedesktop/DBus".as_ptr(),
                c"org.freedesktop.DBus".as_ptr(),
                method.as_ptr(),
                std::ptr::null_mut(),
                &mut reply,
                c"s".as_ptr(),
                input.as_ptr(),
            )
        };
        let reply = Reply::result(result, reply, self.api)?;
        let mut text: *const libc::c_char = std::ptr::null();
        reply.basic(c"s", b's', (&mut text as *mut *const libc::c_char).cast())?;
        if text.is_null() {
            return Err(invalid("missing bus name"));
        }
        let text = unsafe { CStr::from_ptr(text) };
        if text.to_bytes().len() > 255 {
            return Err(invalid("bus name exceeds bounds"));
        }
        Ok(text.to_owned())
    }
    /// Logind's object path for one session id.
    pub fn session_path(&self, session: &CStr) -> io::Result<CString> {
        let mut reply = std::ptr::null_mut();
        let result = unsafe {
            (self.api.call)(
                self.ptr.as_ptr(),
                c"org.freedesktop.login1".as_ptr(),
                c"/org/freedesktop/login1".as_ptr(),
                c"org.freedesktop.login1.Manager".as_ptr(),
                c"GetSession".as_ptr(),
                std::ptr::null_mut(),
                &mut reply,
                c"s".as_ptr(),
                session.as_ptr(),
            )
        };
        let reply = Reply::result(result, reply, self.api)?;
        let mut path: *const libc::c_char = std::ptr::null();
        reply.basic(c"o", b'o', (&mut path as *mut *const libc::c_char).cast())?;
        if path.is_null() {
            return Err(invalid("missing session object path"));
        }
        let path = unsafe { CStr::from_ptr(path) };
        if path.to_bytes().len() > 255 {
            return Err(invalid("session object path exceeds bounds"));
        }
        Ok(path.to_owned())
    }
    /// Logind's `LockedHint` for one session object path.
    pub fn locked_hint(&self, path: &CStr) -> io::Result<bool> {
        let mut value: i32 = 0;
        checked(unsafe {
            (self.api.property)(
                self.ptr.as_ptr(),
                c"org.freedesktop.login1".as_ptr(),
                path.as_ptr(),
                c"org.freedesktop.login1.Session".as_ptr(),
                c"LockedHint".as_ptr(),
                std::ptr::null_mut(),
                b'b' as libc::c_char,
                (&mut value as *mut i32).cast(),
            )
        })?;
        Ok(value != 0)
    }
    pub fn uid(&self, name: &CStr) -> io::Result<u32> {
        self.number(c"GetConnectionUnixUser", name)
    }
    pub fn pid(&self, name: &CStr) -> io::Result<u32> {
        self.number(c"GetConnectionUnixProcessID", name)
    }
    fn number(&self, method: &CStr, name: &CStr) -> io::Result<u32> {
        let mut reply = std::ptr::null_mut();
        let result = unsafe {
            (self.api.call)(
                self.ptr.as_ptr(),
                c"org.freedesktop.DBus".as_ptr(),
                c"/org/freedesktop/DBus".as_ptr(),
                c"org.freedesktop.DBus".as_ptr(),
                method.as_ptr(),
                std::ptr::null_mut(),
                &mut reply,
                c"s".as_ptr(),
                name.as_ptr(),
            )
        };
        let reply = Reply::result(result, reply, self.api)?;
        let mut value = 0u32;
        reply.basic(c"u", b'u', (&mut value as *mut u32).cast())?;
        Ok(value)
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        unsafe {
            (self.api.close)(self.ptr.as_ptr());
        }
    }
}
pub(super) struct Reply {
    pub(super) ptr: NonNull<Message>,
    api: Api,
}
impl Reply {
    pub fn result(result: i32, ptr: *mut Message, api: Api) -> io::Result<Self> {
        let reply = NonNull::new(ptr).map(|ptr| Self { ptr, api });
        checked(result)?;
        reply.ok_or_else(|| invalid("missing system bus reply"))
    }
    pub fn basic(&self, signature: &CStr, kind: u8, output: *mut libc::c_void) -> io::Result<()> {
        if unsafe { (self.api.signature)(self.ptr.as_ptr(), signature.as_ptr()) } != 1 {
            return Err(invalid("unexpected system bus reply signature"));
        }
        if checked(unsafe { (self.api.read)(self.ptr.as_ptr(), kind as libc::c_char, output) })?
            != 1
            || checked(unsafe { (self.api.end)(self.ptr.as_ptr(), 1) })? != 1
        {
            return Err(invalid("incomplete system bus reply"));
        }
        Ok(())
    }
}
impl Drop for Reply {
    fn drop(&mut self) {
        unsafe {
            (self.api.message_unref)(self.ptr.as_ptr());
        }
    }
}
