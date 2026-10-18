use super::*;

const FD: &str = "PREK_MANAGED_FIXTURE_FD";

pub(super) fn spawn() -> io::Result<(Supervisor, Channel, Channel)> {
    let (mut parent, child) = Channel::pair()?;
    let descriptor = child.into_fd();
    let raw = descriptor.as_raw_fd();
    let mut command = ProcessCommand::new(std::env::current_exe()?);
    command.arg("--worker").env(FD, raw.to_string());
    // SAFETY: only the retained fixture socket's close-on-exec flag changes.
    unsafe {
        command.pre_exec(move || {
            if nix::libc::fcntl(descriptor.as_raw_fd(), nix::libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let (supervisor, control) = Supervisor::spawn_controlled(command)?;
    parent.expect_worker(supervisor.worker_pid());
    Ok((supervisor, control, parent))
}

pub(super) fn child(health: &Heartbeat, control: &Channel) -> io::Result<Channel> {
    let raw: i32 = std::env::var(FD)
        .map_err(invalid)?
        .parse()
        .map_err(invalid)?;
    if raw < 3 || raw == health.as_fd().as_raw_fd() || raw == control.as_fd().as_raw_fd() {
        return Err(invalid("invalid managed fixture descriptor"));
    }
    let mut kind: nix::libc::c_int = 0;
    let mut size = std::mem::size_of_val(&kind) as nix::libc::socklen_t;
    // SAFETY: initialized scalar output and exact socket-option length.
    if unsafe {
        nix::libc::getsockopt(
            raw,
            nix::libc::SOL_SOCKET,
            nix::libc::SO_TYPE,
            (&mut kind as *mut nix::libc::c_int).cast(),
            &mut size,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if kind != nix::libc::SOCK_SEQPACKET || size as usize != std::mem::size_of_val(&kind) {
        return Err(invalid("managed fixture channel is not a sequenced socket"));
    }
    // SAFETY: retain the verified fixture socket only in this worker.
    if unsafe { nix::libc::fcntl(raw, nix::libc::F_SETFD, nix::libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the inherited fixture descriptor is checked and claimed once.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut channel = Channel::from_fd(owned)?;
    // SAFETY: read-only parent process identity.
    let parent = unsafe { nix::libc::getppid() } as u32;
    if channel.peer().uid != 0 || channel.peer().pid != parent {
        return Err(invalid(
            "managed fixture peer differs from the owned supervisor",
        ));
    }
    channel.expect_worker(parent);
    Ok(channel)
}

pub(super) fn send<T: Serialize>(channel: &mut Channel, value: &T) -> io::Result<()> {
    if !channel.send(value, monotonic()?)? {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "managed fixture channel is full",
        ));
    }
    Ok(())
}

pub(super) fn live(stream: &mut Stream, health: &mut Heartbeat) -> io::Result<()> {
    health.progress()?;
    stream.service()?;
    stream.refresh_keys(monotonic()?)?;
    if stream.seats.values().all(|seat| seat.keys.is_some()) {
        stream.service_keys(Some(health))?;
    }
    let sources: Vec<_> = stream.outputs.keys().cloned().collect();
    for source in sources {
        if stream.seats.contains_key(&source.seat) {
            stream.offer_transport(&source, monotonic()?)?;
        }
    }
    stream.messages.flush(&mut stream.channel)
}

pub(super) fn request(
    channel: &mut Channel,
    stream: &mut Stream,
    health: &mut Heartbeat,
) -> io::Result<Request> {
    let deadline = Instant::now() + WAIT;
    loop {
        live(stream, health)?;
        if let Some(packet) = channel.receive::<Request>()? {
            return Ok(packet.message);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "managed fixture request timed out",
            ));
        }
        thread::sleep(Duration::from_millis(1));
    }
}
