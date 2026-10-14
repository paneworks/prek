#![cfg(target_os = "linux")]

use nix::libc;
use prek::{
    ipc::{Channel, Endpoint, Listener, monotonic},
    protocol::{self, Epoch, Fence, Packet, Request},
};
use std::{
    fs,
    io::{self, Read},
    mem::size_of,
    os::{
        fd::{AsFd, AsRawFd},
        unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    },
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

fn request() -> Request {
    Request::Heartbeat {
        fence: Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 1,
        },
    }
}
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "prek-protocol-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn protocol_codec_checks_schema_sequence_time_and_size() {
    let packet = Packet {
        version: 1,
        sequence: 1,
        at: Duration::from_secs(2),
        message: request(),
    };
    let bytes = protocol::encode(&packet).unwrap();
    let decoded: Packet<Request> = protocol::decode(&bytes).unwrap();
    decoded.message.validate().unwrap();
    decoded
        .validate_header(1, Duration::from_millis(2999))
        .unwrap();
    assert!(decoded.validate_header(2, Duration::from_secs(2)).is_err());
    assert!(decoded.validate_header(1, Duration::from_secs(1)).is_err());
    assert!(decoded.validate_header(1, Duration::from_secs(3)).is_err());
    let mut unknown: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    unknown["claimed_uid"] = serde_json::json!(1000);
    assert!(protocol::decode::<Request>(&serde_json::to_vec(&unknown).unwrap()).is_err());
    unknown.as_object_mut().unwrap().remove("claimed_uid");
    unknown["message"]["session_name"] = serde_json::json!("1");
    assert!(protocol::decode::<Request>(&serde_json::to_vec(&unknown).unwrap()).is_err());
    assert!(protocol::decode::<Request>(&vec![b' '; protocol::MAX_PACKET + 1]).is_err());
    let large = Packet {
        version: 1,
        sequence: 1,
        at: Duration::ZERO,
        message: "x".repeat(protocol::MAX_PACKET),
    };
    assert!(protocol::encode(&large).is_err());
    assert!(
        Fence {
            broker: Epoch([0, 0]),
            configuration: 1,
            session: 1
        }
        .validate()
        .is_err()
    );
}

#[test]
fn frozen_geometry_survives_json_transport_without_float_changes() {
    use prek::gestures::{Output, Point, Transform};
    let original = Output {
        name: "fractional".into(),
        pixel_width: 1920.0,
        pixel_height: 1080.0,
        scale: 1.25,
        origin: Point {
            x: 0.9951847266721969,
            y: -0.09801714032956059,
        },
        transform: Transform::Normal,
    };
    let packet = Packet {
        version: protocol::VERSION,
        sequence: 1,
        at: Duration::ZERO,
        message: original.clone(),
    };
    let (mut sender, mut receiver) = Channel::pair().unwrap();
    sender.send(&original, monotonic().unwrap()).unwrap();
    let received = receiver.receive::<Output>().unwrap().unwrap().message;
    assert_eq!(
        received, original,
        "a frozen geometry must not change across IPC"
    );
    let decoded = protocol::decode::<Output>(&protocol::encode(&packet).unwrap())
        .unwrap()
        .message;
    assert_eq!(decoded.origin.x.to_bits(), original.origin.x.to_bits());
    assert_eq!(decoded.origin.y.to_bits(), original.origin.y.to_bits());
}

#[test]
fn reload_request_cannot_supply_a_path_configuration_or_claimed_privilege() {
    Request::Reload {}.validate().unwrap();
    assert!(Request::Reload {}.fence().is_none());
    for field in ["path", "config", "claimed_uid"] {
        let mut request = serde_json::json!({"kind": "reload"});
        request[field] = serde_json::json!("forbidden");
        assert!(
            serde_json::from_value::<Request>(request).is_err(),
            "reload accepted {field}"
        );
    }
}

#[test]
fn connected_packets_are_ordered_and_channel_failure_is_permanent() {
    let (mut left, mut right) = Channel::pair().unwrap();
    assert!(right.receive::<Request>().unwrap().is_none());
    for sequence in 1..=3 {
        assert!(left.send(&request(), monotonic().unwrap()).unwrap());
        assert_eq!(
            right.receive::<Request>().unwrap().unwrap().sequence,
            sequence
        );
    }
    let duplicate = protocol::encode(&Packet {
        version: 1,
        sequence: 3,
        at: monotonic().unwrap(),
        message: request(),
    })
    .unwrap();
    assert_eq!(
        unsafe {
            libc::send(
                left.as_fd().as_raw_fd(),
                duplicate.as_ptr().cast(),
                duplicate.len(),
                libc::MSG_NOSIGNAL,
            )
        },
        duplicate.len() as isize
    );
    assert!(right.receive::<Request>().is_err());
    assert!(right.receive::<Request>().is_err());
}

#[test]
fn backpressure_never_blocks_or_advances_an_unsent_sequence() {
    let (mut left, mut right) = Channel::pair().unwrap();
    let started = Instant::now();
    let mut accepted = 0;
    loop {
        if !left.send(&request(), monotonic().unwrap()).unwrap() {
            break;
        }
        accepted += 1;
        assert!(accepted < 10000, "socket never applied backpressure");
    }
    assert!(started.elapsed() < Duration::from_secs(2));
    for sequence in 1..=accepted {
        assert_eq!(
            right.receive::<Request>().unwrap().unwrap().sequence,
            sequence
        );
    }
    assert!(left.send(&request(), monotonic().unwrap()).unwrap());
    assert_eq!(
        right.receive::<Request>().unwrap().unwrap().sequence,
        accepted + 1
    );
}

#[test]
fn oversized_packets_fail_before_decoding() {
    let (left, mut right) = Channel::pair().unwrap();
    let bytes = vec![b'x'; protocol::MAX_PACKET + 1];
    assert_eq!(
        unsafe {
            libc::send(
                left.as_fd().as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_NOSIGNAL,
            )
        },
        bytes.len() as isize
    );
    assert!(right.receive::<Request>().is_err());
}

#[test]
fn forbidden_and_truncated_rights_are_closed_before_rejecting_the_packet() {
    for (count, oversized) in [(1, false), (253, false), (253, true)] {
        let (left, mut right) = Channel::pair().unwrap();
        let (mut witness, transferred) = UnixStream::pair().unwrap();
        witness
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let bytes = if oversized {
            vec![b'x'; protocol::MAX_PACKET + 1]
        } else {
            b"{}".to_vec()
        };
        let mut vector = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let mut control = [0usize; 256];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen =
            unsafe { libc::CMSG_SPACE((count * size_of::<i32>()) as u32) } as usize;
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN((count * size_of::<i32>()) as u32) as usize;
            for index in 0..count {
                libc::CMSG_DATA(header)
                    .cast::<i32>()
                    .add(index)
                    .write_unaligned(transferred.as_raw_fd());
            }
            assert_eq!(
                libc::sendmsg(left.as_fd().as_raw_fd(), &message, libc::MSG_NOSIGNAL),
                bytes.len() as isize
            );
        }
        drop(transferred);
        assert!(right.receive::<Request>().is_err());
        // EOF proves every duplicate was closed, including on truncated data;
        // counting
        // process descriptors would race the other concurrent test fixtures.
        assert_eq!(
            witness.read(&mut [0; 1]).unwrap_or_else(|error| panic!(
                "{count} transferred descriptors were not fully closed: {error}"
            )),
            0
        );
    }
}

#[test]
fn runtime_lease_and_inode_checks_preserve_other_lifetimes() {
    let directory = Directory::new();
    let endpoint = Endpoint::bind(&directory.0, false).unwrap();
    assert!(Endpoint::bind(&directory.0, false).is_err());
    let mut client = Channel::connect(&directory.0.join("control.sock")).unwrap();
    let mut server = endpoint.accept().unwrap().unwrap();
    assert!(client.send(&request(), monotonic().unwrap()).unwrap());
    assert!(server.receive::<Request>().unwrap().is_some());
    fs::rename(
        directory.0.join("control.sock"),
        directory.0.join("old.sock"),
    )
    .unwrap();
    let replacement = Listener::bind(&directory.0.join("control.sock")).unwrap();
    drop(endpoint);
    assert!(directory.0.join("control.sock").exists());
    drop(replacement);
    // Another concurrent process test can briefly inherit the CLOEXEC lease
    // between fork and exec. Its retained kernel lock must be respected.
    let deadline = Instant::now() + Duration::from_secs(1);
    let recovered = loop {
        match Endpoint::bind(&directory.0, false) {
            Ok(endpoint) => break endpoint,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("endpoint recovery: {error}"),
        }
    };
    drop(recovered);
    assert!(!directory.0.join("control.sock").exists());
    fs::write(directory.0.join("control.sock"), "preserve this file").unwrap();
    assert!(Endpoint::bind(&directory.0, false).is_err());
    assert_eq!(
        fs::read_to_string(directory.0.join("control.sock")).unwrap(),
        "preserve this file"
    );
}

#[test]
fn packets_from_a_different_process_using_an_inherited_socket_are_rejected() {
    let (left, mut right) = Channel::pair().unwrap();
    let fd = left.as_fd().as_raw_fd();
    let binary = std::env::current_exe().unwrap();
    let mut command = if let Some(runner) = std::env::var_os("PREK_TEST_RUNNER") {
        let mut command = Command::new(runner);
        command.arg(&binary);
        command
    } else {
        Command::new(binary)
    };
    command
        .args(["--ignored", "--exact", "protocol_packet_writer_fixture"])
        .env("PREK_PROTOCOL_WRITER_FD", fd.to_string());
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    assert!(command.status().unwrap().success());
    assert_eq!(
        right.receive::<Request>().unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
#[ignore = "subprocess fixture; requires an explicitly inherited test socket"]
fn protocol_packet_writer_fixture() {
    let fd: i32 = std::env::var("PREK_PROTOCOL_WRITER_FD")
        .unwrap()
        .parse()
        .unwrap();
    let bytes = protocol::encode(&Packet {
        version: 1,
        sequence: 1,
        at: monotonic().unwrap(),
        message: request(),
    })
    .unwrap();
    assert_eq!(
        unsafe { libc::send(fd, bytes.as_ptr().cast(), bytes.len(), libc::MSG_NOSIGNAL) },
        bytes.len() as isize
    );
}
