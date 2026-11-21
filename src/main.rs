//! `fan_ctl` binary: command line, start-up, and exit codes.
//!
//! Exit codes: `0` success (`--once`, `--help`, `--version`), `1` hardware
//! failure, `2` usage or config error.

use std::{env, path::PathBuf, process::ExitCode};

use fan_ctl::config::{self, Config};
use fan_ctl::daemon::{Daemon, DryRunFan, FanOutput, ThreadSleeper};
use fan_ctl::duty::Duty;
use fan_ctl::log::{self, Level};
use fan_ctl::pwm::{self, PwmChannel};
use fan_ctl::sensor::{CPU_CHIP, HwmonSensor, NVME_CHIP};

const USAGE: &str = "\
Usage: fan_ctl [OPTIONS]

Temperature-driven case fan controller for the Raspberry Pi 5.
The fan follows whichever of the CPU and NVMe temperatures needs more cooling.

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
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut opts = Options::default();
    let mut args = args.into_iter();
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
        return drive(Daemon::new(cpu, nvme, DryRunFan, ThreadSleeper, &cfg), opts.mode);
    }

    let channel = pwm::find_chip(&cfg.sysfs_root, &cfg.pwm_device)
        .and_then(|chip| PwmChannel::open(&chip, cfg.pwm_channel, cfg.period_ns));
    match channel {
        Ok(pwm) => {
            log::info(format_args!("driving {}", pwm.path().display()));
            drive(Daemon::new(cpu, nvme, pwm, ThreadSleeper, &cfg), opts.mode)
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

fn drive<F: FanOutput>(
    mut daemon: Daemon<HwmonSensor, HwmonSensor, F, ThreadSleeper>,
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
    fn errors() {
        assert_eq!(parse(&["--config"]), Err("--config needs a path".to_owned()));
        assert_eq!(parse(&["--fast"]), Err("unknown argument `--fast`".to_owned()));
    }
}
