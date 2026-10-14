#![cfg(target_os = "linux")]

use prek::hyprland::Client;
use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Server {
    directory: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(reply: &'static str, delay: Duration) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "prek-ipc-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        for path in [
            &directory,
            &directory.join("hypr"),
            &directory.join("hypr/test"),
        ] {
            fs::create_dir_all(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let listener = UnixListener::bind(directory.join("hypr/test/.socket.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                if let Ok((mut stream, _)) = listener.accept() {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut bytes = [0; 128];
                    let length = stream.read(&mut bytes).unwrap_or(0);
                    if length == 0 {
                        continue;
                    }
                    if bytes[..length] == *b"j/version" {
                        let _ = stream.write_all(b"{\"version\":\"0.55.4\"}");
                    } else {
                        thread::sleep(delay);
                        let _ = stream.write_all(reply.as_bytes());
                    }
                } else {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        });
        Self {
            directory,
            stop,
            thread: Some(worker),
        }
    }
    fn client(&self) -> Client {
        Client::open(&self.directory, "test", uid(), &test_executable()).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
        fs::remove_dir_all(&self.directory).unwrap();
    }
}
fn uid() -> u32 {
    unsafe { nix::libc::geteuid() }
}
fn test_executable() -> PathBuf {
    // Under user-mode QEMU, peer credentials identify the emulator process.
    // Test that actual kernel image rather than weakening production checks.
    PathBuf::from(format!("/proc/{}/exe", std::process::id()))
}
const EMPTY: &str = "{\"keyboards\":[],\"mice\":[],\"touch\":[],\"switches\":[],\"tablets\":[]}";

#[test]
fn private_socket_requests_verify_the_exact_peer_and_endpoint() {
    let server = Server::new(EMPTY, Duration::ZERO);
    let client = server.client();
    assert_eq!(client.version().minor, 55);
    assert!(
        client.inventory().unwrap()["touch"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let path = server.directory.join("hypr/test/.socket.sock");
    fs::remove_file(&path).unwrap();
    let replacement = UnixListener::bind(&path).unwrap();
    assert_eq!(
        client.inventory().unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    drop(replacement);
}

#[test]
fn registration_rejects_wrong_executable_and_nonprivate_directories() {
    let server = Server::new(EMPTY, Duration::ZERO);
    assert_eq!(
        Client::open(&server.directory, "test", uid(), Path::new("/proc/1/exe"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    fs::set_permissions(
        server.directory.join("hypr/test"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert_eq!(
        Client::open(&server.directory, "test", uid(), &test_executable())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(Client::open(&server.directory, "../test", uid(), &test_executable()).is_err());
}

#[test]
fn stalled_or_malformed_compositor_replies_fail_within_the_budget() {
    let server = Server::new(EMPTY, Duration::from_millis(500));
    let client = server.client();
    let started = Instant::now();
    assert_eq!(
        client.inventory().unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    assert!(started.elapsed() < Duration::from_millis(400));
    drop(server);
    let server = Server::new("not JSON", Duration::ZERO);
    assert!(server.client().inventory().is_err());
}

#[test]
#[ignore = "requires an explicitly selected real compositor; read-only"]
fn stock_compositor_queries_use_the_verified_live_socket() {
    let runtime = PathBuf::from(std::env::var_os("PREK_HYPR_RUNTIME").unwrap());
    let instance = std::env::var("PREK_HYPR_INSTANCE").unwrap();
    let executable = PathBuf::from(std::env::var_os("PREK_HYPR_EXE").unwrap());
    let uid = std::env::var("PREK_HYPR_UID").unwrap().parse().unwrap();
    let client = Client::open(&runtime, &instance, uid, &executable).unwrap();
    assert!(client.inventory().unwrap().is_object());
    println!(
        "verified stock Hyprland {:?}: {:?}",
        client.version(),
        client.consumer()
    );
}

#[path = "support/kernel.rs"]
mod kernel_fixture;

#[test]
#[ignore = "requires the disposable stock-compositor VM and root-owned runtime registry"]
fn stock_compositor_accepts_virtual_keyboard_and_verifies_its_keymap() {
    use prek::{
        input::EV_KEY,
        kernel::Virtual,
        transport::{Descriptor, KernelIdentity},
    };
    use std::collections::BTreeMap;
    assert_eq!(std::env::var("PREK_COMPOSITOR_VM").as_deref(), Ok("1"));
    let registry = kernel_fixture::authorized();
    let runtime = PathBuf::from(std::env::var_os("PREK_HYPR_RUNTIME").unwrap());
    let instance = std::env::var("PREK_HYPR_INSTANCE").unwrap();
    let executable = PathBuf::from(std::env::var_os("PREK_HYPR_EXE").unwrap());
    let uid = std::env::var("PREK_HYPR_UID").unwrap().parse().unwrap();
    let client = Client::open(&runtime, &instance, uid, &executable).unwrap();
    let mut original = kernel_fixture::fixture(false);
    original.capabilities.insert(EV_KEY, (1..=255).collect());
    let identity = KernelIdentity {
        bus: 3,
        vendor: original.vendor,
        product: original.product,
        version: 1,
    };
    let policy = BTreeMap::from([("kb_layout".into(), "us".into())]);
    let prepared = Descriptor::mirror(&original, identity, None, &BTreeMap::new(), policy).unwrap();
    assert!(
        client
            .device_evidence(&prepared, kernel_fixture::now())
            .unwrap()
            .is_none()
    );
    // Visible only inside this disposable VM. No key events are injected and
    // no originals are grabbed while testing the real compositor inventory.
    let output = Virtual::create(registry.prepare(prepared.clone()).unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let evidence = loop {
        if !output.processed_metadata().unwrap() {
            assert!(
                Instant::now() < deadline,
                "virtual udev metadata did not become ready"
            );
            thread::sleep(Duration::from_millis(25));
            continue;
        }
        if let Some(evidence) = client
            .device_evidence(&prepared, kernel_fixture::now())
            .unwrap()
        {
            break evidence;
        }
        assert!(
            Instant::now() < deadline,
            "stock compositor never accepted the virtual keyboard"
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(evidence.settings["kb_layout"], "us");
    let wrong = Descriptor::mirror(
        &original,
        identity,
        None,
        &BTreeMap::new(),
        BTreeMap::from([("kb_layout".into(), "fr".into())]),
    )
    .unwrap();
    assert!(
        client
            .device_evidence(&wrong, kernel_fixture::now())
            .is_err()
    );
    drop(output);
    loop {
        if client
            .device_evidence(&prepared, kernel_fixture::now())
            .unwrap()
            .is_none()
        {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(25));
    }
}
