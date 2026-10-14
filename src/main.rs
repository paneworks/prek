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
use prek::linux;

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
    /// Internal disposable compiler; reads source on stdin and owns no input.
    #[command(hide = true)]
    LuaCompileWorker {
        #[arg(long)]
        name: String,
    },
    /// Internal supervised, unprivileged Lua event component.
    #[command(hide = true)]
    LuaRuntimeWorker,
    /// Deliberately learn one shape without grabbing input or binding an action.
    LearnShape {
        name: String,
        #[arg(long)]
        device: PathBuf,
        #[arg(long)]
        output: PathBuf,
        /// Capture relative movement while holding the selected mouse button.
        #[arg(long, conflicts_with = "fingers")]
        mouse: bool,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=64))]
        fingers: u16,
        #[arg(long, default_value = "BTN_LEFT")]
        button: String,
        /// Maximum time to wait for a valid stroke; captures have their own bound.
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        duration: u64,
        /// Learn offline from the named source in a bounded recording.
        #[arg(long)]
        from_recording: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
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
        /// Trusted actual Morf ELF for native lock handoff.
        #[arg(long)]
        morf_executable: Option<PathBuf>,
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
        /// Receive configured rule outcomes from this session's seat.
        #[arg(long)]
        observations: bool,
        /// Use fresh context sampled from the broker-verified compositor.
        #[arg(long)]
        context: bool,
        /// Execute root-configured argv actions in this user session. Requires
        /// a routing broker; fresh compositor context is negotiated implicitly.
        #[arg(long)]
        commands: bool,
        /// Run approved Lua event subscriptions in a separate unprivileged VM.
        #[arg(long)]
        lua: bool,
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
    /// Validate and reload the broker's configured system file as administrator.
    Reload {
        #[arg(long, default_value = "/run/prek/control.sock")]
        socket: PathBuf,
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
    /// Describe the implemented Lua exports without accessing devices.
    Api {
        #[arg(long)]
        json: bool,
    },
    /// Create a complete declarative preset (.lua or legacy .toml).
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
        Command::LuaRuntimeWorker => prek::lua::session::worker(),
        Command::LuaCompileWorker { name } => {
            prek::lua_compiler::worker_limits()?;
            let mut source = String::new();
            io::Read::read_to_string(
                &mut io::Read::take(io::stdin(), (prek::config::MAX_CONFIG_BYTES + 1) as u64),
                &mut source,
            )?;
            let definition =
                prek::lua::Definition::compile(&source, &name).map_err(io::Error::other)?;
            serde_json::to_writer(&mut output, &definition).map_err(io::Error::other)
        }
        Command::Api { json } => {
            let api = prek::lua::api();
            if json {
                serde_json::to_writer(&mut output, &api).map_err(io::Error::other)?;
                writeln!(output)
            } else {
                writeln!(
                    output,
                    "Lua API {}: configuration and session events; runtime adapters are pending",
                    prek::lua::API_VERSION
                )?;
                for symbol in api["symbols"].as_array().unwrap() {
                    writeln!(output, "{}", symbol["signature"].as_str().unwrap())?;
                }
                Ok(())
            }
        }
        Command::LearnShape {
            name,
            device,
            output: path,
            mouse,
            fingers,
            button,
            duration,
            from_recording,
            json,
        } => {
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "shape output already exists",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let button = prek::config::Key::parse(&button).map_err(io::Error::other)?;
            let mut learner = prek::learner::Learner::new(
                name.clone(),
                device.to_string_lossy().into_owned(),
                if mouse { None } else { Some(fingers) },
                button,
            )?;
            if let Some(recording) = from_recording {
                prek::replay::visit(&mut BufReader::new(File::open(recording)?), |record| {
                    if !learner.finished()
                        && !matches!(
                            record,
                            prek::replay::Record::Expected { .. }
                                | prek::replay::Record::ExpectedRules { .. }
                        )
                    {
                        learner.push(record)?;
                        learner.take_events();
                    }
                    Ok(())
                })?;
            } else {
                let signals = prek::supervisor::TerminationSignals::install()?;
                eprintln!(
                    "Armed {name:?} on {}. Draw and release; Ctrl-C cancels. Input remains forwarded normally.",
                    device.display()
                );
                let mut sink = LearnSink {
                    learner: &mut learner,
                    signals: &signals,
                    next_tick: linux::monotonic()? + Duration::from_millis(250),
                };
                linux::observe(&[device], Some(Duration::from_secs(duration)), &mut sink)?;
                if signals.requested() {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "shape capture cancelled",
                    ));
                }
            }
            let exported = learner.export()?;
            let stroke = learner.stroke().unwrap();
            let text = toml::to_string_pretty(&exported).map_err(io::Error::other)?;
            write_config(&path, &text, false)?;
            if json {
                serde_json::to_writer(
                    &mut output,
                    &serde_json::json!({"version":1,"name":name,"samples":stroke.points().len(),"space":stroke.space,"duration":stroke.finished-stroke.started,"output":path,"enabled_rules":0}),
                )?;
                writeln!(output)
            } else {
                writeln!(
                    output,
                    "Saved {name:?}: {} samples in {:?}, {:?}, {}. No action is bound.",
                    stroke.points().len(),
                    stroke.finished - stroke.started,
                    stroke.space,
                    path.display()
                )
            }
        }
        Command::Broker {
            config,
            mode,
            compositor_executable,
            morf_executable,
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
                morf_executable.as_deref(),
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
            observations,
            context,
            commands,
            lua,
        } => run_session(
            &socket,
            instance,
            SessionOptions {
                json,
                duration,
                inspect: false,
                observations,
                context,
                commands,
                lua,
            },
            &mut output,
        ),
        Command::Status {
            socket,
            instance,
            json,
        } => run_session(
            &socket,
            instance,
            SessionOptions {
                json,
                inspect: true,
                ..Default::default()
            },
            &mut output,
        ),
        Command::Reload { socket, json } => reload_broker(&socket, json, &mut output),
        Command::UdevImport { marker } => prek::kernel::import_metadata(&marker, &mut output),
        Command::Run {
            config: path,
            mode,
            json,
            duration,
        } => {
            compile_config(read_config(&path)?, &path)?;
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
            let mut control = health.take_control();
            let config = if let Some(channel) = control.as_mut() {
                prek::worker::bootstrap(channel, &mut health)?
            } else {
                compile_config(read_config(&path)?, &path)?.config
            };
            let engine = prek::rules::Engine::new(config.clone())?;
            let paths = Vec::new();
            eprintln!(
                "Rule observation only: matches are reported; no actions are executed. Session-dependent rules require a verified provider and are skipped when unavailable."
            );
            if let Some(channel) = control {
                let mut sink = BrokerSink {
                    stream: prek::worker::Stream::new(config, channel)?,
                };
                return linux::observe_supervised(
                    &paths,
                    duration.map(Duration::from_secs),
                    &mut sink,
                    &mut health,
                );
            }
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
            let config = compile_config(read_config(&path)?, &path)?.config;
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
            let config = compile_config(text, &path)
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: {error}", path.display()),
                    )
                })?
                .config;
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
            let text = if path.extension().is_some_and(|extension| extension == "lua") {
                format!(
                    "local prek = require(\"prek\")\nlocal config = prek.config.preset(\"{preset}\")\n\n-- Add or customize validated rules here.\nreturn config\n"
                )
            } else {
                toml::to_string_pretty(&config).map_err(io::Error::other)?
            };
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
            let config = compile_config(read_config(&config_path)?, &config_path)?.config;
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
    morf: Option<&std::path::Path>,
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
    let text = read_system_config(config_path)?;
    let compiled = compile_config(text, config_path)?;
    let (text, config, runtime_definition) = (compiled.text, compiled.config, compiled.runtime);
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
            morf_executable: morf.map(Into::into),
            allowed_seats: config.policy.allowed_seats.iter().cloned().collect(),
        },
    )?;
    broker.install_lua_source(runtime_definition.as_ref())?;
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
    let (mut supervisor, mut control) = Supervisor::spawn_controlled(command)?;
    broker.enable_observations();
    let mut refresh = monotonic()?;
    let mut revision = 0u64;
    let mut subscriptions = None;
    let mut control_closed = false;
    let mut transfer = Some(prek::configuration::Transfer::new(text, 1)?);
    let mut installation = Some((
        1,
        config,
        runtime_definition,
        monotonic()? + prek::configuration::INSTALL_TIMEOUT,
    ));
    let mut compiler: Option<prek::lua_compiler::Job> = None;
    let mut startup = true;
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
        let now = monotonic()?;
        if installation
            .as_ref()
            .is_some_and(|(_, _, _, deadline)| now >= *deadline)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "worker configuration installation timed out",
            ));
        }
        if broker.take_reload_request() {
            if installation.is_some() || compiler.is_some() {
                broker.finish_reload(
                    Err("configuration compilation or installation is in progress".into()),
                    now,
                )?;
            } else {
                let proposed = read_system_config(config_path).and_then(|text| {
                    if config_path
                        .extension()
                        .is_some_and(|extension| extension == "lua")
                    {
                        compiler = Some(spawn_compiler(text, config_path)?);
                        Ok(None)
                    } else {
                        let compiled = compile_config(text, config_path)?;
                        prek::rules::Engine::new(compiled.config.clone())?;
                        Ok(Some(compiled))
                    }
                });
                match proposed {
                    Ok(Some(compiled)) => {
                        let config = compiled.config;
                        let generation =
                            broker.configuration().checked_add(1).ok_or_else(|| {
                                io::Error::other("configuration generation exhausted")
                            })?;
                        transfer = Some(prek::configuration::Transfer::new(
                            compiled.text,
                            generation,
                        )?);
                        installation = Some((
                            generation,
                            config,
                            compiled.runtime,
                            now + prek::configuration::INSTALL_TIMEOUT,
                        ));
                    }
                    Ok(None) => {}
                    Err(error) => broker.finish_reload(Err(error.to_string()), now)?,
                }
            }
        }
        if let Some(job) = compiler.as_mut() {
            match job.step() {
                Ok(Some(compiled)) => {
                    compiler = None;
                    match prek::rules::Engine::new(compiled.config.clone()) {
                        Ok(_) => {
                            let generation =
                                broker.configuration().checked_add(1).ok_or_else(|| {
                                    io::Error::other("configuration generation exhausted")
                                })?;
                            transfer = Some(prek::configuration::Transfer::new(
                                compiled.text,
                                generation,
                            )?);
                            installation = Some((
                                generation,
                                compiled.config,
                                compiled.runtime,
                                now + prek::configuration::INSTALL_TIMEOUT,
                            ));
                        }
                        Err(error) => broker.finish_reload(Err(error.to_string()), now)?,
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    compiler = None;
                    broker.finish_reload(Err(error.to_string()), now)?;
                }
            }
        }
        if !control_closed && let Some(snapshot) = transfer.as_mut() {
            match snapshot.send(&mut control, now) {
                Ok(true) => transfer = None,
                Ok(false) => {}
                Err(error) if worker_link_closed(&error) => control_closed = true,
                Err(error) => return Err(error),
            }
        }
        if now >= refresh {
            revision = revision
                .checked_add(1)
                .ok_or_else(|| io::Error::other("worker subscription revision exhausted"))?;
            subscriptions = Some(prek::worker::Subscriptions {
                revision,
                seats: broker.subscriptions(now)?,
            });
            refresh = now + prek::service::CONTEXT_REFRESH;
        }
        if !control_closed
            && (!startup || transfer.is_none())
            && let Some(update) = subscriptions.as_ref()
        {
            match control.send(
                &prek::configuration::Command::Subscriptions {
                    update: prek::worker::Subscriptions {
                        revision: update.revision,
                        seats: update.seats.clone(),
                    },
                },
                now,
            ) {
                Ok(true) => subscriptions = None,
                Ok(false) => {}
                Err(error) if worker_link_closed(&error) => control_closed = true,
                Err(error) => return Err(error),
            }
        }
        // A report may require a bounded compositor lock recheck. Return to
        // supervision after each packet so failed IPC cannot delay health.
        for _ in 0..1 {
            if control_closed {
                break;
            }
            match control.receive::<prek::worker::Message>() {
                Ok(Some(packet)) => match packet.message {
                    prek::worker::Message::Outcome { report } => {
                        broker.report(report, monotonic()?)?
                    }
                    prek::worker::Message::Configuration { generation, error } => {
                        let (expected, config, runtime_definition, _) =
                            installation.take().ok_or_else(|| {
                                io::Error::other("unsolicited worker configuration acknowledgement")
                            })?;
                        if generation != expected {
                            return Err(io::Error::other(
                                "worker acknowledged a different configuration generation",
                            ));
                        }
                        if startup {
                            if let Some(error) = error {
                                return Err(io::Error::other(error));
                            }
                            startup = false;
                        } else {
                            let previous = broker.configuration();
                            broker.finish_configuration(
                                error.map_or(Ok(config), Err),
                                monotonic()?,
                            )?;
                            if broker.configuration() != previous {
                                broker.install_lua_source(runtime_definition.as_ref())?;
                            }
                            refresh = monotonic()?;
                            subscriptions = None;
                        }
                    }
                    prek::worker::Message::Transport {
                        subscription,
                        descriptor,
                    } => {
                        if let Some(command) =
                            broker.transport(&subscription, &descriptor, monotonic()?)?
                        {
                            let _ = control.send(&command, monotonic()?)?;
                        }
                    }
                },
                Ok(None) => break,
                Err(error) if worker_link_closed(&error) => control_closed = true,
                Err(error) => return Err(error),
            }
        }
        let until = broker
            .deadline()
            .into_iter()
            .chain([supervisor.deadline()?])
            .chain([refresh])
            .chain(installation.as_ref().map(|(_, _, _, deadline)| *deadline))
            .chain(compiler.as_ref().map(prek::lua_compiler::Job::deadline))
            .min()
            .unwrap();
        let timeout =
            PollTimeout::try_from(until.saturating_sub(monotonic()?)).map_err(io::Error::other)?;
        let mut fds = broker.poll_sources();
        if let Some(job) = &compiler {
            fds.extend(job.poll_sources());
        }
        fds.push(PollFd::new(supervisor.as_fd(), PollFlags::POLLIN));
        if !control_closed {
            fds.push(PollFd::new(
                control.as_fd(),
                PollFlags::POLLIN
                    | if subscriptions.is_some() || transfer.is_some() {
                        PollFlags::POLLOUT
                    } else {
                        PollFlags::empty()
                    },
            ));
        }
        if let Err(error) = poll(&mut fds, timeout)
            && error != nix::errno::Errno::EINTR
        {
            return Err(error.into());
        }
    }
}

#[cfg(target_os = "linux")]
fn worker_link_closed(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
    )
}

#[cfg(target_os = "linux")]
fn reload_broker(socket: &std::path::Path, json: bool, output: &mut impl Write) -> io::Result<()> {
    use nix::{
        libc,
        poll::{PollFd, PollFlags, PollTimeout, poll},
    };
    use prek::{
        ipc::{Channel, monotonic},
        protocol::{Reply, Request},
    };
    use std::os::fd::AsFd;
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "configuration reload requires an administrator",
        ));
    }
    let mut channel = Channel::connect(socket)?;
    if channel.peer().uid != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "reload endpoint is not a privileged broker",
        ));
    }
    if !channel.send(&Request::Reload {}, monotonic()?)? {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    let deadline = monotonic()? + prek::configuration::INSTALL_TIMEOUT;
    loop {
        if let Some(packet) = channel.receive::<Reply>()? {
            packet.message.validate()?;
            match packet.message {
                Reply::ReloadResult {
                    configuration,
                    error,
                } => {
                    if json {
                        serde_json::to_writer(
                            &mut *output,
                            &serde_json::json!({"version": 1, "configuration": configuration, "error": error}),
                        )?;
                        writeln!(output)?;
                        output.flush()?;
                    }
                    if let Some(error) = error {
                        return Err(io::Error::other(error));
                    }
                    if !json {
                        writeln!(output, "activated configuration {configuration}")?;
                    }
                    return Ok(());
                }
                Reply::Rejected { reason } => return Err(io::Error::other(reason)),
                _ => return Err(io::Error::other("unexpected configuration reload reply")),
            }
        }
        let now = monotonic()?;
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "configuration reload acknowledgement timed out",
            ));
        }
        let timeout =
            PollTimeout::try_from(deadline.saturating_sub(now)).map_err(io::Error::other)?;
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
#[derive(Default)]
struct SessionOptions {
    json: bool,
    duration: Option<u64>,
    inspect: bool,
    observations: bool,
    context: bool,
    commands: bool,
    lua: bool,
}

#[cfg(target_os = "linux")]
fn run_session(
    socket: &std::path::Path,
    instance: Option<String>,
    options: SessionOptions,
    output: &mut impl Write,
) -> io::Result<()> {
    let SessionOptions {
        json,
        duration,
        inspect,
        observations,
        context,
        commands,
        lua,
    } = options;
    use nix::{
        libc,
        poll::{PollFd, PollFlags, PollTimeout, poll},
    };
    use prek::{
        ipc::{Channel, monotonic},
        protocol::{Capability, Mode, Reply, Request, Role},
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
    let mut requested = std::collections::BTreeSet::new();
    let observations = observations || lua;
    if observations {
        requested.insert(Capability::Observations);
    }
    if context || commands || lua {
        requested.insert(Capability::Context);
    }
    if commands {
        requested.insert(Capability::Commands);
    }
    if lua {
        requested.insert(Capability::LuaEvents);
    }
    let registration = Request::Register {
        instance,
        role: if inspect { Role::Inspect } else { Role::Agent },
        capabilities: requested.clone(),
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
    let mut broker_mode = Mode::Observe;
    let mut executor = None::<prek::actions::Executor>;
    let mut lua_download = None::<prek::lua::source_transfer::Assembly>;
    let mut lua_runtime = None::<prek::lua::session::Session>;
    let mut lua_seat = String::new();
    let mut lua_pending = prek::lua::session::PendingEvents::default();
    let mut lua_lifecycle = None;
    loop {
        let now = monotonic()?;
        if signals.requested() || stop_at.is_some_and(|deadline| now >= deadline) {
            if let Some(runtime) = lua_runtime.as_mut() {
                runtime.stop()?;
                let until = monotonic()? + Duration::from_millis(250);
                while monotonic()? < until && !runtime.finished() {
                    let logs = match runtime.step(monotonic()?) {
                        Ok(logs) => logs,
                        Err(_) => break,
                    };
                    for record in logs {
                        if json {
                            serde_json::to_writer(
                                &mut *output,
                                &serde_json::json!({"version":1,"kind":"lua_log","fence":fence,"record":record}),
                            )?;
                            writeln!(output)?;
                        } else {
                            writeln!(output, "Lua: {} {}", record.message, record.fields)?;
                        }
                    }
                    let timeout = PollTimeout::try_from(
                        until
                            .saturating_sub(monotonic()?)
                            .min(Duration::from_millis(10)),
                    )
                    .map_err(io::Error::other)?;
                    let _ = poll(&mut runtime.poll_sources(), timeout);
                }
                output.flush()?;
            }
            if let Some(fence) = fence {
                let _ = channel.send(&Request::Disconnect { fence }, now);
            }
            return Ok(());
        }
        let mut drained = false;
        for _ in 0..32 {
            let Some(packet) = channel.receive::<Reply>()? else {
                drained = true;
                break;
            };
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
                    if (commands && mode != Mode::Route) || capabilities != requested {
                        return Err(io::Error::other(
                            "session adapter does not support these broker capabilities",
                        ));
                    }
                    broker_mode = mode;
                    if commands {
                        executor = Some(prek::actions::Executor::new(
                            accepted,
                            seat.clone(),
                            mode,
                            capabilities,
                        )?);
                    }
                    fence = Some(accepted);
                    lua_seat = seat.clone();
                    if lua {
                        lua_download =
                            Some(prek::lua::source_transfer::Assembly::new(accepted, now)?);
                        if !channel.send(
                            &Request::LuaSource {
                                fence: accepted,
                                offset: 0,
                            },
                            now,
                        )? {
                            return Err(io::ErrorKind::WouldBlock.into());
                        }
                    }
                    if !channel.send(&Request::Status { fence: accepted }, now)? {
                        return Err(io::ErrorKind::WouldBlock.into());
                    }
                    if !inspect {
                        if json {
                            serde_json::to_writer(
                                &mut *output,
                                &serde_json::json!({"version": 1, "registered": true, "seat": seat, "session": session, "mode": mode}),
                            )?;
                            writeln!(output)?;
                        } else {
                            writeln!(output, "registered {session} on {seat}; {mode:?} mode")?;
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
                    last_reload_error,
                    context_error,
                } if Some(observed) == fence => {
                    if json {
                        serde_json::to_writer(
                            &mut *output,
                            &serde_json::json!({"version": 1, "fence": observed, "mode": broker_mode, "state": state, "revision": revision, "pending": pending, "last_reload_error": last_reload_error, "context_error": context_error}),
                        )?;
                        writeln!(output)?;
                    } else {
                        writeln!(
                            output,
                            "broker: {broker_mode:?}, context {state:?}, pending {pending}"
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
                Reply::Configuration { fence: next_fence }
                    if fence.is_some_and(|old| {
                        old.broker == next_fence.broker
                            && old.session <= next_fence.session
                            && old.configuration < next_fence.configuration
                    }) =>
                {
                    if let Some(executor) = executor.as_mut() {
                        executor.transition(next_fence)?;
                    }
                    fence = Some(next_fence);
                    if lua {
                        lua_runtime = None;
                        lua_pending = Default::default();
                        lua_lifecycle = Some((
                            prek::lua::EventKind::Reload,
                            serde_json::json!({"fence":next_fence}),
                        ));
                        lua_download =
                            Some(prek::lua::source_transfer::Assembly::new(next_fence, now)?);
                        if !channel.send(
                            &Request::LuaSource {
                                fence: next_fence,
                                offset: 0,
                            },
                            now,
                        )? {
                            return Err(io::ErrorKind::WouldBlock.into());
                        }
                    }
                    if json {
                        serde_json::to_writer(
                            &mut *output,
                            &serde_json::json!({"version": 1, "configuration": next_fence.configuration, "fence": next_fence}),
                        )?;
                        writeln!(output)?;
                    } else {
                        writeln!(
                            output,
                            "configuration {} activated; previous gesture progress cancelled",
                            next_fence.configuration
                        )?;
                    }
                    output.flush()?;
                    if !channel.send(&Request::Heartbeat { fence: next_fence }, now)? {
                        return Err(io::ErrorKind::WouldBlock.into());
                    }
                    if inspect && !channel.send(&Request::Status { fence: next_fence }, now)? {
                        return Err(io::ErrorKind::WouldBlock.into());
                    }
                    next = now + Duration::from_millis(250);
                }
                Reply::SessionChanged {
                    fence: next_fence,
                    state,
                } if fence.is_some_and(|old| {
                    old.broker == next_fence.broker
                        && old.configuration <= next_fence.configuration
                        && old.session < next_fence.session
                }) =>
                {
                    if let Some(executor) = executor.as_mut() {
                        executor.transition(next_fence)?;
                    }
                    fence = Some(next_fence);
                    if lua {
                        lua_runtime = None;
                        lua_pending = Default::default();
                        lua_lifecycle = Some((
                            prek::lua::EventKind::SessionChanged,
                            serde_json::json!({"fence":next_fence,"state":state}),
                        ));
                        lua_download =
                            Some(prek::lua::source_transfer::Assembly::new(next_fence, now)?);
                        if !channel.send(
                            &Request::LuaSource {
                                fence: next_fence,
                                offset: 0,
                            },
                            now,
                        )? {
                            return Err(io::ErrorKind::WouldBlock.into());
                        }
                    }
                    if json {
                        serde_json::to_writer(
                            &mut *output,
                            &serde_json::json!({"version": 1,
                            "session_changed": true, "fence": next_fence, "state": state,
                            "configuration": next_fence.configuration}),
                        )?;
                        writeln!(output)?;
                    } else {
                        writeln!(
                            output,
                            "context {state:?}; previous gesture progress cancelled"
                        )?;
                    }
                    output.flush()?;
                    if !channel.send(&Request::Heartbeat { fence: next_fence }, now)? {
                        return Err(io::ErrorKind::WouldBlock.into());
                    }
                    if !channel.send(&Request::Status { fence: next_fence }, now)? {
                        return Err(io::ErrorKind::WouldBlock.into());
                    }
                    next = now + Duration::from_millis(250);
                }
                Reply::Subscribed { fence: subscribed }
                    if (observations || commands) && Some(subscribed) == fence =>
                {
                    if json {
                        serde_json::to_writer(
                            &mut *output,
                            &serde_json::json!({"version": 1, "subscribed": true, "fence": subscribed}),
                        )?;
                        writeln!(output)?;
                    } else {
                        writeln!(output, "observing fresh input on the registered seat")?;
                    }
                    output.flush()?;
                }
                Reply::Observation {
                    fence: observed,
                    event,
                } if observations && Some(observed) == fence => {
                    if lua_runtime.is_some() || lua_download.is_some() {
                        let payload = serde_json::json!({"fence":observed,"event":event});
                        let kind = match event.as_ref() {
                            prek::rules::Event::Matched { stage, .. } => match stage {
                                prek::rules::Stage::Discrete => prek::lua::EventKind::Matched,
                                prek::rules::Stage::Begin => prek::lua::EventKind::Begin,
                                prek::rules::Stage::Update => prek::lua::EventKind::Update,
                                prek::rules::Stage::End => prek::lua::EventKind::End,
                                prek::rules::Stage::Cancel => prek::lua::EventKind::Cancel,
                            },
                            prek::rules::Event::Cancelled { .. } => prek::lua::EventKind::Cancel,
                            prek::rules::Event::Skipped { .. } => prek::lua::EventKind::Skipped,
                        };
                        let result = if let Some(runtime) = lua_runtime.as_mut() {
                            if kind != prek::lua::EventKind::Matched
                                && matches!(event.as_ref(), prek::rules::Event::Matched { .. })
                            {
                                runtime
                                    .notify(prek::lua::EventKind::Matched, payload.clone())
                                    .and_then(|()| runtime.notify(kind, payload))
                            } else {
                                runtime.notify(kind, payload)
                            }
                        } else {
                            if kind != prek::lua::EventKind::Matched
                                && matches!(event.as_ref(), prek::rules::Event::Matched { .. })
                            {
                                lua_pending
                                    .push(prek::lua::EventKind::Matched, payload.clone())
                                    .and_then(|()| lua_pending.push(kind, payload))
                            } else {
                                lua_pending.push(kind, payload)
                            }
                        };
                        if let Err(error) = result {
                            eprintln!("Lua runtime unavailable: {error}");
                            lua_runtime = None;
                            lua_download = None;
                            lua_pending = Default::default();
                        }
                    }
                    if json {
                        serde_json::to_writer(
                            &mut *output,
                            &Reply::Observation {
                                fence: observed,
                                event,
                            },
                        )?;
                        writeln!(output)?;
                    } else {
                        writeln!(output, "{event:?}")?;
                    }
                    output.flush()?;
                }
                Reply::Dispatch {
                    fence: offered,
                    event,
                } if commands && Some(offered) == fence => {
                    executor
                        .as_mut()
                        .unwrap()
                        .offer(offered, event, monotonic()?)?;
                }
                Reply::LuaSource {
                    fence: incoming,
                    offset,
                    bytes,
                    data,
                } if lua && Some(incoming) == fence => {
                    let Some(download) = lua_download.as_mut() else {
                        continue;
                    };
                    match download.push(incoming, offset, bytes, &data, now)? {
                        Some(definition) => {
                            lua_download = None;
                            let loaded = definition
                                .map(|definition| {
                                    prek::lua::session::Session::spawn(
                                        &std::env::current_exe()?,
                                        incoming,
                                        &lua_seat,
                                        definition,
                                    )
                                })
                                .transpose();
                            lua_runtime = match loaded {
                                Ok(runtime) => runtime,
                                Err(error) => {
                                    eprintln!("Lua runtime unavailable: {error}");
                                    None
                                }
                            };
                            if let Some(runtime) = lua_runtime.as_mut()
                                && let Some((kind, payload)) = lua_lifecycle.take()
                            {
                                runtime.notify(kind, payload.clone())?;
                                if kind == prek::lua::EventKind::SessionChanged
                                    && payload["state"] != "desktop"
                                {
                                    runtime.notify(prek::lua::EventKind::Recovery, payload)?;
                                }
                            }
                            if let Some(runtime) = lua_runtime.as_mut()
                                && let Err(error) =
                                    std::mem::take(&mut lua_pending).drain_into(runtime)
                            {
                                eprintln!("Lua runtime unavailable: {error}");
                                lua_runtime = None;
                            }
                            if lua_runtime.is_none() {
                                lua_pending = Default::default();
                                lua_lifecycle = None;
                            }
                        }
                        None => {
                            if !channel.send(
                                &Request::LuaSource {
                                    fence: incoming,
                                    offset: download.offset(),
                                },
                                now,
                            )? {
                                return Err(io::ErrorKind::WouldBlock.into());
                            }
                        }
                    }
                }
                Reply::Execute {
                    fence: granted,
                    id,
                    ordinal,
                    expires,
                } if commands && Some(granted) == fence => {
                    executor.as_mut().unwrap().grant(
                        granted,
                        id,
                        ordinal,
                        expires,
                        monotonic()?,
                    )?;
                }
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
        if let Some(executor) = executor.as_mut() {
            executor.reap()?;
            if drained {
                executor.execute_ready(monotonic()?)?;
            }
            executor.flush(&mut channel)?;
        }
        if drained && let Some(runtime) = lua_runtime.as_mut() {
            match runtime.step(monotonic()?) {
                Ok(logs) => {
                    for record in logs {
                        if json {
                            serde_json::to_writer(
                                &mut *output,
                                &serde_json::json!({"version":1,"kind":"lua_log","fence":fence,"record":record}),
                            )?;
                            writeln!(output)?;
                        } else {
                            writeln!(output, "Lua: {} {}", record.message, record.fields)?;
                        }
                    }
                    output.flush()?;
                }
                Err(error) => {
                    eprintln!("Lua runtime unavailable: {error}");
                    lua_runtime = None;
                }
            }
        }
        if lua_download
            .as_ref()
            .is_some_and(|download| now >= download.deadline())
        {
            eprintln!("Lua source transfer timed out");
            lua_download = None;
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
        let until = lua_download
            .as_ref()
            .map_or(until, |download| until.min(download.deadline()));
        let until = if let Some(runtime) = &lua_runtime {
            until.min(runtime.deadline()?)
        } else {
            until
        };
        let timeout =
            PollTimeout::try_from(until.saturating_sub(monotonic()?)).map_err(io::Error::other)?;
        if !drained {
            continue;
        }
        let mut sources = vec![PollFd::new(
            channel.as_fd(),
            PollFlags::POLLIN
                | if executor
                    .as_ref()
                    .is_some_and(|executor| executor.wants_write())
                {
                    PollFlags::POLLOUT
                } else {
                    PollFlags::empty()
                },
        )];
        if let Some(executor) = executor.as_ref() {
            sources.extend(executor.poll_sources());
        }
        if let Some(runtime) = &lua_runtime {
            sources.extend(runtime.poll_sources());
        }
        if let Err(error) = poll(&mut sources, timeout)
            && error != nix::errno::Errno::EINTR
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
struct BrokerSink {
    stream: prek::worker::Stream,
}
#[cfg(target_os = "linux")]
impl linux::InputSink for BrokerSink {
    fn push(&mut self, record: prek::replay::Record) -> io::Result<()> {
        self.stream.push(record)
    }
    fn deadline(&self) -> Option<Duration> {
        self.stream.deadline()
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
    fn service(&mut self) -> io::Result<()> {
        self.stream.service()
    }
    fn poll_sources(&self) -> Vec<nix::poll::PollFd<'_>> {
        vec![self.stream.poll_source()]
    }
    fn device_policy(&self) -> Option<(&prek::config::Config, u64)> {
        Some((self.stream.config(), self.stream.generation()))
    }
}

#[cfg(target_os = "linux")]
struct LearnSink<'a> {
    learner: &'a mut prek::learner::Learner,
    signals: &'a prek::supervisor::TerminationSignals,
    next_tick: Duration,
}
#[cfg(target_os = "linux")]
impl linux::InputSink for LearnSink<'_> {
    fn push(&mut self, record: prek::replay::Record) -> io::Result<()> {
        if self.finished() {
            return Ok(());
        }
        let ready = matches!(record, prek::replay::Record::Device { .. });
        if let prek::replay::Record::Tick { at } = &record {
            self.next_tick = *at + Duration::from_millis(250);
        }
        self.learner.push(record)?;
        if ready {
            eprintln!("Shape reader ready; draw a fresh stroke and release.");
        }
        // Feedback contains only outcomes, never intermediate trajectories.
        for event in self.learner.take_events() {
            if let prek::rules::Event::Cancelled { reason, .. } = event
                && !self.learner.finished()
            {
                eprintln!("Capture rejected: {reason}. Try a fresh stroke.");
            }
        }
        Ok(())
    }
    fn deadline(&self) -> Option<Duration> {
        Some(
            self.learner
                .deadline()
                .into_iter()
                .chain([self.next_tick])
                .min()
                .unwrap(),
        )
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn finished(&self) -> bool {
        self.learner.finished() || self.signals.requested()
    }
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
    fn device_policy(&self) -> Option<(&prek::config::Config, u64)> {
        Some((self.engine.config(), self.engine.generation()))
    }
}

fn spawn_compiler(text: String, path: &std::path::Path) -> io::Result<prek::lua_compiler::Job> {
    let executable = std::env::current_exe()?;
    // Release builds accept no environment-selected compiler executable.
    #[cfg(debug_assertions)]
    if let Some(runner) = std::env::var_os("PREK_TEST_RUNNER") {
        return prek::lua_compiler::Job::spawn_with_runner(
            std::path::Path::new(&runner),
            &executable,
            text,
            &path.to_string_lossy(),
        );
    }
    prek::lua_compiler::Job::spawn(&executable, text, &path.to_string_lossy())
}

fn compile_config(
    text: String,
    path: &std::path::Path,
) -> io::Result<prek::lua_compiler::Compiled> {
    if path.extension().is_some_and(|extension| extension == "lua") {
        // The root-validated model crosses the private channel. The device
        // worker never evaluates the source or rereads its Lua filepath.
        spawn_compiler(text, path)?.wait()
    } else {
        let config = prek::config::Config::parse(&text).map_err(io::Error::other)?;
        Ok(prek::lua_compiler::Compiled {
            text,
            config,
            runtime: None,
        })
    }
}

fn read_config(path: &std::path::Path) -> io::Result<String> {
    let file = File::open(path)?;
    let mut text = String::new();
    io::Read::read_to_string(
        &mut io::Read::take(&file, (prek::config::MAX_CONFIG_BYTES + 1) as u64),
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

#[cfg(target_os = "linux")]
fn read_system_config(path: &std::path::Path) -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    for directory in absolute
        .parent()
        .into_iter()
        .flat_map(std::path::Path::ancestors)
    {
        let stat = std::fs::metadata(directory)?;
        if !stat.is_dir() || stat.uid() != 0 || stat.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "broker configuration requires protected root-owned parent directories",
            ));
        }
    }
    let file = File::open(path)?;
    let stat = file.metadata()?;
    if !stat.is_file() || stat.uid() != 0 || stat.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "broker configuration must be a protected root-owned regular file",
        ));
    }
    let mut text = String::new();
    io::Read::read_to_string(
        &mut io::Read::take(&file, (prek::config::MAX_CONFIG_BYTES + 1) as u64),
        &mut text,
    )?;
    let after = file.metadata()?;
    if (
        stat.len(),
        stat.mtime(),
        stat.mtime_nsec(),
        stat.ctime(),
        stat.ctime_nsec(),
    ) != (
        after.len(),
        after.mtime(),
        after.mtime_nsec(),
        after.ctime(),
        after.ctime_nsec(),
    ) {
        return Err(io::Error::other(
            "broker configuration changed during its snapshot read",
        ));
    }
    if text.len() > prek::config::MAX_CONFIG_BYTES {
        return Err(io::Error::other("configuration exceeds size limit"));
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
