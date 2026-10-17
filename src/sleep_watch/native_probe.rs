use super::*;
use std::{os::fd::AsRawFd, time::Instant};

fn library() -> std::path::PathBuf {
    std::env::var_os("PREK_SLEEP_LOGIND_LIBRARY")
        .map(Into::into)
        .expect("use the immutable root sleep-watch probe wrapper")
}
#[test]
#[ignore = "requires explicit immutable policy library; no system bus access"]
fn library_preflight() {
    let library = crate::session::Library::open(&library()).unwrap();
    Api::load(&library).unwrap();
    println!("sleep-watch protected library ABI passed; no bus connection or inhibitor acquired");
}
pub(super) fn own_inhibitors(watch: &mut Watch) -> io::Result<usize> {
    watch.step()?;
    type Enter = unsafe extern "C" fn(*mut Message, libc::c_char, *const libc::c_char) -> i32;
    type Exit = unsafe extern "C" fn(*mut Message) -> i32;
    let enter: Enter =
        unsafe { std::mem::transmute(watch._library.symbol(c"sd_bus_message_enter_container")?) };
    let exit: Exit =
        unsafe { std::mem::transmute(watch._library.symbol(c"sd_bus_message_exit_container")?) };
    let mut message = std::ptr::null_mut();
    let result = unsafe {
        (watch.bus.api.call)(
            watch.bus.ptr.as_ptr(),
            watch.state.owner.as_ptr(),
            OBJECT.as_ptr(),
            MANAGER.as_ptr(),
            c"ListInhibitors".as_ptr(),
            std::ptr::null_mut(),
            &mut message,
            c"".as_ptr(),
        )
    };
    let reply = Reply::result(result, message, watch.bus.api)?;
    let message = reply.ptr.as_ptr();
    if unsafe { (watch.bus.api.signature)(message, c"a(ssssuu)".as_ptr()) } != 1
        || checked(unsafe { enter(message, b'a' as libc::c_char, c"(ssssuu)".as_ptr()) })? != 1
    {
        return Err(invalid("invalid inhibitor inventory"));
    }
    let mut count = 0;
    for _ in 0..4096 {
        let entry = checked(unsafe { enter(message, b'r' as libc::c_char, c"ssssuu".as_ptr()) })?;
        if entry == 0 {
            checked(unsafe { exit(message) })?;
            return Ok(count);
        }
        let mut fields = [std::ptr::null::<libc::c_char>(); 4];
        for field in &mut fields {
            if checked(unsafe {
                (watch.bus.api.read)(
                    message,
                    b's' as libc::c_char,
                    (field as *mut *const libc::c_char).cast(),
                )
            })? != 1
                || field.is_null()
            {
                return Err(invalid("invalid inhibitor text"));
            }
        }
        let mut uid = 0u32;
        let mut pid = 0u32;
        for value in [&mut uid, &mut pid] {
            if checked(unsafe {
                (watch.bus.api.read)(message, b'u' as libc::c_char, (value as *mut u32).cast())
            })? != 1
            {
                return Err(invalid("invalid inhibitor owner"));
            }
        }
        if pid == std::process::id() && uid == unsafe { libc::geteuid() } {
            let expected = [
                c"sleep",
                c"Prek",
                c"Quiesce input workers before sleep",
                c"delay",
            ];
            if !fields
                .into_iter()
                .zip(expected)
                .all(|(actual, expected)| unsafe { CStr::from_ptr(actual) } == expected)
            {
                return Err(invalid("unexpected owned inhibitor"));
            }
            count += 1;
        }
        checked(unsafe { exit(message) })?;
    }
    Err(invalid("inhibitor inventory exceeds probe bound"))
}
#[test]
#[ignore = "user-launched root delay-only probe; never requests sleep or opens input"]
fn root_delay_preflight() {
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "run the root probe script yourself"
    );
    let mut watch = Watch::open(&library()).unwrap();
    assert!(
        !watch.step().unwrap().preparing,
        "sleep is already preparing; probe refuses late acquisition"
    );
    assert_eq!(own_inhibitors(&mut watch).unwrap(), 0);
    let delay = watch.delay().unwrap();
    assert_ne!(
        unsafe { libc::fcntl(delay._fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    assert_eq!(own_inhibitors(&mut watch).unwrap(), 1);
    drop(delay);
    let until = Instant::now() + Duration::from_secs(2);
    while own_inhibitors(&mut watch).unwrap() != 0 {
        assert!(
            Instant::now() < until,
            "owned delay inhibitor did not disappear"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    println!(
        "PREK_SLEEP_RECEIPT={{\"status\":\"passed\",\"actual_system_bus\":true,\"logind_uid\":0,\"delay_acquired_and_removed\":true,\"input_devices_opened\":0,\"suspend_requested\":false,\"services_stopped\":false}}"
    );
}
