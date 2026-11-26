//! Manual override and live status, through two small files in the runtime
//! directory (`/run/fan_ctl`, created by systemd's `RuntimeDirectory=`).
//!
//! | file | written by | read by | content |
//! |---|---|---|---|
//! | `override` | root (`fan_ctl set`, other tools) | the daemon | see [`Override::parse`] |
//! | `status` | the daemon, every step | anyone | see [`Status::render`] |
//!
//! Two fans can be set, each with its own end time: the case fan (`duty`, %) and
//! the Active Cooler (`cooler`, a level, see [`crate::cooler`]).
//!
//! Rules, so an override can never make things less safe than the daemon allows:
//! - a missing, unreadable or invalid override file means **auto** for both;
//! - an expired part is removed and that fan goes back to auto;
//! - a sensor error means 100 % for the case fan and the kernel for the cooler;
//! - the case fan runs at 100 % while either sensor is at its `*_full`
//!   temperature, the cooler goes back to the kernel while the CPU is at
//!   `cooler_safety` (both released `hysteresis` °C below);
//! - the files are removed when the service stops (`--full`) and the override
//!   again when it starts, so a restart or reboot always starts in auto.
//!
//! Both files are replaced atomically (written to a temporary name, then renamed).

use std::{
    fmt::{self, Write as _},
    fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::controller::Readings;
use crate::curve::{Celsius, Curve};
use crate::duty::Duty;
use crate::log;

/// Where systemd creates the runtime directory for `fan_ctl.service`.
pub const DEFAULT_RUN_DIR: &str = "/run/fan_ctl";
/// The override file name inside the runtime directory.
pub const OVERRIDE_FILE: &str = "override";
/// The status file name inside the runtime directory.
pub const STATUS_FILE: &str = "status";
/// Highest cooler level accepted in an override (the Pi's has 4; higher is clamped).
pub const MAX_COOLER_LEVEL: u8 = 10;

/// Seconds since the Unix epoch, now.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A manual value for one fan, optionally until a point in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Manual<T> {
    /// The value to hold.
    pub value: T,
    /// Unix time (s) at which it ends; `None` = until the service stops.
    pub until: Option<u64>,
}

impl<T> Manual<T> {
    /// Whether it has ended at `now` (unix s).
    #[must_use]
    pub fn expired(&self, now: u64) -> bool {
        self.until.is_some_and(|u| now >= u)
    }
}

/// The manual settings for both fans. Empty = auto.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Override {
    /// Case fan duty.
    pub case: Option<Manual<Duty>>,
    /// Active Cooler level.
    pub cooler: Option<Manual<u8>>,
}

impl Override {
    /// Parses override file text, one `key = value` per line, `#` comments:
    ///
    /// ```text
    /// duty = 75                # case fan, 0..=100 %
    /// duty_until = 1764170000  # unix s; 0 or absent = no end
    /// cooler = 2               # Active Cooler level, 0..=10 (clamped to its max)
    /// cooler_until = 0
    /// ```
    ///
    /// An empty file is auto for both.
    ///
    /// # Errors
    ///
    /// Returns a message for syntax errors, unknown or repeated keys, bad values,
    /// or an `*_until` without its value.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut seen: Vec<&str> = Vec::new();
        let (mut duty, mut cooler, mut duty_until, mut cooler_until) = (None, None, None, None);
        for (i, raw) in text.lines().enumerate() {
            let line = raw.split_once('#').map_or(raw, |(before, _)| before).trim();
            if line.is_empty() {
                continue;
            }
            let n = i + 1;
            let (key, value) =
                line.split_once('=').ok_or_else(|| format!("line {n}: expected `key = value`"))?;
            let (key, value) = (key.trim(), value.trim());
            if seen.contains(&key) {
                return Err(format!("line {n}: `{key}` set twice"));
            }
            seen.push(key);
            let bad = || format!("line {n}: bad value `{key} = {value}`");
            let time = || value.parse::<u64>().map(|u| (u != 0).then_some(u)).map_err(|_| bad());
            match key {
                "duty" => duty = Some(value.parse().ok().and_then(Duty::new).ok_or_else(bad)?),
                "cooler" => {
                    cooler = Some(
                        value
                            .parse::<u8>()
                            .ok()
                            .filter(|&l| l <= MAX_COOLER_LEVEL)
                            .ok_or_else(bad)?,
                    );
                }
                "duty_until" => duty_until = Some(time()?),
                "cooler_until" => cooler_until = Some(time()?),
                _ => return Err(format!("line {n}: unknown key `{key}`")),
            }
        }
        if duty.is_none() && duty_until.is_some() {
            return Err("`duty_until` without `duty`".into());
        }
        if cooler.is_none() && cooler_until.is_some() {
            return Err("`cooler_until` without `cooler`".into());
        }
        Ok(Self {
            case: duty.map(|value| Manual { value, until: duty_until.flatten() }),
            cooler: cooler.map(|value| Manual { value, until: cooler_until.flatten() }),
        })
    }

    /// The file text for this override (what [`parse`](Self::parse) reads back).
    #[must_use]
    pub fn render(&self) -> String {
        let mut s = String::new();
        if let Some(m) = self.case {
            let _ =
                write!(s, "duty = {}\nduty_until = {}\n", m.value.percent(), m.until.unwrap_or(0));
        }
        if let Some(m) = self.cooler {
            let _ = write!(s, "cooler = {}\ncooler_until = {}\n", m.value, m.until.unwrap_or(0));
        }
        s
    }

    /// Neither fan is manual.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.case.is_none() && self.cooler.is_none()
    }

    /// Drops the parts that have ended at `now`; `true` if anything was dropped.
    pub fn expire(&mut self, now: u64) -> bool {
        let before = *self;
        if self.case.is_some_and(|m| m.expired(now)) {
            self.case = None;
        }
        if self.cooler.is_some_and(|m| m.expired(now)) {
            self.cooler = None;
        }
        *self != before
    }

    /// The earliest end time of either part.
    #[must_use]
    pub fn next_end(&self) -> Option<u64> {
        let a = self.case.and_then(|m| m.until);
        let b = self.cooler.and_then(|m| m.until);
        match (a, b) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// Who decided a fan's speed in the last step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The curves (case fan) or the kernel (cooler).
    Auto,
    /// A manual override.
    Manual,
    /// A manual override, overruled because a sensor reached its safety temperature.
    Safety,
    /// A sensor could not be read: case fan 100 %, cooler back to the kernel.
    FailSafe,
}

impl Mode {
    /// The word used in the status file and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
            Self::Safety => "safety",
            Self::FailSafe => "failsafe",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The Active Cooler's part of the status.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoolerStatus {
    /// Who decided its level.
    pub mode: Mode,
    /// Current level (`None` if unreadable).
    pub level: Option<u8>,
    /// Highest level.
    pub max: u8,
    /// Tachometer, rpm.
    pub rpm: Option<u32>,
    /// The override in force (also while overruled).
    pub manual: Option<Manual<u8>>,
    /// CPU °C at which manual control is overruled.
    pub safety: Celsius,
}

/// One step's state, as published in the status file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Status {
    /// Who decided `duty`.
    pub mode: Mode,
    /// The duty applied.
    pub duty: Duty,
    /// What the curves ask for.
    pub auto: Duty,
    /// The case fan override in force (also while overruled by safety).
    pub manual: Option<Manual<Duty>>,
    /// The temperatures read.
    pub readings: Readings,
    /// Lowest auto duty.
    pub min: Duty,
    /// CPU ramp.
    pub cpu_curve: Curve,
    /// NVMe ramp.
    pub nvme_curve: Curve,
    /// The Active Cooler, if there is one.
    pub cooler: Option<CoolerStatus>,
    /// Unix time (s) of this step.
    pub updated: u64,
}

impl Status {
    /// The status file text: one `key = value` per line, in this order.
    ///
    /// ```text
    /// version = 0.2.0
    /// mode = manual          # case fan: auto | manual | safety | failsafe
    /// duty = 75              # % applied now
    /// auto = 60              # % the curves ask for
    /// manual = 75            # only with a case fan override
    /// until = 1764170000     # only with a case fan override; 0 = no end
    /// cpu = 42.5             # °C, `?` if unreadable
    /// nvme = 57.8
    /// min_duty = 60
    /// cpu_curve = 60 80      # °C: start, full
    /// nvme_curve = 55 72
    /// cooler_mode = auto     # the rest only with an Active Cooler
    /// cooler = 0             # level now, `?` if unreadable
    /// cooler_max = 4
    /// cooler_rpm = 0         # `?` without a tachometer
    /// cooler_manual = 2      # only with a cooler override
    /// cooler_until = 0       # only with a cooler override; 0 = no end
    /// cooler_safety = 75     # CPU °C that overrules manual
    /// updated = 1764166400   # unix s
    /// ```
    #[must_use]
    pub fn render(&self) -> String {
        let temp = |t: Option<Celsius>| t.map_or_else(|| "?".to_owned(), |t| format!("{t:.1}"));
        let opt = |v: Option<String>| v.unwrap_or_else(|| "?".to_owned());
        let mut s = format!(
            "version = {}\nmode = {}\nduty = {}\nauto = {}\n",
            env!("CARGO_PKG_VERSION"),
            self.mode,
            self.duty.percent(),
            self.auto.percent(),
        );
        if let Some(m) = self.manual {
            let _ = write!(s, "manual = {}\nuntil = {}\n", m.value.percent(), m.until.unwrap_or(0));
        }
        let _ = write!(
            s,
            "cpu = {}\nnvme = {}\nmin_duty = {}\ncpu_curve = {} {}\nnvme_curve = {} {}\n",
            temp(self.readings.cpu),
            temp(self.readings.nvme),
            self.min.percent(),
            self.cpu_curve.start(),
            self.cpu_curve.full(),
            self.nvme_curve.start(),
            self.nvme_curve.full(),
        );
        if let Some(c) = self.cooler {
            let _ = write!(
                s,
                "cooler_mode = {}\ncooler = {}\ncooler_max = {}\ncooler_rpm = {}\n",
                c.mode,
                opt(c.level.map(|l| l.to_string())),
                c.max,
                opt(c.rpm.map(|r| r.to_string())),
            );
            if let Some(m) = c.manual {
                let _ = write!(
                    s,
                    "cooler_manual = {}\ncooler_until = {}\n",
                    m.value,
                    m.until.unwrap_or(0)
                );
            }
            let _ = writeln!(s, "cooler_safety = {}", c.safety);
        }
        let _ = writeln!(s, "updated = {}", self.updated);
        s
    }
}

/// Where the daemon gets overrides from and publishes its status.
pub trait Commands {
    /// Whether the override may have changed since the last [`load`](Self::load).
    /// Cheap (one `stat`): polled about once a second while the daemon waits.
    fn changed(&mut self) -> bool;
    /// The current override; empty = auto (no file, or an invalid one).
    fn load(&mut self) -> Override;
    /// Replaces the override (removes the file when `o` is empty), e.g. after expiry.
    fn store(&mut self, o: &Override);
    /// Publishes the status text. Failures are logged, never fatal.
    fn publish(&mut self, status: &str);
}

/// No overrides and no status (`--dry-run`).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCommands;

impl Commands for NoCommands {
    fn changed(&mut self) -> bool {
        false
    }
    fn load(&mut self) -> Override {
        Override::default()
    }
    fn store(&mut self, _: &Override) {}
    fn publish(&mut self, _: &str) {}
}

/// Identity of a file version: inode, size, mtime (ns).
type Stamp = Option<(u64, u64, i64, i64)>;

fn stamp(path: &Path) -> Stamp {
    fs::metadata(path).ok().map(|m| (m.ino(), m.size(), m.mtime(), m.mtime_nsec()))
}

/// The real runtime directory.
#[derive(Debug)]
pub struct RunDir {
    dir: PathBuf,
    seen: Option<Stamp>,
    load_error: Option<String>,
    publish_error: Option<String>,
}

impl RunDir {
    /// Uses `dir` (normally [`DEFAULT_RUN_DIR`]).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), seen: None, load_error: None, publish_error: None }
    }

    fn path(&self) -> PathBuf {
        self.dir.join(OVERRIDE_FILE)
    }
}

impl Commands for RunDir {
    fn changed(&mut self) -> bool {
        self.seen != Some(stamp(&self.path()))
    }

    fn load(&mut self) -> Override {
        let path = self.path();
        self.seen = Some(stamp(&path));
        let result = match fs::read_to_string(&path) {
            Ok(text) => Override::parse(&text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Override::default()),
            Err(e) => Err(e.to_string()),
        };
        match result {
            Ok(o) => {
                self.load_error = None;
                o
            }
            Err(msg) => {
                if self.load_error.as_deref() != Some(msg.as_str()) {
                    log::warn(format_args!("{}: {msg}; ignored (auto)", path.display()));
                    self.load_error = Some(msg);
                }
                Override::default()
            }
        }
    }

    fn store(&mut self, o: &Override) {
        let result = if o.is_empty() {
            match fs::remove_file(self.path()) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                r => r,
            }
        } else {
            write_atomic(&self.dir, OVERRIDE_FILE, &o.render())
        };
        if let Err(e) = result {
            log::warn(format_args!("{}: {e}", self.path().display()));
        }
        self.seen = Some(stamp(&self.path()));
    }

    fn publish(&mut self, status: &str) {
        match write_atomic(&self.dir, STATUS_FILE, status) {
            Ok(()) => self.publish_error = None,
            Err(e) => {
                let msg = e.to_string();
                if self.publish_error.as_deref() != Some(msg.as_str()) {
                    log::warn(format_args!("status in {}: {msg}", self.dir.display()));
                    self.publish_error = Some(msg);
                }
            }
        }
    }
}

/// Writes `dir/name` by writing `dir/.name.tmp` and renaming it, so readers
/// never see a half-written file. The file is world-readable (0644) whatever
/// the umask (the service runs with `UMask=0077`).
///
/// # Errors
///
/// Returns the I/O error if `dir` is missing or not writable.
pub fn write_atomic(dir: &Path, name: &str, text: &str) -> io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp"));
    fs::write(&tmp, text)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
    fs::rename(&tmp, dir.join(name))
}

/// Reads one `key = value` from status (or override) text.
#[must_use]
pub fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.split('#').next().unwrap_or("").trim())
    })
}

/// Parses `90s`, `30m`, `1h`, `2d` or plain seconds; `forever` = `None`.
///
/// # Errors
///
/// Returns a message for anything else, or for zero.
pub fn parse_duration(s: &str) -> Result<Option<u64>, String> {
    if s == "forever" {
        return Ok(None);
    }
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(format!("bad duration `{s}` (use e.g. 90s, 30m, 1h, forever)")),
    };
    match num.parse::<u64>() {
        Ok(n) if n > 0 => Ok(Some(n.saturating_mul(mult))),
        _ => Err(format!("bad duration `{s}` (use e.g. 90s, 30m, 1h, forever)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakeSys;

    fn pct(p: u8) -> Duty {
        Duty::new(p).unwrap()
    }

    fn both(until: Option<u64>) -> Override {
        Override {
            case: Some(Manual { value: pct(75), until }),
            cooler: Some(Manual { value: 2, until: None }),
        }
    }

    #[test]
    fn override_round_trips() {
        for o in [
            both(Some(1_764_170_000)),
            Override { case: Some(Manual { value: Duty::OFF, until: None }), cooler: None },
            Override { case: None, cooler: Some(Manual { value: 4, until: Some(9) }) },
            Override::default(),
        ] {
            assert_eq!(Override::parse(&o.render()), Ok(o), "{o:?}");
        }
        assert_eq!(
            Override::parse("duty=40 # quiet\n\n"),
            Ok(Override { case: Some(Manual { value: pct(40), until: None }), cooler: None })
        );
        assert!(Override::parse("").unwrap().is_empty());
    }

    #[test]
    fn override_errors() {
        let err = |t| Override::parse(t).unwrap_err();
        assert_eq!(err("duty_until = 5"), "`duty_until` without `duty`");
        assert_eq!(err("cooler_until = 5"), "`cooler_until` without `cooler`");
        assert_eq!(err("duty = 101"), "line 1: bad value `duty = 101`");
        assert_eq!(err("duty = -1"), "line 1: bad value `duty = -1`");
        assert_eq!(err("cooler = 11"), "line 1: bad value `cooler = 11`");
        assert_eq!(err("duty = 50\nduty_until = soon"), "line 2: bad value `duty_until = soon`");
        assert_eq!(err("duty = 50\nduty = 60"), "line 2: `duty` set twice");
        assert_eq!(err("speed = 50"), "line 1: unknown key `speed`");
        assert_eq!(err("50"), "line 1: expected `key = value`");
    }

    #[test]
    fn expiry_per_fan() {
        let mut o = Override {
            case: Some(Manual { value: pct(75), until: Some(100) }),
            cooler: Some(Manual { value: 2, until: Some(200) }),
        };
        assert_eq!(o.next_end(), Some(100));
        assert!(!o.expire(99));
        assert!(o.expire(100));
        assert_eq!((o.case, o.cooler.map(|m| m.value)), (None, Some(2)));
        assert_eq!(o.next_end(), Some(200));
        assert!(o.expire(500));
        assert!(o.is_empty());
        assert_eq!(both(None).next_end(), None);
        assert!(!both(None).expire(u64::MAX));
    }

    fn status(mode: Mode, manual: Option<Manual<Duty>>, cooler: Option<CoolerStatus>) -> Status {
        Status {
            mode,
            duty: pct(75),
            auto: pct(60),
            manual,
            readings: Readings { cpu: Some(42.46), nvme: None },
            min: pct(60),
            cpu_curve: Curve::new(60.0, 80.0).unwrap(),
            nvme_curve: Curve::new(55.0, 72.0).unwrap(),
            cooler,
            updated: 1_764_166_400,
        }
    }

    #[test]
    fn status_text() {
        let cooler = CoolerStatus {
            mode: Mode::Manual,
            level: Some(2),
            max: 4,
            rpm: None,
            manual: Some(Manual { value: 2, until: None }),
            safety: 75.0,
        };
        let s = status(Mode::Manual, Some(Manual { value: pct(75), until: Some(9) }), Some(cooler))
            .render();
        let want = format!(
            "version = {}\nmode = manual\nduty = 75\nauto = 60\nmanual = 75\nuntil = 9\n\
             cpu = 42.5\nnvme = ?\nmin_duty = 60\ncpu_curve = 60 80\nnvme_curve = 55 72\n\
             cooler_mode = manual\ncooler = 2\ncooler_max = 4\ncooler_rpm = ?\n\
             cooler_manual = 2\ncooler_until = 0\ncooler_safety = 75\nupdated = 1764166400\n",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(s, want);
        let auto = status(Mode::Auto, None, None).render();
        assert!(!auto.contains("manual") && !auto.contains("until") && !auto.contains("cooler"));
        assert_eq!(field(&auto, "mode"), Some("auto"));
        assert_eq!(field(&auto, "nvme_curve"), Some("55 72"));
        assert_eq!(field(&auto, "missing"), None);
    }

    #[test]
    fn run_dir_load_change_store_publish() {
        let sys = FakeSys::new();
        let mut d = RunDir::new(sys.path());
        assert!(d.changed(), "nothing loaded yet");
        assert!(d.load().is_empty());
        assert!(!d.changed());

        write_atomic(sys.path(), OVERRIDE_FILE, &both(None).render()).unwrap();
        assert!(d.changed());
        assert_eq!(d.load(), both(None));
        assert!(!d.changed());

        write_atomic(sys.path(), OVERRIDE_FILE, "duty = lots\n").unwrap();
        assert!(d.changed());
        assert!(d.load().is_empty(), "invalid = auto");

        let cooler_only = Override { case: None, cooler: both(None).cooler };
        d.store(&cooler_only);
        assert!(!d.changed(), "own write is not a change");
        assert_eq!(Override::parse(&sys.read(OVERRIDE_FILE)), Ok(cooler_only));
        d.store(&Override::default());
        assert!(!sys.path().join(OVERRIDE_FILE).exists());
        d.store(&Override::default());

        d.publish("mode = auto\n");
        assert_eq!(sys.read(STATUS_FILE), "mode = auto\n");
        let mode = fs::metadata(sys.path().join(STATUS_FILE)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644, "readable by everyone");
        assert!(!sys.path().join(".status.tmp").exists());
    }

    #[test]
    fn publish_to_missing_dir_is_not_fatal() {
        let sys = FakeSys::new();
        let mut d = RunDir::new(sys.path().join("gone"));
        d.publish("x");
        d.publish("x");
        assert!(d.load().is_empty());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s"), Ok(Some(90)));
        assert_eq!(parse_duration("45"), Ok(Some(45)));
        assert_eq!(parse_duration("30m"), Ok(Some(1_800)));
        assert_eq!(parse_duration("1h"), Ok(Some(3_600)));
        assert_eq!(parse_duration("2d"), Ok(Some(172_800)));
        assert_eq!(parse_duration("forever"), Ok(None));
        for bad in ["", "0", "0m", "1w", "h", "-5", "1.5h"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }
}
