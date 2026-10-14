#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::{File, OpenOptions},
    io::{self, BufReader, Write},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use clap::{Parser, Subcommand};

#[cfg(target_os = "linux")]
mod linux;

#[derive(Parser)]
#[command(
    version,
    about = "Inspect Linux input devices and recognize key, button and touch gestures"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Supervise configured observation and authenticated session control.
    /// Input routing and desktop actions remain unavailable.
    Broker {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, default_value = "observe", value_parser = ["observe", "route"])]
        mode: String,
        /// Trusted actual Hyprland executable (the ELF, rather than a wrapper).
        #[arg(long)]
        compositor_executable: PathBuf,
        /// Trusted absolute path to the system libsystemd.so.0.
        #[arg(long)]
        logind_library: PathBuf,
        #[arg(long, default_value = "/run/prek")]
        runtime_dir: PathBuf,
        #[arg(long)]
        json: bool,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=3600))]
        duration: Option<u64>,
    },
    /// Register from this compositor's login session and maintain a private lease.
    Session {
        #[arg(long, default_value = "/run/prek/control.sock")]
        socket: PathBuf,
        /// Defaults to HYPRLAND_INSTANCE_SIGNATURE; the broker verifies it.
        #[arg(long)]
        instance: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=3600))]
        duration: Option<u64>,
    },
    /// Query broker status from a verified compositor login session.
    Status {
        #[arg(long, default_value = "/run/prek/control.sock")]
        socket: PathBuf,
        #[arg(long)]
        instance: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Internal observer worker; requires a bound supervisor health channel.
    #[command(hide = true)]
    ObserveWorker {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        json: bool,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=3600))]
        duration: Option<u64>,
    },
    /// Internal root-only udev metadata helper.
    #[command(hide = true)]
    UdevImport {
        #[arg(long)]
        marker: String,
    },
    /// Evaluate configured rules without grabbing input or executing actions.
    Run {
        #[arg(long)]
        config: PathBuf,
        /// Routing remains unavailable until takeover/recovery gates pass.
        #[arg(long, default_value = "observe", value_parser = ["observe", "route"])]
        mode: String,
        #[arg(long)]
        json: bool,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=3600))]
        duration: Option<u64>,
    },
    /// Check readable hardware/capacity and report missing operational components.
    Doctor {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Validate rule syntax, units, references and ownership conflicts offline.
    Check {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Create a complete declarative preset without overwriting existing files.
    Init {
        #[arg(long)]
        preset: String,
        #[arg(long)]
        output: PathBuf,
        /// Atomically replace an existing configuration.
        #[arg(long)]
        force: bool,
    },
    /// List event devices and report which can be read.
    Devices {
        #[arg(long)]
        json: bool,
    },
    /// Observe selected devices without grabbing or changing their input.
    Watch {
        /// An event device path, preferably a stable /dev/input/by-id/... link.
        #[arg(required = true)]
        devices: Vec<PathBuf>,
        /// Print recognized gestures and cancellations, without raw events.
        #[arg(long)]
        gestures_only: bool,
        #[arg(long)]
        json: bool,
    },
    /// Capture explicitly selected input into a private, versioned recording.
    Record {
        #[arg(long, required = true)]
        device: Vec<PathBuf>,
        #[arg(long)]
        output: PathBuf,
        /// Maximum capture duration in seconds.
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=3600))]
        duration: u64,
    },
    /// Replay offline, without device access or desktop actions.
    Replay {
        path: PathBuf,
        /// Evaluate configured rules instead of generic recognition.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Check expected outcomes; geometry permits only floating-point roundoff.
        #[arg(long)]
        check: bool,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("prek: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
fn run(command: Command) -> io::Result<()> {
    let mut output = io::stdout().lock();
    match command {
        Command::Broker {
            config,
            mode,
            compositor_executable,
            logind_library,
            runtime_dir,
            json,
            duration,
        } => {
            if mode != "observe" {
                return Err(io::Error::other(
                    "broker routing is unavailable: takeover and adapter gates are pending",
                ));
            }
            run_broker(
                &config,
                &runtime_dir,
                &compositor_executable,
                &logind_library,
                json,
                duration,
            )
        }
        Command::Session {
            socket,
            instance,
            json,
            duration,
        } => run_session(&socket, instance, json, duration, false, &mut output),
        Command::Status {
            socket,
            instance,
            json,
        } => run_session(&socket, instance, json, None, true, &mut output),
        Command::UdevImport { marker } => prek::kernel::import_metadata(&marker, &mut output),
        Command::Run {
            config: path,
            mode,
            json,
            duration,
        } => {
            prek::config::Config::parse(&read_config(&path)?).map_err(io::Error::other)?;
            if mode != "observe" {
                return Err(io::Error::other(
                    "routing is unavailable: virtual-device readiness, ownership and recovery gates are pending",
                ));
            }
            supervise_observer(&path, json, duration)
        }
        Command::ObserveWorker {
            config: path,
            json,
            duration,
        } => {
            let mut health = prek::supervisor::Heartbeat::connect()?;
            health.progress()?;
            let config =
                prek::config::Config::parse(&read_config(&path)?).map_err(io::Error::other)?;
            let engine = prek::rules::Engine::new(config.clone())?;
            let paths = linux::inventory()?
                .into_iter()
                .filter_map(|entry| {
                    entry
                        .device
                        .filter(|device| {
                            entry.available
                                && config.policy.allowed_seats.contains(&device.source.seat)
                                && config.devices.iter().any(|(id, selector)| {
                                    config.policy.allowed_devices.contains(id)
                                        && selector.matches(device)
                                })
                        })
                        .map(|_| entry.path)
                })
                .collect::<Vec<_>>();
            if paths.is_empty() {
                return Err(io::Error::other(
                    "no readable physical devices match the configured allowlist",
                ));
            }
            eprintln!(
                "Rule observation only: matches are reported; no actions are executed. Session-dependent rules require a verified provider and are skipped when unavailable."
            );
            let mut sink = RuleSink {
                engine,
                output: &mut output,
                json,
            };
            linux::observe_supervised(
                &paths,
                duration.map(Duration::from_secs),
                &mut sink,
                &mut health,
            )
        }
        Command::Doctor { config: path, json } => {
            let config =
                prek::config::Config::parse(&read_config(&path)?).map_err(io::Error::other)?;
            let report = prek::diagnostics::Report::inspect(&config, &linux::inventory()?);
            if json {
                serde_json::to_writer(&mut output, &report).map_err(io::Error::other)?;
                writeln!(output)?;
            } else {
                writeln!(
                    output,
                    "Prek preflight: {}",
                    if report.ready {
                        "ready for available observation tools"
                    } else {
                        "not operational"
                    }
                )?;
                for finding in &report.findings {
                    writeln!(
                        output,
                        "{}{}: {}",
                        finding.code,
                        finding
                            .rule
                            .as_ref()
                            .map(|rule| format!(" [{rule}]"))
                            .unwrap_or_default(),
                        finding.message
                    )?;
                }
            }
            if report.ready {
                Ok(())
            } else {
                Err(io::Error::other(
                    "preflight found unavailable requirements; see report",
                ))
            }
        }
        Command::Check { config: path, json } => {
            let text = read_config(&path)?;
            let config = prek::config::Config::parse(&text).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {error}", path.display()),
                )
            })?;
            if json {
                serde_json::to_writer(&mut output,&serde_json::json!({ "version":prek::config::SCHEMA_VERSION,"status":"valid","mode":config.mode,"rules":config.rules.len(),"selectors":config.devices.len() })).map_err(io::Error::other)?;
                writeln!(output)?;
            } else {
                writeln!(
                    output,
                    "Configuration valid: {} rules, {} device selectors, {:?} mode.",
                    config.rules.len(),
                    config.devices.len(),
                    config.mode
                )?;
            }
            Ok(())
        }
        Command::Init {
            preset,
            output: path,
            force,
        } => {
            let config = prek::config::Config::preset(&preset)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let text = toml::to_string_pretty(&config).map_err(io::Error::other)?;
            write_config(&path, &text, force)?;
            writeln!(output, "Created {} ({preset}).", path.display())
        }
        Command::Devices { json } => linux::list(&mut output, json),
        Command::Watch {
            devices,
            gestures_only,
            json,
        } => linux::watch(
            &devices,
            linux::ObserveOptions {
                gestures_only,
                json,
                duration: None,
            },
            &mut output,
            None,
        ),
        Command::Record {
            device,
            output: path,
            duration,
        } => {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            eprintln!(
                "Recording only the selected sources to {} for {duration}s.",
                path.display()
            );
            linux::watch(
                &device,
                linux::ObserveOptions {
                    gestures_only: true,
                    json: false,
                    duration: Some(Duration::from_secs(duration)),
                },
                &mut output,
                Some(&mut file),
            )
        }
        Command::Replay {
            path,
            config: Some(config_path),
            check,
            json,
        } => {
            let config = prek::config::Config::parse(&read_config(&config_path)?)
                .map_err(io::Error::other)?;
            let mut engine = prek::rules::Engine::new(config)?;
            prek::replay::visit(&mut BufReader::new(File::open(path)?), |record| {
                engine.push(record)
            })?;
            if check {
                engine.check()?;
            }
            let events = engine.take_events();
            if json {
                serde_json::to_writer(&mut output, &events).map_err(io::Error::other)?;
                writeln!(output)?;
            } else {
                writeln!(
                    output,
                    "Replayed {} configured rule outcomes{}.",
                    events.len(),
                    if check {
                        "; expected outcomes match"
                    } else {
                        ""
                    }
                )?;
            }
            Ok(())
        }
        Command::Replay {
            path,
            config: None,
            check,
            json,
        } => {
            let replay = prek::replay::read(&mut BufReader::new(File::open(path)?), check)?;
            if json {
                serde_json::to_writer(&mut output, replay.observations())
                    .map_err(io::Error::other)?;
                writeln!(output)?;
            } else {
                writeln!(
                    output,
                    "Replayed {} observations{}.",
                    replay.observations().len(),
                    if check {
                        "; expected outcomes match"
                    } else {
                        ""
                    }
                )?;
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn run_broker(
    config_path: &std::path::Path,
    runtime: &std::path::Path,
    compositor: &std::path::Path,
    library: &std::path::Path,
    json: bool,
    duration: Option<u64>,
) -> io::Result<()> {
    use nix::{
        libc,
        poll::{PollFd, PollFlags, PollTimeout, poll},
    };
    use prek::{
        ipc::Endpoint,
        service::{Broker, Policy, monotonic},
        supervisor::{Reason, Supervisor, TerminationSignals},
    };
    use std::os::fd::AsFd;
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "broker requires administrator device policy",
        ));
    }
    let config =
        prek::config::Config::parse(&read_config(config_path)?).map_err(io::Error::other)?;
    // Compile before touching runtime state or starting the device worker.
    prek::rules::Engine::new(config.clone())?;
    std::fs::create_dir_all(runtime)?;
    let endpoint = Endpoint::bind(runtime, true)?;
    let mut broker = Broker::open(
        endpoint,
        library,
        Policy {
            runtime_root: "/run/user".into(),
            compositor_executable: compositor.into(),
            agent_executable: std::env::current_exe()?,
            allowed_seats: config.policy.allowed_seats.into_iter().collect(),
        },
    )?;
    let signals = TerminationSignals::install()?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("observe-worker")
        .arg("--config")
        .arg(config_path);
    if json {
        command.arg("--json");
    }
    if let Some(duration) = duration {
        command.arg("--duration").arg(duration.to_string());
    }
    let mut supervisor = Supervisor::spawn(command)?;
    loop {
        if signals.requested() {
            supervisor.request_stop()?;
        }
        if let Some(report) = supervisor.check()? {
            return if signals.requested()
                || report.reason() == Reason::Requested
                || report.reason() == Reason::Exited && report.status().success()
            {
                Ok(())
            } else {
                Err(io::Error::other(format!(
                    "broker worker {:?}: {}",
                    report.reason(),
                    report.status()
                )))
            };
        }
        broker.step(monotonic()?)?;
        let until = broker
            .deadline()
            .into_iter()
            .chain([supervisor.deadline()?])
            .min()
            .unwrap();
        let timeout =
            PollTimeout::try_from(until.saturating_sub(monotonic()?)).map_err(io::Error::other)?;
        let mut fds = broker.poll_sources();
        fds.push(PollFd::new(supervisor.as_fd(), PollFlags::POLLIN));
        if let Err(error) = poll(&mut fds, timeout)
            && error != nix::errno::Errno::EINTR
        {
            return Err(error.into());
        }
    }
}

#[cfg(target_os = "linux")]
fn run_session(
    socket: &std::path::Path,
    instance: Option<String>,
    json: bool,
    duration: Option<u64>,
    inspect: bool,
    output: &mut impl Write,
) -> io::Result<()> {
    use nix::{
        libc,
        poll::{PollFd, PollFlags, PollTimeout, poll},
    };
    use prek::{
        ipc::{Channel, monotonic},
        protocol::{Mode, Reply, Request, Role},
        supervisor::TerminationSignals,
    };
    use std::os::fd::AsFd;
    if unsafe { libc::geteuid() } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session agent must run as the compositor's unprivileged user",
        ));
    }
    let instance = instance
        .or_else(|| std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok())
        .ok_or_else(|| {
            io::Error::other(
                "select the live compositor with --instance or HYPRLAND_INSTANCE_SIGNATURE",
            )
        })?;
    let registration = Request::Register {
        instance,
        role: if inspect { Role::Inspect } else { Role::Agent },
        capabilities: Default::default(),
    };
    registration.validate()?;
    let mut channel = Channel::connect(socket)?;
    if channel.peer().uid != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session endpoint is not a privileged broker",
        ));
    }
    if !channel.send(&registration, monotonic()?)? {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    let signals = TerminationSignals::install()?;
    let started = monotonic()?;
    let stop_at = duration.map(|duration| started + Duration::from_secs(duration));
    let mut fence = None;
    let mut next = started + Duration::from_secs(1);
    let mut last_reply = started;
    loop {
        let now = monotonic()?;
        if signals.requested() || stop_at.is_some_and(|deadline| now >= deadline) {
            if let Some(fence) = fence {
                let _ = channel.send(&Request::Disconnect { fence }, now);
            }
            return Ok(());
        }
        if let Some(packet) = channel.receive::<Reply>()? {
            packet.message.validate()?;
            last_reply = now;
            match packet.message {
                Reply::Welcome {
                    fence: accepted,
                    seat,
                    session,
                    mode,
                    capabilities,
                } if fence.is_none() => {
                    if mode != Mode::Observe || !capabilities.is_empty() {
                        return Err(io::Error::other(
                            "session adapter does not support these broker capabilities",
                        ));
                    }
                    fence = Some(accepted);
                    if !channel.send(&Request::Status { fence: accepted }, now)? {
                        return Err(io::ErrorKind::WouldBlock.into());
                    }
                    if !inspect {
                        if json {
                            serde_json::to_writer(
                                &mut *output,
                                &serde_json::json!({"version": 1, "registered": true, "seat": seat, "session": session, "mode": "observe"}),
                            )?;
                            writeln!(output)?;
                        } else {
                            writeln!(output, "registered {session} on {seat}; observation mode")?;
                        }
                        output.flush()?;
                    }
                    next = now + Duration::from_millis(250);
                }
                Reply::Status {
                    fence: observed,
                    state,
                    revision,
                    pending,
                } if Some(observed) == fence => {
                    if json {
                        serde_json::to_writer(
                            &mut *output,
                            &serde_json::json!({"version": 1, "fence": observed, "mode": "observe", "state": state, "revision": revision, "pending": pending}),
                        )?;
                        writeln!(output)?;
                    } else {
                        writeln!(
                            output,
                            "broker: observe, context {state:?}, pending {pending}"
                        )?;
                    }
                    output.flush()?;
                    if inspect {
                        let _ = channel.send(&Request::Disconnect { fence: observed }, now);
                        return Ok(());
                    }
                }
                Reply::Accepted {
                    fence: accepted, ..
                } if Some(accepted) == fence => {}
                Reply::Rejected { reason } | Reply::Revoked { reason, .. } => {
                    return Err(io::Error::other(reason));
                }
                _ => {
                    return Err(io::Error::other(
                        "unexpected broker message or stale session generation",
                    ));
                }
            }
        }
        if now.saturating_sub(last_reply) >= Duration::from_secs(1) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "broker registration/lease acknowledgement timed out",
            ));
        }
        if now >= next
            && let Some(fence) = fence
        {
            if !channel.send(&Request::Heartbeat { fence }, now)? {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            next = now + Duration::from_millis(250);
        }
        let until = next.min(last_reply + Duration::from_secs(1));
        let until = stop_at.map_or(until, |stop| until.min(stop));
        let timeout =
            PollTimeout::try_from(until.saturating_sub(monotonic()?)).map_err(io::Error::other)?;
        if let Err(error) = poll(
            &mut [PollFd::new(channel.as_fd(), PollFlags::POLLIN)],
            timeout,
        ) && error != nix::errno::Errno::EINTR
        {
            return Err(error.into());
        }
    }
}

#[cfg(target_os = "linux")]
fn supervise_observer(
    config: &std::path::Path,
    json: bool,
    duration: Option<u64>,
) -> io::Result<()> {
    use prek::supervisor::{Reason, Supervisor, TerminationSignals};
    let signals = TerminationSignals::install()?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg("observe-worker").arg("--config").arg(config);
    if json {
        command.arg("--json");
    }
    if let Some(duration) = duration {
        command.arg("--duration").arg(duration.to_string());
    }
    let mut supervisor = Supervisor::spawn(command)?;
    let report = supervisor.wait_with_stop(|| signals.requested())?;
    if signals.requested()
        || report.reason() == Reason::Requested
        || report.reason() == Reason::Exited && report.status().success()
    {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "observation worker {:?}: {}",
            report.reason(),
            report.status()
        )))
    }
}

#[cfg(target_os = "linux")]
struct RuleSink<'a, W: Write> {
    engine: prek::rules::Engine,
    output: &'a mut W,
    json: bool,
}
#[cfg(target_os = "linux")]
impl<W: Write> linux::InputSink for RuleSink<'_, W> {
    fn push(&mut self, record: prek::replay::Record) -> io::Result<()> {
        self.engine.push(record)?;
        for event in self.engine.take_events() {
            if self.json {
                serde_json::to_writer(&mut *self.output, &event).map_err(io::Error::other)?;
                writeln!(self.output)?;
            } else {
                writeln!(self.output, "{event:?}")?;
            }
        }
        Ok(())
    }
    fn deadline(&self) -> Option<Duration> {
        self.engine.deadline()
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

fn read_config(path: &std::path::Path) -> io::Result<String> {
    let file = File::open(path)?;
    let mut text = String::new();
    io::Read::read_to_string(
        &mut io::Read::take(file, (prek::config::MAX_CONFIG_BYTES + 1) as u64),
        &mut text,
    )?;
    if text.len() > prek::config::MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration exceeds 1 MiB",
        ));
    }
    Ok(text)
}

#[cfg(unix)]
fn write_config(path: &std::path::Path, text: &str, force: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let temp = parent.join(format!(".prek-init-{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        if force {
            std::fs::rename(&temp, path)
        } else {
            std::fs::hard_link(&temp, path)
        }
    })();
    let cleanup = std::fs::remove_file(&temp);
    // A successful force-rename already removed the temporary directory entry.
    if let Err(error) = cleanup
        && error.kind() != io::ErrorKind::NotFound
        && result.is_ok()
    {
        return Err(error);
    }
    result
}

#[cfg(not(target_os = "linux"))]
fn run(_command: Command) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the input monitor requires Linux",
    ))
}
