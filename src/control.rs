//! Manual override and live status, through two small files in the runtime
//! directory (`/run/fan_ctl`, created by systemd's `RuntimeDirectory=`).
//!
//! | file | written by | read by | content |
//! |---|---|---|---|
//! | `override` | root (`fan_ctl set`, other tools) | the daemon | `duty = 75` and optional `until = <unix s>` |
//! | `status` | the daemon, every step | anyone | `key = value` lines, see [`Status::render`] |
//!
//! Rules, so an override can never make the fan less safe than the daemon allows:
//! - a missing, unreadable or invalid override file means **auto**;
//! - an expired override is deleted and the fan goes back to auto;
//! - a sensor error still means 100 %, and so does either sensor reaching its
//!   `*_full` temperature (until it is `hysteresis` °C below it again);
//! - `/run` is a tmpfs and the directory is removed when the service stops, so
//!   a reboot or restart always starts in auto.
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

/// Seconds since the Unix epoch, now.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A manual duty, optionally until a point in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Override {
    /// The duty to run at.
    pub duty: Duty,
    /// Unix time (s) at which the override ends; `None` = until the service stops.
    pub until: Option<u64>,
}

impl Override {
    /// Parses override file text: `duty = 0..=100` (required), `until = <unix s>`
    /// (optional, `0` = no end). `#` starts a comment.
    ///
    /// # Errors
    ///
    /// Returns a message for syntax errors, unknown or repeated keys, bad values,
    /// or a missing `duty`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (mut duty, mut until) = (None, None);
        for (i, raw) in text.lines().enumerate() {
            let line = raw.split_once('#').map_or(raw, |(before, _)| before).trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("line {}: expected `key = value`", i + 1))?;
            let (key, value) = (key.trim(), value.trim());
            let bad = || format!("line {}: bad value `{key} = {value}`", i + 1);
            match key {
                "duty" if duty.is_none() => {
                    duty = Some(value.parse().ok().and_then(Duty::new).ok_or_else(bad)?);
                }
                "until" if until.is_none() => {
                    until = Some(value.parse::<u64>().map_err(|_| bad())?);
                }
                "duty" | "until" => return Err(format!("line {}: `{key}` set twice", i + 1)),
                _ => return Err(format!("line {}: unknown key `{key}`", i + 1)),
            }
        }
        let duty = duty.ok_or("missing `duty`")?;
        Ok(Self { duty, until: until.filter(|&u| u != 0) })
    }

    /// The file text for this override (what [`parse`](Self::parse) reads back).
    #[must_use]
    pub fn render(&self) -> String {
        format!("duty = {}\nuntil = {}\n", self.duty.percent(), self.until.unwrap_or(0))
    }

    /// Whether the override has ended at `now` (unix s).
    #[must_use]
    pub fn expired(&self, now: u64) -> bool {
        self.until.is_some_and(|u| now >= u)
    }
}

/// Who decided the duty in the last step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The temperature curves.
    Auto,
    /// A manual override.
    Manual,
    /// A manual override, overruled to 100 % because a sensor reached its full temperature.
    Safety,
    /// A sensor could not be read: 100 %.
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

/// One step's state, as published in the status file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Status {
    /// Who decided `duty`.
    pub mode: Mode,
    /// The duty applied.
    pub duty: Duty,
    /// What the curves ask for.
    pub auto: Duty,
    /// The override in force (also while overruled by safety).
    pub manual: Option<Override>,
    /// The temperatures read.
    pub readings: Readings,
    /// Lowest auto duty.
    pub min: Duty,
    /// CPU ramp.
    pub cpu_curve: Curve,
    /// NVMe ramp.
    pub nvme_curve: Curve,
    /// Unix time (s) of this step.
    pub updated: u64,
}

impl Status {
    /// The status file text: one `key = value` per line, in this order.
    ///
    /// ```text
    /// version = 0.2.0
    /// mode = manual          # auto | manual | safety | failsafe
    /// duty = 75              # % applied now
    /// auto = 60              # % the curves ask for
    /// manual = 75            # only with an override
    /// until = 1764170000     # only with an override; 0 = no end
    /// cpu = 42.5             # °C, `?` if unreadable
    /// nvme = 57.8
    /// min_duty = 60
    /// cpu_curve = 60 80      # °C: start, full
    /// nvme_curve = 55 72
    /// updated = 1764166400   # unix s
    /// ```
    #[must_use]
    pub fn render(&self) -> String {
        let temp = |t: Option<Celsius>| t.map_or_else(|| "?".to_owned(), |t| format!("{t:.1}"));
        let mut s = format!(
            "version = {}\nmode = {}\nduty = {}\nauto = {}\n",
            env!("CARGO_PKG_VERSION"),
            self.mode,
            self.duty.percent(),
            self.auto.percent(),
        );
        if let Some(o) = self.manual {
            let _ = write!(s, "manual = {}\nuntil = {}\n", o.duty.percent(), o.until.unwrap_or(0));
        }
        let _ = write!(
            s,
            "cpu = {}\nnvme = {}\nmin_duty = {}\ncpu_curve = {} {}\nnvme_curve = {} {}\nupdated = {}\n",
            temp(self.readings.cpu),
            temp(self.readings.nvme),
            self.min.percent(),
            self.cpu_curve.start(),
            self.cpu_curve.full(),
            self.nvme_curve.start(),
            self.nvme_curve.full(),
            self.updated,
        );
        s
    }
}

/// Where the daemon gets overrides from and publishes its status.
pub trait Commands {
    /// Whether the override may have changed since the last [`load`](Self::load).
    /// Cheap (one `stat`): polled about once a second while the daemon waits.
    fn changed(&mut self) -> bool;
    /// The current override; `None` = auto (no file, or an invalid one).
    fn load(&mut self) -> Option<Override>;
    /// Removes the override (it expired).
    fn clear(&mut self);
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
    fn load(&mut self) -> Option<Override> {
        None
    }
    fn clear(&mut self) {}
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

    fn load(&mut self) -> Option<Override> {
        let path = self.path();
        self.seen = Some(stamp(&path));
        let result = match fs::read_to_string(&path) {
            Ok(text) => Override::parse(&text).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
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
                None
            }
        }
    }

    fn clear(&mut self) {
        let _ = fs::remove_file(self.path());
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

    #[test]
    fn override_round_trips() {
        let o = Override { duty: pct(75), until: Some(1_764_170_000) };
        assert_eq!(Override::parse(&o.render()), Ok(o));
        let forever = Override { duty: Duty::OFF, until: None };
        assert_eq!(Override::parse(&forever.render()), Ok(forever));
        assert_eq!(
            Override::parse("duty=40 # quiet\n\n"),
            Ok(Override { duty: pct(40), until: None })
        );
    }

    #[test]
    fn override_errors() {
        let err = |t| Override::parse(t).unwrap_err();
        assert_eq!(err(""), "missing `duty`");
        assert_eq!(err("until = 5"), "missing `duty`");
        assert_eq!(err("duty = 101"), "line 1: bad value `duty = 101`");
        assert_eq!(err("duty = -1"), "line 1: bad value `duty = -1`");
        assert_eq!(err("duty = 50\nuntil = soon"), "line 2: bad value `until = soon`");
        assert_eq!(err("duty = 50\nduty = 60"), "line 2: `duty` set twice");
        assert_eq!(err("speed = 50"), "line 1: unknown key `speed`");
        assert_eq!(err("50"), "line 1: expected `key = value`");
    }

    #[test]
    fn expiry() {
        let o = Override { duty: pct(75), until: Some(100) };
        assert!(!o.expired(99));
        assert!(o.expired(100));
        assert!(!Override { duty: pct(75), until: None }.expired(u64::MAX));
    }

    fn status(mode: Mode, manual: Option<Override>) -> Status {
        Status {
            mode,
            duty: pct(75),
            auto: pct(60),
            manual,
            readings: Readings { cpu: Some(42.46), nvme: None },
            min: pct(60),
            cpu_curve: Curve::new(60.0, 80.0).unwrap(),
            nvme_curve: Curve::new(55.0, 72.0).unwrap(),
            updated: 1_764_166_400,
        }
    }

    #[test]
    fn status_text() {
        let s = status(Mode::Manual, Some(Override { duty: pct(75), until: Some(9) })).render();
        let want = format!(
            "version = {}\nmode = manual\nduty = 75\nauto = 60\nmanual = 75\nuntil = 9\n\
             cpu = 42.5\nnvme = ?\nmin_duty = 60\ncpu_curve = 60 80\nnvme_curve = 55 72\n\
             updated = 1764166400\n",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(s, want);
        let auto = status(Mode::Auto, None).render();
        assert!(!auto.contains("manual") && !auto.contains("until"));
        assert_eq!(field(&auto, "mode"), Some("auto"));
        assert_eq!(field(&auto, "nvme_curve"), Some("55 72"));
        assert_eq!(field(&auto, "missing"), None);
    }

    #[test]
    fn run_dir_load_change_clear_publish() {
        let sys = FakeSys::new();
        let mut d = RunDir::new(sys.path());
        assert!(d.changed(), "nothing loaded yet");
        assert_eq!(d.load(), None);
        assert!(!d.changed());

        write_atomic(sys.path(), OVERRIDE_FILE, "duty = 80\nuntil = 0\n").unwrap();
        assert!(d.changed());
        assert_eq!(d.load(), Some(Override { duty: pct(80), until: None }));
        assert!(!d.changed());

        write_atomic(sys.path(), OVERRIDE_FILE, "duty = lots\n").unwrap();
        assert!(d.changed());
        assert_eq!(d.load(), None, "invalid = auto");

        d.clear();
        assert!(!sys.path().join(OVERRIDE_FILE).exists());
        assert!(!d.changed());

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
        assert_eq!(d.load(), None);
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
