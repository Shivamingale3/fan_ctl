//! Reading and writing sysfs attributes with errors that name the file.
//!
//! Every function takes a full path, so tests can point the crate at a fake
//! tree instead of [`DEFAULT_ROOT`].

use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

/// Where sysfs is mounted on a real system.
pub const DEFAULT_ROOT: &str = "/sys";

/// What was being done when a sysfs access failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Reading an attribute.
    Read,
    /// Writing an attribute.
    Write,
    /// Listing a directory.
    List,
    /// Resolving a symlink.
    ReadLink,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::List => "list",
            Self::ReadLink => "resolve link",
        })
    }
}

/// A failed sysfs access, with the path and the action that failed.
#[derive(Debug)]
pub struct SysfsError {
    path: PathBuf,
    action: Action,
    source: io::Error,
}

impl SysfsError {
    fn new(action: Action, path: &Path, source: io::Error) -> Self {
        Self { path: path.to_path_buf(), action, source }
    }

    /// The file or directory involved.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What was being attempted.
    #[must_use]
    pub const fn action(&self) -> Action {
        self.action
    }

    /// The underlying I/O error kind.
    #[must_use]
    pub fn kind(&self) -> io::ErrorKind {
        self.source.kind()
    }
}

impl fmt::Display for SysfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot {} {}: {}", self.action, self.path.display(), self.source)
    }
}

impl Error for SysfsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Reads an attribute and trims surrounding whitespace (sysfs ends values with `\n`).
///
/// # Errors
///
/// Returns [`SysfsError`] if the file cannot be read.
pub fn read_trimmed(path: &Path) -> Result<String, SysfsError> {
    fs::read_to_string(path)
        .map(|s| s.trim().to_owned())
        .map_err(|e| SysfsError::new(Action::Read, path, e))
}

/// Writes `value` to an attribute.
///
/// # Errors
///
/// Returns [`SysfsError`] if the write fails (e.g. `EINVAL` for an out-of-range value).
pub fn write(path: &Path, value: impl fmt::Display) -> Result<(), SysfsError> {
    fs::write(path, value.to_string()).map_err(|e| SysfsError::new(Action::Write, path, e))
}

/// Lists a directory's entries, sorted by path for deterministic discovery.
///
/// # Errors
///
/// Returns [`SysfsError`] if the directory cannot be read.
pub fn list_dir(path: &Path) -> Result<Vec<PathBuf>, SysfsError> {
    let err = |e| SysfsError::new(Action::List, path, e);
    let mut entries = fs::read_dir(path)
        .map_err(err)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    entries.sort();
    Ok(entries)
}

/// Reads the target of a symlink, such as a pwmchip's `device`.
///
/// # Errors
///
/// Returns [`SysfsError`] if `path` is not a readable symlink.
pub fn read_link(path: &Path) -> Result<PathBuf, SysfsError> {
    fs::read_link(path).map_err(|e| SysfsError::new(Action::ReadLink, path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakeSys;

    #[test]
    fn read_trims_newline() {
        let sys = FakeSys::new();
        sys.file("a/value", "42000\n");
        assert_eq!(read_trimmed(&sys.path().join("a/value")).unwrap(), "42000");
    }

    #[test]
    fn write_then_read_back() {
        let sys = FakeSys::new();
        sys.file("a/value", "");
        write(&sys.path().join("a/value"), 24_000).unwrap();
        assert_eq!(sys.read("a/value"), "24000");
    }

    #[test]
    fn error_names_path_and_action() {
        let sys = FakeSys::new();
        let missing = sys.path().join("nope");
        let err = read_trimmed(&missing).unwrap_err();
        assert_eq!(err.action(), Action::Read);
        assert_eq!(err.path(), missing);
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().starts_with(&format!("cannot read {}", missing.display())));
        assert!(err.source().is_some());
    }

    #[test]
    fn list_is_sorted() {
        let sys = FakeSys::new();
        for d in ["d/hwmon2/x", "d/hwmon0/x", "d/hwmon1/x"] {
            sys.file(d, "");
        }
        let names: Vec<_> = list_dir(&sys.path().join("d"))
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["hwmon0", "hwmon1", "hwmon2"]);
    }

    #[test]
    fn read_link_returns_target() {
        let sys = FakeSys::new();
        sys.symlink("chip/device", "../../devices/platform/pwm_pio@1b");
        let target = read_link(&sys.path().join("chip/device")).unwrap();
        assert!(target.ends_with("pwm_pio@1b"));
        assert_eq!(read_link(&sys.path().join("x")).unwrap_err().action(), Action::ReadLink);
    }
}
