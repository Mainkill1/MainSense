mod metrics;
mod sensors;
mod snapshot_log;
mod tui;

use clap::{error::ErrorKind, CommandFactory, Parser};
use metrics::Collector;
use serde_json::Value;
use snapshot_log::SnapshotLog;
use std::env;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static STOPPED: AtomicBool = AtomicBool::new(false);

#[derive(Parser)]
#[command(
    version,
    about = "Read-only Linux CPU, AMD GPU, RAM, VRAM, and sensor monitoring",
    after_help = "Keys: q quit, Space pause, +/- refresh, c cores, t sensors, p processes, g GPU, ? help."
)]
struct Args {
    /// Refresh period in seconds, from 0.2 to 5
    #[arg(short, long, default_value = "1", value_name = "SECONDS", value_parser = interval, allow_negative_numbers = true)]
    interval: Duration,
    /// Print one snapshot and exit
    #[arg(long)]
    once: bool,
    /// Stream one JSON object per sample
    #[arg(long)]
    json: bool,
    /// Stop JSON output after N samples (0: unlimited)
    #[arg(long, default_value_t = 0, value_name = "N")]
    count: u64,
    /// Append timestamped telemetry snapshots as JSON lines
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,
    /// Use ASCII borders and graphs
    #[arg(long)]
    ascii: bool,
    /// Disable terminal colors (also honors NO_COLOR)
    #[arg(long)]
    no_color: bool,
    /// Start with core and sensor detail collapsed
    #[arg(long)]
    compact: bool,
}

fn interval(text: &str) -> Result<Duration, &'static str> {
    let value = text
        .parse::<f64>()
        .map_err(|_| "interval must be a number of seconds")?;
    if !(0.2..=5.0).contains(&value) {
        return Err("interval must be between 0.2 and 5 seconds");
    }
    Ok(Duration::from_secs_f64(value))
}

extern "C" fn stop(_: libc::c_int) {
    STOPPED.store(true, Ordering::Relaxed);
}

fn handle_signals() -> io::Result<()> {
    for (signal, handler) in [
        (libc::SIGINT, stop as libc::sighandler_t),
        (libc::SIGTERM, stop as libc::sighandler_t),
        // Let the log writer handle and roll back partial limit-restricted writes.
        (libc::SIGXFSZ, libc::SIG_IGN),
    ] {
        // Only the atomic flag is accessed from the asynchronous handlers.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

fn wait(interval: Duration) {
    let deadline = Instant::now() + interval;
    while !STOPPED.load(Ordering::Relaxed) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

fn json_line(snapshot: &Value) -> String {
    let mut line = snapshot.to_string();
    line.push('\n');
    line
}

fn record(log: &mut SnapshotLog, line: String) -> io::Result<()> {
    log.record(line).map_err(log_error)
}

fn log_error(error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("log file: {error}"))
}

fn write_stdout(mut output: &[u8]) -> io::Result<()> {
    let mut descriptor = libc::pollfd {
        fd: libc::STDOUT_FILENO,
        events: libc::POLLOUT,
        revents: 0,
    };
    while !output.is_empty() && !STOPPED.load(Ordering::Relaxed) {
        // Poll a full pipe so shutdown can drain the log without a stdout reader.
        // SAFETY: descriptor is one initialized pollfd, writable for the call.
        match unsafe { libc::poll(&mut descriptor, 1, 50) } {
            0 => continue,
            -1 => {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            _ => {}
        }
        if STOPPED.load(Ordering::Relaxed) {
            break;
        }
        // A writable Linux pipe has room for an atomic write of this size.
        let length = output.len().min(libc::PIPE_BUF);
        // SAFETY: output has at least length readable bytes for the syscall.
        match unsafe { libc::write(descriptor.fd, output.as_ptr().cast(), length) } {
            -1 => {
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => continue,
                    _ => return Err(error),
                }
            }
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "could not write stdout",
                ))
            }
            written => output = &output[written as usize..],
        }
    }
    Ok(())
}

fn monitor(args: &Args, log: Option<&mut SnapshotLog>) -> io::Result<()> {
    let mut collector = Collector::new();
    let terminal = io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && env::var("TERM").is_ok_and(|term| !term.is_empty() && term != "dumb");
    let utf8 = ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .filter_map(|key| env::var(key).ok())
        .find(|value| !value.is_empty())
        .is_some_and(|locale| {
            let locale = locale.to_ascii_lowercase();
            locale.contains("utf-8") || locale.contains("utf8")
        });
    let ascii = args.ascii || !utf8;
    let mut log = log;
    if terminal && !args.once && !args.json {
        let no_color = args.no_color || env::var_os("NO_COLOR").is_some();
        return tui::run(
            &mut collector,
            args.interval,
            ascii,
            no_color,
            args.compact,
            &STOPPED,
            |snapshot| match log.as_deref_mut() {
                Some(log) => record(log, json_line(snapshot)),
                None => Ok(()),
            },
        );
    }

    let count = if args.once || !args.json {
        1
    } else {
        args.count
    };
    let mut emitted = 0;
    collector.sample(); // Prime counters before measuring CPU utilization.
    while !STOPPED.load(Ordering::Relaxed) {
        wait(args.interval);
        if STOPPED.load(Ordering::Relaxed) {
            break;
        }
        let snapshot = collector.sample();
        let output = if args.json {
            let line = json_line(&snapshot);
            if let Some(log) = log.as_deref_mut() {
                record(log, line.clone())?;
            }
            line
        } else {
            if let Some(log) = log.as_deref_mut() {
                record(log, json_line(&snapshot))?;
            }
            let width = crossterm::terminal::size().map_or(100, |(width, _)| width.max(38));
            let mut output = tui::render_once(&snapshot, width, ascii, args.compact);
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output
        };
        match write_stdout(output.as_bytes()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
            Err(error) => return Err(error),
        }
        emitted += 1;
        if count != 0 && emitted == count {
            break;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args = Args::parse();
    if args.count != 0 && !args.json {
        Args::command()
            .error(ErrorKind::ArgumentConflict, "--count requires --json")
            .exit();
    }
    if let Err(error) = handle_signals() {
        eprintln!("mainsense: could not configure signals: {error}");
        return ExitCode::FAILURE;
    }
    let mut log = args
        .log_file
        .as_deref()
        .map(SnapshotLog::open)
        .transpose()
        .unwrap_or_else(|error| {
            Args::command()
                .error(
                    ErrorKind::Io,
                    format!(
                        "could not open log file {:?}: {error}",
                        args.log_file.as_deref().unwrap()
                    ),
                )
                .exit()
        });
    let result = monitor(&args, log.as_mut()).or_else(|error| {
        if error.kind() == io::ErrorKind::Interrupted && STOPPED.load(Ordering::Relaxed) {
            Ok(())
        } else {
            Err(error)
        }
    });
    let drained = log.map(SnapshotLog::finish).transpose().map_err(log_error);
    if let Err(error) = result.and(drained) {
        eprintln!("mainsense: {error}");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
