use super::*;
use std::{os::unix::net::UnixStream, path::PathBuf};

fn select<T>(
    paths: &[PathBuf],
    mut open: impl FnMut(&Path) -> io::Result<T>,
) -> io::Result<(PathBuf, T)> {
    if paths.is_empty() || paths.len() > 8 || paths.iter().any(|path| !path.is_absolute()) {
        return Err(invalid(
            "native probe requires one to eight absolute library paths",
        ));
    }
    let mut failures = Vec::new();
    for path in paths {
        match open(path) {
            Ok(library) => return Ok((path.clone(), library)),
            Err(error) => failures.push(format!("{}: {error}", path.display())),
        }
    }
    Err(denied(&format!(
        "no trusted usable logind library: {}",
        failures.join("; ")
    )))
}

pub(crate) fn load() -> io::Result<(PathBuf, Logind)> {
    let paths = std::env::var_os("PREK_NATIVE_LIBRARIES")
        .ok_or_else(|| invalid("missing fixed native probe library policy"))?;
    let paths: Vec<_> = std::env::split_paths(&paths).take(9).collect();
    select(&paths, Logind::open)
}

#[test]
#[ignore = "explicit unprivileged host-library compatibility check"]
fn library_preflight() {
    let (path, _library) = load().unwrap();
    println!(
        "PREK_NATIVE_LIBRARY_PREFLIGHT={}",
        serde_json::json!({
            "version": 1, "library": fs::canonicalize(path).unwrap(), "loaded": true,
            "root_owned": true, "session_checked": false, "handoff_tested": false,
        })
    );
}

#[test]
#[ignore = "explicit unprivileged active-local-login preflight"]
fn session_preflight() {
    let uid = unsafe { libc::geteuid() };
    assert_ne!(
        uid, 0,
        "run the preflight as your regular user, without sudo"
    );
    let (path, library) = load().unwrap();
    let (socket, _other) = UnixStream::pair().unwrap();
    let peer = Peer::from_socket(&socket).unwrap();
    let process = Process::open(std::process::id(), uid).unwrap();
    let facts = library.facts(&peer, &process).unwrap_or_else(|error| {
        panic!(
            "native handoff requires an active local Wayland or TTY login on a seat; \
            SSH/headless Cage alone is insufficient. Run from your logged-in local terminal. \
            No sudo or handoff was attempted. logind: {error}"
        )
    });
    println!(
        "PREK_NATIVE_SESSION_PREFLIGHT={}",
        serde_json::json!({
            "version": 1, "library": fs::canonicalize(path).unwrap(), "uid": facts.uid,
            "session": facts.session, "seat": facts.seat, "type": facts.session_type,
            "active_local": true, "handoff_tested": false,
        })
    );
}

#[test]
#[ignore = "explicit unprivileged display-session controller check"]
fn controller_preflight() {
    let uid = unsafe { libc::geteuid() };
    let pid = std::env::var("PREK_COMPOSITOR_PID")
        .expect("set PREK_COMPOSITOR_PID to the running compositor")
        .parse()
        .unwrap();
    let (path, library) = load().unwrap();
    let compositor = Process::open(pid, uid).unwrap();
    let own = library.process_session(&compositor);
    assert_eq!(
        own.as_ref().err().and_then(io::Error::raw_os_error),
        Some(libc::ENODATA),
        "the compositor already has a login session: {own:?}"
    );
    let cgroup = library.process_cgroup(&compositor).unwrap();
    assert!(user_manager_cgroup(&cgroup, uid), "{cgroup}");
    let session = allocated(|out| unsafe { (library.uid_display)(uid, out) }).unwrap();
    let controller = controller(&session).unwrap();
    let name = CString::new(controller.as_str()).unwrap();
    let owner = crate::sleep_watch::connection_pid(&library.library, &name).unwrap();
    assert_eq!(
        owner, compositor.pid,
        "{controller} belongs to another process"
    );
    compositor.check().unwrap();
    println!(
        "PREK_CONTROLLER_PREFLIGHT={}",
        serde_json::json!({
            "version": 1, "library": fs::canonicalize(path).unwrap(), "compositor": pid,
            "cgroup": cgroup, "session": session, "controller": controller,
        })
    );
}

#[test]
fn candidates_are_absolute_nonempty_and_bounded_before_opening() {
    for paths in [
        vec![],
        vec![PathBuf::from("libsystemd.so.0")],
        vec![PathBuf::new()],
        vec![PathBuf::from("/library"); 9],
        vec![PathBuf::from("/library"), PathBuf::from("relative")],
    ] {
        assert!(
            select::<()>(&paths, |_| panic!("invalid policy reached library loading")).is_err()
        );
    }
}

#[test]
fn unusable_candidates_fall_through_without_reopening_the_selected_library() {
    let paths = ["/untrusted", "/incompatible", "/host", "/unused"].map(PathBuf::from);
    let mut attempted = Vec::new();
    let (path, value) = select(&paths, |path| {
        attempted.push(path.to_owned());
        match path.to_str().unwrap() {
            "/host" => Ok(42),
            "/incompatible" => Err(io::Error::new(io::ErrorKind::Unsupported, "ABI mismatch")),
            _ => Err(denied("not root-owned")),
        }
    })
    .unwrap();
    assert_eq!(path, paths[2]);
    assert_eq!(value, 42);
    assert_eq!(attempted, paths[..3]);
}

#[test]
fn rejected_candidates_keep_their_paths_and_failure_reasons() {
    let paths = [PathBuf::from("/first"), PathBuf::from("/second")];
    let error = select::<()>(&paths, |path| {
        Err(denied(&format!("rejected {}", path.display())))
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("/first: rejected /first"));
    assert!(error.to_string().contains("/second: rejected /second"));
}

#[test]
fn actual_user_owned_library_is_rejected_before_loading() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("prek-logind-{}-{nonce}", std::process::id()));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    let result = Logind::open(&path);
    drop(file);
    fs::remove_file(&path).unwrap();
    let error = result.err().expect("user-owned library must not load");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("protected root-owned file"));
}
