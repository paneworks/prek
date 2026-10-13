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
        Command::UdevImport { marker } => prek::kernel::import_metadata(&marker, &mut output),
        Command::Run {
            config: path,
            mode,
            json,
            duration,
        } => {
            let config =
                prek::config::Config::parse(&read_config(&path)?).map_err(io::Error::other)?;
            if mode != "observe" {
                return Err(io::Error::other(
                    "routing is unavailable: virtual-device readiness, ownership and recovery gates are pending",
                ));
            }
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
            linux::observe(&paths, duration.map(Duration::from_secs), &mut sink)
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
