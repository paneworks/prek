#![cfg(target_os = "linux")]
use nix::libc;
use prek::session_launcher::{Ready, run};
use std::{
    fs, io,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
const RETRY_EXIT: u8 = 75;
const MODE: &str = "PREK_TEST_SESSION_LAUNCH_MODE";
const LOG: &str = "PREK_TEST_SESSION_LAUNCH_LOG";
fn command(mode: &str, log: &std::path::Path) -> Command {
    let executable = std::env::current_exe().unwrap();
    let mut command = if let Some(runner) = std::env::var_os("PREK_TEST_RUNNER") {
        let mut command = Command::new(runner);
        command.arg(executable);
        command
    } else {
        Command::new(executable)
    };
    command
        .env(MODE, mode)
        .env(LOG, log)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    command
}
fn lines(path: &std::path::Path) -> Vec<Vec<i32>> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| {
            line.split_whitespace()
                .map(|v| v.parse().unwrap())
                .collect()
        })
        .collect()
}
fn until(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(4);
    while !condition() {
        assert!(Instant::now() < until, "owned process did not complete");
        thread::sleep(Duration::from_millis(5));
    }
}
fn fixture(mode: &str, log: &std::path::Path) {
    use std::io::Write;
    if mode == "controller" {
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let mut owner = command("kill-owner", log).spawn().unwrap();
        until(|| !lines(log).is_empty());
        let pid = lines(log)[0][0];
        struct Adopted(i32);
        impl Drop for Adopted {
            fn drop(&mut self) {
                let mut status = 0;
                if unsafe { libc::waitpid(self.0, &mut status, libc::WNOHANG) } == 0 {
                    unsafe {
                        libc::kill(self.0, libc::SIGKILL);
                        libc::waitpid(self.0, &mut status, 0);
                    }
                }
            }
        }
        let _adopted = Adopted(pid);
        assert_eq!(unsafe { libc::kill(owner.id() as i32, libc::SIGKILL) }, 0);
        assert!(!owner.wait().unwrap().success());
        let mut status = 0;
        until(|| unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid);
        assert!(libc::WIFSIGNALED(status));
        assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
        return;
    }
    if mode == "owner" {
        for (name, expected, count) in [
            ("retry", true, 2),
            ("normal", true, 1),
            ("fatal", false, 1),
            ("invalid-ready", false, 1),
            ("unready", true, 2),
        ] {
            let path = log.with_extension(name);
            let result = run(|| command(name, &path));
            assert_eq!(result.is_ok(), expected, "{name}: {result:?}");
            let entries = lines(&path);
            assert_eq!(entries.len(), count, "{name}");
            for entry in &entries {
                assert_eq!(entry[1], std::process::id() as i32);
                assert_eq!(entry[2], unsafe { libc::getsid(0) });
                assert_eq!(unsafe { libc::kill(entry[0], 0) }, -1);
                assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
            }
            if count == 2 {
                assert_ne!(entries[0][0], entries[1][0]);
            }
            fs::remove_file(path).unwrap();
        }
        return;
    }
    if mode == "stop-owner" || mode == "kill-owner" {
        run(|| command("linger", log)).unwrap();
        assert_eq!(unsafe { libc::kill(lines(log)[0][0], 0) }, -1);
        return;
    }
    let index = lines(log).len();
    let mut output = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .unwrap();
    writeln!(
        output,
        "{} {} {}",
        std::process::id(),
        unsafe { libc::getppid() },
        unsafe { libc::getsid(0) }
    )
    .unwrap();
    let mut ready = Ready::connect().unwrap();
    assert!(prek::ipc::monotonic().is_ok());
    if mode == "unready" && index == 0 {
        thread::sleep(Duration::from_secs(10));
    }
    if mode == "invalid-ready" {
        let fd = std::env::var("PREK_SESSION_READY_FD")
            .unwrap()
            .parse::<i32>()
            .unwrap();
        assert_eq!(unsafe { libc::write(fd, b"X".as_ptr().cast(), 1) }, 1);
        thread::sleep(Duration::from_secs(10));
        return;
    }
    ready.accepted().unwrap();
    assert!(ready.accepted().is_err());
    match mode {
        "retry" if index == 0 => std::process::exit(i32::from(RETRY_EXIT)),
        "fatal" => std::process::exit(1),
        "linger" => thread::sleep(Duration::from_secs(10)),
        "retry" | "normal" | "unready" => {}
        _ => panic!("unknown owned fixture"),
    }
}

fn main() {
    if let Ok(mode) = std::env::var(MODE) {
        fixture(&mode, &PathBuf::from(std::env::var_os(LOG).unwrap()));
        return;
    }
    let root = std::env::temp_dir().join(format!("prek-launch-test-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let path = root.join("children");
    assert!(command("owner", &path).status().unwrap().success());
    let mut owner = command("stop-owner", &path).spawn().unwrap();
    until(|| !lines(&path).is_empty());
    // The launcher alone receives termination; it must reap its own child.
    assert_eq!(unsafe { libc::kill(owner.id() as i32, libc::SIGTERM) }, 0);
    until(|| owner.try_wait().unwrap().is_some());
    assert!(owner.wait().unwrap().success());
    fs::remove_file(&path).unwrap();
    assert!(command("controller", &path).status().unwrap().success());
    fs::remove_file(&path).unwrap();
    fs::remove_dir(root).unwrap();
    println!("session launcher native gates passed; no input devices opened");
}
