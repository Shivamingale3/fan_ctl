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
    self, Commands, Manual, NoCommands, OVERRIDE_FILE, Override, RunDir, STATUS_FILE, unix_now,
};
use fan_ctl::cooler::{Cooler, ThermalCooler};
use fan_ctl::daemon::{Daemon, DryRunFan, FanOutput, ThreadSleeper};
use fan_ctl::duty::Duty;
use fan_ctl::log::{self, Level};
use fan_ctl::pwm::{self, PwmChannel};
use fan_ctl::sensor::{CPU_CHIP, HwmonSensor, NVME_CHIP};

const USAGE: &str = "\
Usage: fan_ctl [OPTIONS]                      run the controller (what the service does)
       fan_ctl status                         print the running controller's status
       fan_ctl set [PCT] [--cooler N] [--for TIME]
                                              manual case fan duty (0-100 %) and/or
                                              Active Cooler level (0-4); TIME e.g.
                                              30m, 2h, forever (default 1h)
       fan_ctl auto [--case | --cooler]       end manual control (default: both)

Temperature-driven case fan controller for the Raspberry Pi 5.
The fan follows whichever of the CPU and NVMe temperatures needs more cooling.
Manual control never beats the safety rules: a sensor error, or a sensor at its
full temperature, runs the case fan at 100 %; a sensor error, or the CPU at
cooler_safety (75 °C), gives the Active Cooler back to the kernel. Manual
control ends when fan_ctl stops.

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
    Set { config: Option<PathBuf>, duty: Option<Duty>, cooler: Option<u8>, secs: Option<u64> },
    Auto { config: Option<PathBuf>, case: bool, cooler: bool },
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
    let (mut config, mut duty, mut cooler, mut secs) = (None, None, None, Some(DEFAULT_SET_SECS));
    let (mut only_case, mut only_cooler) = (false, false);
    while let Some(arg) = args.next() {
        match (cmd.as_str(), arg.as_str()) {
            (_, "-c" | "--config") => {
                config = Some(PathBuf::from(args.next().ok_or("--config needs a path")?));
            }
            (_, "-h" | "--help") => return Ok(Command::Help),
            ("set", "--for") => {
                secs = control::parse_duration(&args.next().ok_or("--for needs a time")?)?;
            }
            ("set", "--cooler") => {
                let v = args.next().ok_or("--cooler needs a level")?;
                cooler = Some(
                    v.parse()
                        .ok()
                        .filter(|&l| l <= control::MAX_COOLER_LEVEL)
                        .ok_or(format!("bad cooler level `{v}` (0-4)"))?,
                );
            }
            ("set", v) if duty.is_none() && !v.starts_with('-') => {
                let p = v.trim_end_matches('%');
                duty = Some(
                    p.parse().ok().and_then(Duty::new).ok_or(format!("bad duty `{v}` (0-100)"))?,
                );
            }
            ("auto", "--case") => only_case = true,
            ("auto", "--cooler") => only_cooler = true,
            (_, other) => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(match cmd.as_str() {
        "status" => Command::Status(config),
        "auto" => {
            let both = only_case == only_cooler;
            Command::Auto { config, case: both || only_case, cooler: both || only_cooler }
        }
        _ if duty.is_none() && cooler.is_none() => {
            return Err("set needs a duty (0-100) and/or --cooler LEVEL".into());
        }
        _ => Command::Set { config, duty, cooler, secs },
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
        Ok(Command::Set { config, duty, cooler, secs }) => {
            let until = secs.map(|s| unix_now().saturating_add(s));
            with_run_dir(config.as_ref(), |dir| {
                change(dir, |o| {
                    if let Some(value) = duty {
                        o.case = Some(Manual { value, until });
                    }
                    if let Some(value) = cooler {
                        o.cooler = Some(Manual { value, until });
                    }
                })
            })
        }
        Ok(Command::Auto { config, case, cooler }) => with_run_dir(config.as_ref(), |dir| {
            change(dir, |o| {
                if case {
                    o.case = None;
                }
                if cooler {
                    o.cooler = None;
                }
            })
        }),
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
        let d = Daemon::new(
            cpu,
            nvme,
            DryRunFan,
            ThreadSleeper,
            NoCommands,
            None::<ThermalCooler>,
            &cfg,
        );
        return drive(d, opts.mode);
    }
    let cooler = ThermalCooler::find(&cfg.sysfs_root);
    if let Some(c) = &cooler {
        log::info(format_args!("Active Cooler: {} (manual control available)", c.zone().display()));
    }

    let channel = pwm::find_chip(&cfg.sysfs_root, &cfg.pwm_device)
        .and_then(|chip| PwmChannel::open(&chip, cfg.pwm_channel, cfg.period_ns));
    match channel {
        Ok(pwm) => {
            log::info(format_args!("driving {}", pwm.path().display()));
            let commands = RunDir::new(&cfg.run_dir);
            drive(Daemon::new(cpu, nvme, pwm, ThreadSleeper, commands, cooler, &cfg), opts.mode)
        }
        Err(e) => {
            log::error(format_args!("pwm: {e}"));
            ExitCode::FAILURE
        }
    }
}

/// `--full`: give the Active Cooler back to the kernel, drive the fan to 100 %
/// and exit, without reading any sensor. Runs on every service stop.
fn full_speed(cfg: &Config, dry_run: bool) -> ExitCode {
    if !dry_run {
        if let Some(mut c) = ThermalCooler::find(&cfg.sysfs_root) {
            match c.release() {
                Ok(()) => log::info(format_args!("Active Cooler: kernel in control")),
                Err(e) => log::error(format_args!("Active Cooler: {e}")),
            }
        }
    }
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
            for (key, what) in [("until", "case fan"), ("cooler_until", "cooler")] {
                if let Some(u) = control::field(&text, key).and_then(|u| u.parse::<u64>().ok()) {
                    if u > now {
                        println!("# {what} manual ends in {}", human(u - now));
                    }
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

/// `fan_ctl set` / `fan_ctl auto`: edits the override file (keeping the other
/// fan's part), then waits up to 5 s for the controller to apply it and prints
/// the status.
fn change(dir: &Path, edit: impl FnOnce(&mut Override)) -> ExitCode {
    let path = dir.join(OVERRIDE_FILE);
    let mut o =
        fs::read_to_string(&path).ok().and_then(|t| Override::parse(&t).ok()).unwrap_or_default();
    edit(&mut o);
    let result = if o.is_empty() {
        match fs::remove_file(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    } else {
        control::write_atomic(dir, OVERRIDE_FILE, &o.render())
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
    let case = o.case.map(|m| m.value.percent().to_string());
    let cooler = o.cooler.map(|m| m.value.to_string());
    for _ in 0..50 {
        let text = fs::read_to_string(dir.join(STATUS_FILE)).unwrap_or_default();
        let has_cooler = control::field(&text, "cooler_mode").is_some();
        if cooler.is_some() && !has_cooler && !text.is_empty() {
            log::error(format_args!("no Active Cooler found; the cooler level is ignored"));
            return ExitCode::FAILURE;
        }
        if control::field(&text, "manual").map(str::to_owned) == case
            && (!has_cooler || control::field(&text, "cooler_manual").map(str::to_owned) == cooler)
        {
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

fn drive<F: FanOutput, K: Commands, L: Cooler>(
    mut daemon: Daemon<HwmonSensor, HwmonSensor, F, ThreadSleeper, K, L>,
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
        let pct = |p| Duty::new(p);
        let set = |duty, cooler, secs| Ok(Command::Set { config: None, duty, cooler, secs });
        assert_eq!(parse(&["status"]), Ok(Command::Status(None)));
        assert_eq!(parse(&["set", "75"]), set(pct(75), None, Some(3_600)));
        assert_eq!(parse(&["set", "0%", "--for", "30m"]), set(Some(Duty::OFF), None, Some(1_800)));
        assert_eq!(parse(&["set", "--for", "forever", "100"]), set(Some(Duty::FULL), None, None));
        assert_eq!(parse(&["set", "--cooler", "2"]), set(None, Some(2), Some(3_600)));
        assert_eq!(parse(&["set", "80", "--cooler", "0"]), set(pct(80), Some(0), Some(3_600)));
        assert_eq!(
            parse(&["set"]),
            Err("set needs a duty (0-100) and/or --cooler LEVEL".to_owned())
        );
        assert_eq!(parse(&["set", "101"]), Err("bad duty `101` (0-100)".to_owned()));
        assert_eq!(
            parse(&["set", "--cooler", "11"]),
            Err("bad cooler level `11` (0-4)".to_owned())
        );
        assert_eq!(parse(&["set", "50", "60"]), Err("unknown argument `60`".to_owned()));
        assert_eq!(parse(&["status", "--for", "1h"]), Err("unknown argument `--for`".to_owned()));
        assert!(parse(&["set", "50", "--for", "1w"]).is_err());
        let auto = |case, cooler| Ok(Command::Auto { config: None, case, cooler });
        assert_eq!(parse(&["auto"]), auto(true, true));
        assert_eq!(parse(&["auto", "--case"]), auto(true, false));
        assert_eq!(parse(&["auto", "--cooler"]), auto(false, true));
        assert_eq!(parse(&["auto", "--case", "--cooler"]), auto(true, true));
        assert_eq!(
            parse(&["auto", "-c", "/x"]),
            Ok(Command::Auto { config: Some(PathBuf::from("/x")), case: true, cooler: true })
        );
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
