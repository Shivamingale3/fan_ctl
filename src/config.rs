//! Runtime configuration: tuned built-in defaults, optionally overridden by a file.
//!
//! The file has one `key = value` per line; `#` starts a comment. Unknown and
//! repeated keys are errors, so a typo never silently falls back to a default.
//!
//! | key | default | meaning |
//! |---|---|---|
//! | `pwm_device` | `pwm_pio` | fragment of the pwmchip `device` link |
//! | `pwm_channel` | `0` | channel on that chip |
//! | `pwm_frequency_hz` | `25000` | PWM frequency (lower ones whine on the tested fan) |
//! | `min_duty` | `60` | lowest duty in %, `0` allows stopping |
//! | `step` | `5` | duty granularity in % |
//! | `hysteresis` | `3.0` | °C drop needed before slowing down |
//! | `cpu_start` / `cpu_full` | `60` / `80` | CPU ramp, °C |
//! | `nvme_start` / `nvme_full` | `55` / `72` | NVMe ramp, °C |
//! | `kick_ms` | `2000` | full-speed kick when starting the fan (0..=10000) |
//! | `interval_idle_s` | `15` | poll interval while at `min_duty` (1..=300) |
//! | `interval_active_s` | `5` | poll interval above `min_duty` (1..=300) |
//! | `sysfs_root` | `/sys` | for testing only |
//! | `run_dir` | `/run/fan_ctl` | override + status files; for testing only |
//!
//! Defaults come from the thermal model and bench test in `sim/RESULTS.md`.

use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use crate::control::DEFAULT_RUN_DIR;
use crate::controller::Settings;
use crate::curve::{Celsius, Curve};
use crate::duty::Duty;
use crate::pwm::PIO_DEVICE;
use crate::sysfs::DEFAULT_ROOT;

/// Where the daemon looks for its config file.
pub const DEFAULT_PATH: &str = "/etc/fan_ctl.conf";

const NS_PER_S: u32 = 1_000_000_000;
const MAX_FREQUENCY_HZ: u32 = 1_000_000;
const MAX_INTERVAL_S: u64 = 300;
const MAX_KICK_MS: u64 = 10_000;

// Evaluated at compile time: an invalid default fails the build, not the daemon.
#[allow(clippy::panic)]
const DEFAULT_CPU_CURVE: Curve = match Curve::new(60.0, 80.0) {
    Ok(c) => c,
    Err(_) => panic!("invalid default CPU curve"),
};
#[allow(clippy::panic)]
const DEFAULT_NVME_CURVE: Curve = match Curve::new(55.0, 72.0) {
    Ok(c) => c,
    Err(_) => panic!("invalid default NVMe curve"),
};
#[allow(clippy::panic)]
const DEFAULT_SETTINGS: Settings = match Settings::new(Duty::saturating(60), 5, 3.0) {
    Ok(s) => s,
    Err(_) => panic!("invalid default controller settings"),
};

/// Everything the daemon needs to run.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Where sysfs is mounted.
    pub sysfs_root: PathBuf,
    /// Runtime directory with the override and status files.
    pub run_dir: PathBuf,
    /// Fragment of the pwmchip `device` link to look for.
    pub pwm_device: String,
    /// PWM channel on that chip.
    pub pwm_channel: u32,
    /// PWM period in nanoseconds (from `pwm_frequency_hz`).
    pub period_ns: u32,
    /// CPU temperature ramp.
    pub cpu_curve: Curve,
    /// NVMe temperature ramp.
    pub nvme_curve: Curve,
    /// Minimum duty, step and hysteresis.
    pub settings: Settings,
    /// How long to run at full speed when starting the fan.
    pub kick: Duration,
    /// Poll interval while the fan is at its minimum duty.
    pub interval_idle: Duration,
    /// Poll interval while the fan is above its minimum duty.
    pub interval_active: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sysfs_root: PathBuf::from(DEFAULT_ROOT),
            run_dir: PathBuf::from(DEFAULT_RUN_DIR),
            pwm_device: PIO_DEVICE.to_owned(),
            pwm_channel: 0,
            period_ns: NS_PER_S / 25_000,
            cpu_curve: DEFAULT_CPU_CURVE,
            nvme_curve: DEFAULT_NVME_CURVE,
            settings: DEFAULT_SETTINGS,
            kick: Duration::from_millis(2_000),
            interval_idle: Duration::from_secs(15),
            interval_active: Duration::from_secs(5),
        }
    }
}

/// Why the configuration could not be loaded.
#[derive(Debug)]
pub enum ConfigError {
    /// The file could not be read.
    Io {
        /// The config file.
        path: PathBuf,
        /// The read error.
        source: io::Error,
    },
    /// A non-comment line without `=`.
    Syntax {
        /// 1-based line number.
        line: usize,
    },
    /// A key this version does not know.
    UnknownKey {
        /// 1-based line number.
        line: usize,
        /// The key.
        key: String,
    },
    /// A key given twice.
    DuplicateKey {
        /// 1-based line number of the second occurrence.
        line: usize,
        /// The key.
        key: String,
    },
    /// A value of the wrong type or out of range.
    BadValue {
        /// 1-based line number.
        line: usize,
        /// The key.
        key: String,
        /// The value given.
        value: String,
        /// What was expected.
        expected: &'static str,
    },
    /// Values that are fine alone but not together (e.g. `nvme_start >= nvme_full`).
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Syntax { line } => write!(f, "line {line}: expected `key = value`"),
            Self::UnknownKey { line, key } => write!(f, "line {line}: unknown key `{key}`"),
            Self::DuplicateKey { line, key } => write!(f, "line {line}: `{key}` set twice"),
            Self::BadValue { line, key, value, expected } => {
                write!(f, "line {line}: `{key} = {value}`: expected {expected}")
            }
            Self::Invalid(msg) => f.write_str(msg),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Config {
    /// Loads `path`. A missing file yields the defaults unless `required`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the file is unreadable or invalid.
    pub fn load(path: &Path, required: bool) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) if !required && e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Io { path: path.to_path_buf(), source }),
        }
    }

    /// Parses config text on top of the defaults.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] for syntax errors, unknown or repeated keys, and
    /// invalid values.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut cfg = Self::default();
        let (mut cpu, mut nvme) = (
            (cfg.cpu_curve.start(), cfg.cpu_curve.full()),
            (cfg.nvme_curve.start(), cfg.nvme_curve.full()),
        );
        let s = cfg.settings;
        let (mut min, mut step, mut hysteresis) = (s.min(), s.step(), s.hysteresis());
        let mut seen: Vec<&str> = Vec::new();

        for (i, raw) in text.lines().enumerate() {
            let line = i + 1;
            let content = raw.split_once('#').map_or(raw, |(before, _)| before).trim();
            if content.is_empty() {
                continue;
            }
            let (key, value) = content.split_once('=').ok_or(ConfigError::Syntax { line })?;
            let (key, value) = (key.trim(), value.trim());
            if seen.contains(&key) {
                return Err(ConfigError::DuplicateKey { line, key: key.to_owned() });
            }
            seen.push(key);
            let v = Entry { line, key, value };

            match key {
                "sysfs_root" => cfg.sysfs_root = PathBuf::from(v.non_empty()?),
                "run_dir" => cfg.run_dir = PathBuf::from(v.non_empty()?),
                "pwm_device" => v.non_empty()?.clone_into(&mut cfg.pwm_device),
                "pwm_channel" => cfg.pwm_channel = v.parse("an integer >= 0")?,
                "pwm_frequency_hz" => {
                    let hz: u32 = v.parse("an integer in 1..=1000000")?;
                    if !(1..=MAX_FREQUENCY_HZ).contains(&hz) {
                        return Err(v.bad("an integer in 1..=1000000"));
                    }
                    cfg.period_ns = NS_PER_S / hz;
                }
                "min_duty" => {
                    min = Duty::new(v.parse("a percentage 0..=100")?)
                        .ok_or_else(|| v.bad("a percentage 0..=100"))?;
                }
                "step" => step = v.parse("a percentage 1..=50")?,
                "hysteresis" => hysteresis = v.parse("°C as a number")?,
                "cpu_start" => cpu.0 = v.parse("°C as a number")?,
                "cpu_full" => cpu.1 = v.parse("°C as a number")?,
                "nvme_start" => nvme.0 = v.parse("°C as a number")?,
                "nvme_full" => nvme.1 = v.parse("°C as a number")?,
                "kick_ms" => {
                    cfg.kick = Duration::from_millis(v.ranged(
                        0,
                        MAX_KICK_MS,
                        "milliseconds 0..=10000",
                    )?);
                }
                "interval_idle_s" => {
                    cfg.interval_idle =
                        Duration::from_secs(v.ranged(1, MAX_INTERVAL_S, "seconds 1..=300")?);
                }
                "interval_active_s" => {
                    cfg.interval_active =
                        Duration::from_secs(v.ranged(1, MAX_INTERVAL_S, "seconds 1..=300")?);
                }
                _ => return Err(ConfigError::UnknownKey { line, key: key.to_owned() }),
            }
        }

        cfg.cpu_curve = curve("cpu", cpu)?;
        cfg.nvme_curve = curve("nvme", nvme)?;
        cfg.settings = Settings::new(min, step, hysteresis)
            .map_err(|e| ConfigError::Invalid(e.to_string()))?;
        Ok(cfg)
    }
}

fn curve(name: &str, (start, full): (Celsius, Celsius)) -> Result<Curve, ConfigError> {
    Curve::new(start, full).map_err(|e| ConfigError::Invalid(format!("{name} curve: {e}")))
}

/// One `key = value` pair being parsed, for error reporting.
struct Entry<'a> {
    line: usize,
    key: &'a str,
    value: &'a str,
}

impl Entry<'_> {
    fn bad(&self, expected: &'static str) -> ConfigError {
        ConfigError::BadValue {
            line: self.line,
            key: self.key.to_owned(),
            value: self.value.to_owned(),
            expected,
        }
    }

    fn parse<T: FromStr>(&self, expected: &'static str) -> Result<T, ConfigError> {
        self.value.parse().map_err(|_| self.bad(expected))
    }

    fn ranged(&self, lo: u64, hi: u64, expected: &'static str) -> Result<u64, ConfigError> {
        let n: u64 = self.parse(expected)?;
        if (lo..=hi).contains(&n) { Ok(n) } else { Err(self.bad(expected)) }
    }

    fn non_empty(&self) -> Result<&str, ConfigError> {
        if self.value.is_empty() { Err(self.bad("a non-empty value")) } else { Ok(self.value) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakeSys;

    #[test]
    fn defaults_match_tuning() {
        let c = Config::default();
        assert_eq!(c.period_ns, 40_000);
        assert_eq!(c.settings.min(), Duty::saturating(60));
        assert_eq!(c.settings.step(), 5);
        assert!((c.nvme_curve.start() - 55.0).abs() < f32::EPSILON);
        assert!((c.nvme_curve.full() - 72.0).abs() < f32::EPSILON);
        assert!((c.cpu_curve.start() - 60.0).abs() < f32::EPSILON);
        assert_eq!(c.kick, Duration::from_secs(2));
        assert_eq!(c.pwm_device, "pwm_pio");
        assert_eq!(c.sysfs_root, Path::new("/sys"));
    }

    #[test]
    fn empty_and_comments_give_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(Config::parse("# hi\n\n   # indented\n").unwrap(), Config::default());
    }

    #[test]
    fn overrides_every_key() {
        let c = Config::parse(
            "sysfs_root = /tmp/sys\n\
             run_dir = /tmp/run\n\
             pwm_device = pwm_pio@1c\n\
             pwm_channel = 1\n\
             pwm_frequency_hz = 20000  # trailing comment\n\
             min_duty = 0\n\
             step = 10\n\
             hysteresis = 2.5\n\
             cpu_start = 65\ncpu_full = 85\n\
             nvme_start = 50\nnvme_full = 70\n\
             kick_ms = 500\n\
             interval_idle_s = 30\ninterval_active_s = 2\n",
        )
        .unwrap();
        assert_eq!(c.sysfs_root, Path::new("/tmp/sys"));
        assert_eq!(c.run_dir, Path::new("/tmp/run"));
        assert_eq!((c.pwm_device.as_str(), c.pwm_channel, c.period_ns), ("pwm_pio@1c", 1, 50_000));
        assert_eq!((c.settings.min(), c.settings.step()), (Duty::OFF, 10));
        assert!((c.settings.hysteresis() - 2.5).abs() < f32::EPSILON);
        assert_eq!(c.cpu_curve, Curve::new(65.0, 85.0).unwrap());
        assert_eq!(c.nvme_curve, Curve::new(50.0, 70.0).unwrap());
        assert_eq!(c.kick, Duration::from_millis(500));
        assert_eq!(c.interval_idle, Duration::from_secs(30));
        assert_eq!(c.interval_active, Duration::from_secs(2));
    }

    #[test]
    fn errors_point_at_the_line() {
        let err = |t| Config::parse(t).unwrap_err().to_string();
        assert_eq!(err("\n\nnope"), "line 3: expected `key = value`");
        assert_eq!(err("min_dutty = 50"), "line 1: unknown key `min_dutty`");
        assert_eq!(err("step = 5\nstep = 10"), "line 2: `step` set twice");
        assert_eq!(
            err("min_duty = 120"),
            "line 1: `min_duty = 120`: expected a percentage 0..=100"
        );
        assert_eq!(
            err("hysteresis = warm"),
            "line 1: `hysteresis = warm`: expected °C as a number"
        );
        assert!(err("pwm_frequency_hz = 0").starts_with("line 1: `pwm_frequency_hz = 0`"));
        assert_eq!(
            err("interval_idle_s = 0"),
            "line 1: `interval_idle_s = 0`: expected seconds 1..=300"
        );
        assert_eq!(
            err("kick_ms = 60000"),
            "line 1: `kick_ms = 60000`: expected milliseconds 0..=10000"
        );
        assert!(err("pwm_device =").contains("non-empty"));
    }

    #[test]
    fn cross_field_validation() {
        let err = |t| Config::parse(t).unwrap_err().to_string();
        assert_eq!(
            err("nvme_start = 80"),
            "nvme curve: curve start (80 °C) must be below full (72 °C)"
        );
        assert!(err("step = 0").contains("step must be 1..=50"));
        assert!(err("hysteresis = -1").contains("hysteresis"));
    }

    const SAMPLE: &str = include_str!("../packaging/fan_ctl.conf");

    #[test]
    fn sample_config_documents_the_defaults() {
        assert_eq!(Config::parse(SAMPLE).unwrap(), Config::default(), "sample as shipped");
        let uncommented: String = SAMPLE
            .lines()
            .map(|l| match l.strip_prefix('#') {
                Some(rest) if rest.contains(" = ") && !rest.starts_with(' ') => rest,
                _ => l,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let keys = uncommented.lines().filter(|l| l.contains(" = ") && !l.starts_with('#')).count();
        assert_eq!(keys, 13, "every key except sysfs_root and run_dir is documented");
        assert_eq!(Config::parse(&uncommented).unwrap(), Config::default(), "uncommented sample");
    }

    #[test]
    fn missing_file_is_default_unless_required() {
        let sys = FakeSys::new();
        let path = sys.path().join("fan_ctl.conf");
        assert_eq!(Config::load(&path, false).unwrap(), Config::default());
        assert!(matches!(Config::load(&path, true), Err(ConfigError::Io { .. })));
        sys.file("fan_ctl.conf", "min_duty = 70\n");
        assert_eq!(Config::load(&path, true).unwrap().settings.min(), Duty::saturating(70));
    }
}
