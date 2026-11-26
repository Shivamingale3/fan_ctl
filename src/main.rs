//! `fan_ctl` binary: command line, start-up, and exit codes.
//!
//! Exit codes: `0` success (`--once`, `--help`, `--version`), `1` hardware
//! failure, `2` usage or config error.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
    thread,
    time::Duration,
};

use fan_ctl::config::{self, Config};
use fan_ctl::control::{
    self, Commands, NoCommands, OVERRIDE_FILE, Override, RunDir, STATUS_FILE, unix_now,
};
use fan_ctl::daemon::{Daemon, DryRunFan, FanOutput, ThreadSleeper};
use fan_ctl::duty::Duty;
use fan_ctl::log::{self, Level};
use fan_ctl::pwm::{self, PwmChannel};
use fan_ctl::sensor::{CPU_CHIP, HwmonSensor, NVME_CHIP};

const USAGE: &str = "\
Usage: fan_ctl [OPTIONS]                 run the controller (what the service does)
       fan_ctl status                    print the running controller's status
       fan_ctl set <0-100> [--for TIME]  manual duty in %, TIME e.g. 30m, 2h, forever (default 1h)
       fan_ctl auto                      end manual control

Temperature-driven case fan controller for the Raspberry Pi 5.
The fan follows whichever of the CPU and NVMe temperatures needs more cooling.
Manual control never beats the safety rules: a sensor error, or a sensor at its
full temperature, still runs the fan at 100 %. It ends when fan_ctl stops.

Options:
  -c, --config <PATH>  config file (default: /etc/fan_ctl.conf, used if present)
  -n, --dry-run        read sensors and log decisions; never touch the PWM
  -1, --once           run a single control step and exit
  -f, --full           set the fan to 100% and exit (fail-safe, used on service stop)
  -v, --verbose        log every step, not only changes
  -h, --help           print this help
  -V, --version        print the version
";

const USAGE_ERROR: u8 = 2;

/// What to do after start-up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    /// Control the fan until stopped.
    #[default]
    Run,
    /// One control step, then exit (`--once`).
    Once,
    /// Fan to 100 %, then exit (`--full`).
    Full,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    config: Option<PathBuf>,
    mode: Mode,
    dry_run: bool,
    verbose: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run(Options),
    Help,
    Version,
    Status(Option<PathBuf>),
    Set { config: Option<PathBuf>, duty: Duty, secs: Option<u64> },
    Auto(Option<PathBuf>),
}

/// Default length of `fan_ctl set` without `--for`.
const DEFAULT_SET_SECS: u64 = 3_600;

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = args.into_iter().peekable();
    match args.peek().map(String::as_str) {
        Some("status" | "set" | "auto") => {}
        _ => return parse_run(args),
    }
    let cmd = args.next().unwrap_or_default();
    let (mut config, mut duty, mut secs) = (None, None, Some(DEFAULT_SET_SECS));
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => {
                config = Some(PathBuf::from(args.next().ok_or("--config needs a path")?));
            }
            "--for" if cmd == "set" => {
                secs = control::parse_duration(&args.next().ok_or("--for needs a time")?)?;
            }
            "-h" | "--help" => return Ok(Command::Help),
            v if cmd == "set" && duty.is_none() && !v.starts_with('-') => {
                let p = v.trim_end_matches('%');
                duty = Some(
                    p.parse().ok().and_then(Duty::new).ok_or(format!("bad duty `{v}` (0-100)"))?,
                );
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(match cmd.as_str() {
        "status" => Command::Status(config),
        "auto" => Command::Auto(config),
        _ => Command::Set { config, duty: duty.ok_or("set needs a duty (0-100)")?, secs },
    })
}

fn parse_run(args: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut opts = Options::default();
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => {
                let path = args.next().ok_or_else(|| format!("{arg} needs a path"))?;
                opts.config = Some(PathBuf::from(path));
            }
            "-n" | "--dry-run" => opts.dry_run = true,
            "-1" | "--once" => opts.mode = set_mode(opts.mode, Mode::Once)?,
            "-f" | "--full" => opts.mode = set_mode(opts.mode, Mode::Full)?,
            "-v" | "--verbose" => opts.verbose = true,
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(Command::Run(opts))
}

fn set_mode(current: Mode, wanted: Mode) -> Result<Mode, String> {
    if current == Mode::Run || current == wanted {
        Ok(wanted)
    } else {
        Err("--once and --full cannot be combined".to_owned())
    }
}

#[allow(clippy::print_stdout)] // --help and --version belong on stdout
fn main() -> ExitCode {
    match parse_args(env::args().skip(1)) {
        Ok(Command::Run(opts)) => run(&opts),
        Ok(Command::Help) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("fan_ctl {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Status(c)) => with_run_dir(c.as_ref(), status),
        Ok(Command::Set { config, duty, secs }) => {
            let o = Override { duty, until: secs.map(|s| unix_now().saturating_add(s)) };
            with_run_dir(config.as_ref(), |dir| set(dir, Some(o)))
        }
        Ok(Command::Auto(c)) => with_run_dir(c.as_ref(), |dir| set(dir, None)),
        Err(msg) => {
            eprint!("fan_ctl: {msg}\n\n{USAGE}");
            ExitCode::from(USAGE_ERROR)
        }
    }
}

fn run(opts: &Options) -> ExitCode {
    if opts.verbose {
        log::set_max_level(Level::Debug);
    }
    let (path, required) = match &opts.config {
        Some(p) => (p.clone(), true),
        None => (PathBuf::from(config::DEFAULT_PATH), false),
    };
    let cfg = match Config::load(&path, required) {
        Ok(c) => c,
        Err(e) => {
            log::error(format_args!("config {}: {e}", path.display()));
            return ExitCode::from(USAGE_ERROR);
        }
    };
    if opts.mode == Mode::Full {
        return full_speed(&cfg, opts.dry_run);
    }

    log::info(format_args!(
        "fan_ctl {} starting: nvme {}→{} °C, cpu {}→{} °C, min {}, {} Hz{}",
        env!("CARGO_PKG_VERSION"),
        cfg.nvme_curve.start(),
        cfg.nvme_curve.full(),
        cfg.cpu_curve.start(),
        cfg.cpu_curve.full(),
        cfg.settings.min(),
        1_000_000_000 / cfg.period_ns,
        if opts.dry_run { " (dry run)" } else { "" },
    ));

    let cpu = HwmonSensor::new(&cfg.sysfs_root, CPU_CHIP);
    let nvme = HwmonSensor::new(&cfg.sysfs_root, NVME_CHIP);
    if opts.dry_run {
        let d = Daemon::new(cpu, nvme, DryRunFan, ThreadSleeper, NoCommands, &cfg);
        return drive(d, opts.mode);
    }

    let channel = pwm::find_chip(&cfg.sysfs_root, &cfg.pwm_device)
        .and_then(|chip| PwmChannel::open(&chip, cfg.pwm_channel, cfg.period_ns));
    match channel {
        Ok(pwm) => {
            log::info(format_args!("driving {}", pwm.path().display()));
            let commands = RunDir::new(&cfg.run_dir);
            drive(Daemon::new(cpu, nvme, pwm, ThreadSleeper, commands, &cfg), opts.mode)
        }
        Err(e) => {
            log::error(format_args!("pwm: {e}"));
            ExitCode::FAILURE
        }
    }
}

/// `--full`: drive the fan to 100 % and exit, without reading any sensor.
fn full_speed(cfg: &Config, dry_run: bool) -> ExitCode {
    let result = if dry_run {
        DryRunFan.set(Duty::FULL)
    } else {
        pwm::find_chip(&cfg.sysfs_root, &cfg.pwm_device)
            .and_then(|chip| PwmChannel::open(&chip, cfg.pwm_channel, cfg.period_ns))
            .and_then(|pwm| pwm.set(Duty::FULL))
    };
    match result {
        Ok(()) => {
            log::info(format_args!("fan set to {}", Duty::FULL));
            ExitCode::SUCCESS
        }
        Err(e) => {
            log::error(format_args!("pwm: {e}"));
            ExitCode::FAILURE
        }
    }
}

/// Loads the config (for `run_dir`) and runs `f` on the runtime directory.
fn with_run_dir(config: Option<&PathBuf>, f: impl FnOnce(&Path) -> ExitCode) -> ExitCode {
    let (path, required) =
        config.map_or_else(|| (PathBuf::from(config::DEFAULT_PATH), false), |p| (p.clone(), true));
    match Config::load(&path, required) {
        Ok(cfg) => f(&cfg.run_dir),
        Err(e) => {
            log::error(format_args!("config {}: {e}", path.display()));
            ExitCode::from(USAGE_ERROR)
        }
    }
}

/// `fan_ctl status`: prints the status file, plus how long a manual duty has left.
#[allow(clippy::print_stdout)]
fn status(dir: &Path) -> ExitCode {
    match fs::read_to_string(dir.join(STATUS_FILE)) {
        Ok(text) => {
            print!("{text}");
            let now = unix_now();
            if let Some(u) = control::field(&text, "until").and_then(|u| u.parse::<u64>().ok()) {
                if u > now {
                    println!("# manual ends in {}", human(u - now));
                }
            }
            if let Some(age) = control::field(&text, "updated")
                .and_then(|u| u.parse::<u64>().ok())
                .map(|u| now.saturating_sub(u))
                .filter(|&a| a > 60)
            {
                println!("# warning: status is {age} s old; is fan_ctl running?");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            log::error(format_args!(
                "{}: {e} (is fan_ctl.service running?)",
                dir.join(STATUS_FILE).display()
            ));
            ExitCode::FAILURE
        }
    }
}

/// `fan_ctl set` / `fan_ctl auto`: writes or removes the override, then waits
/// up to 5 s for the controller to apply it and prints the status.
fn set(dir: &Path, o: Option<Override>) -> ExitCode {
    let result = match o {
        Some(o) => control::write_atomic(dir, OVERRIDE_FILE, &o.render()),
        None => match fs::remove_file(dir.join(OVERRIDE_FILE)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        },
    };
    if let Err(e) = result {
        let hint = if e.kind() == io::ErrorKind::PermissionDenied {
            "run as root (sudo)"
        } else {
            "is fan_ctl.service running?"
        };
        log::error(format_args!("{}: {e} ({hint})", dir.display()));
        return ExitCode::FAILURE;
    }
    let want = o.map(|o| o.duty.percent().to_string());
    for _ in 0..50 {
        let text = fs::read_to_string(dir.join(STATUS_FILE)).unwrap_or_default();
        if control::field(&text, "manual").map(str::to_owned) == want {
            return status(dir);
        }
        thread::sleep(Duration::from_millis(100));
    }
    log::warn(format_args!("written, but the controller has not applied it within 5 s"));
    ExitCode::FAILURE
}

/// `3725` → `1h 2m 5s`.
fn human(secs: u64) -> String {
    let (h, m, s) = (secs / 3_600, secs / 60 % 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m {s}s"),
        _ => format!("{h}h {m}m {s}s"),
    }
}

fn drive<F: FanOutput, K: Commands>(
    mut daemon: Daemon<HwmonSensor, HwmonSensor, F, ThreadSleeper, K>,
    mode: Mode,
) -> ExitCode {
    if mode == Mode::Once {
        return match daemon.tick() {
            Ok(tick) => {
                log::info(format_args!("next step would be in {:?}", tick.next_in));
                ExitCode::SUCCESS
            }
            Err(e) => {
                log::error(format_args!("pwm: {e}"));
                ExitCode::FAILURE
            }
        };
    }
    match daemon.run() {
        Ok(never) => match never {},
        Err(e) => {
            log::error(format_args!("pwm: {e}; exiting so systemd can restart fan_ctl"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn no_args_runs_with_defaults() {
        assert_eq!(parse(&[]), Ok(Command::Run(Options::default())));
    }

    #[test]
    fn all_flags() {
        let want = |mode| Options {
            config: Some(PathBuf::from("/tmp/f.conf")),
            mode,
            dry_run: true,
            verbose: true,
        };
        assert_eq!(
            parse(&["-c", "/tmp/f.conf", "-n", "-1", "-v"]),
            Ok(Command::Run(want(Mode::Once)))
        );
        assert_eq!(
            parse(&["--config", "/tmp/f.conf", "--dry-run", "--full", "--verbose"]),
            Ok(Command::Run(want(Mode::Full)))
        );
    }

    #[test]
    fn once_and_full_are_exclusive() {
        assert_eq!(parse(&["--once", "--once"]).map(|_| ()), Ok(()));
        assert_eq!(
            parse(&["--once", "--full"]),
            Err("--once and --full cannot be combined".to_owned())
        );
    }

    #[test]
    fn help_and_version_win() {
        assert_eq!(parse(&["-n", "--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-V"]), Ok(Command::Version));
    }

    #[test]
    fn control_commands() {
        let pct = |p| Duty::new(p).unwrap();
        assert_eq!(parse(&["status"]), Ok(Command::Status(None)));
        assert_eq!(parse(&["auto", "-c", "/x"]), Ok(Command::Auto(Some(PathBuf::from("/x")))));
        assert_eq!(
            parse(&["set", "75"]),
            Ok(Command::Set { config: None, duty: pct(75), secs: Some(3_600) })
        );
        assert_eq!(
            parse(&["set", "0%", "--for", "30m"]),
            Ok(Command::Set { config: None, duty: Duty::OFF, secs: Some(1_800) })
        );
        assert_eq!(
            parse(&["set", "--for", "forever", "100"]),
            Ok(Command::Set { config: None, duty: Duty::FULL, secs: None })
        );
        assert_eq!(parse(&["set"]), Err("set needs a duty (0-100)".to_owned()));
        assert_eq!(parse(&["set", "101"]), Err("bad duty `101` (0-100)".to_owned()));
        assert_eq!(parse(&["set", "50", "60"]), Err("unknown argument `60`".to_owned()));
        assert_eq!(parse(&["status", "--for", "1h"]), Err("unknown argument `--for`".to_owned()));
        assert!(parse(&["set", "50", "--for", "1w"]).is_err());
    }

    #[test]
    fn human_durations() {
        assert_eq!(human(5), "5s");
        assert_eq!(human(125), "2m 5s");
        assert_eq!(human(3_725), "1h 2m 5s");
    }

    #[test]
    fn errors() {
        assert_eq!(parse(&["--config"]), Err("--config needs a path".to_owned()));
        assert_eq!(parse(&["--fast"]), Err("unknown argument `--fast`".to_owned()));
    }
}
