//! Temperature sensors exposed through the kernel hwmon class.
//!
//! hwmon numbers (`hwmon0`, `hwmon1`, …) depend on probe order and can change
//! between boots or after an NVMe reset, so sensors are found by their chip
//! `name` (`cpu_thermal`, `nvme`) and found again whenever a read fails.

use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use crate::curve::Celsius;
use crate::sysfs::{self, SysfsError};

/// hwmon chip name of the Raspberry Pi SoC temperature sensor.
pub const CPU_CHIP: &str = "cpu_thermal";
/// hwmon chip name of an NVMe drive's composite temperature.
pub const NVME_CHIP: &str = "nvme";

/// Readings outside this range are treated as sensor faults, not temperatures.
const PLAUSIBLE: std::ops::RangeInclusive<Celsius> = -40.0..=150.0;

/// Why a temperature could not be read.
#[derive(Debug)]
pub enum SensorError {
    /// No hwmon device with this chip name exists (yet).
    NotFound {
        /// The chip name searched for.
        chip: String,
    },
    /// A sysfs read failed.
    Sysfs(SysfsError),
    /// The attribute did not contain an integer.
    Parse {
        /// The attribute read.
        path: PathBuf,
        /// What it contained.
        value: String,
    },
    /// The value parsed but is outside any real operating range.
    Implausible {
        /// The attribute read.
        path: PathBuf,
        /// The decoded temperature.
        celsius: Celsius,
    },
}

impl fmt::Display for SensorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { chip } => write!(f, "no hwmon sensor named {chip:?}"),
            Self::Sysfs(e) => e.fmt(f),
            Self::Parse { path, value } => {
                write!(f, "{}: expected millidegrees, got {value:?}", path.display())
            }
            Self::Implausible { path, celsius } => {
                write!(f, "{}: implausible temperature {celsius} °C", path.display())
            }
        }
    }
}

impl Error for SensorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sysfs(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SysfsError> for SensorError {
    fn from(e: SysfsError) -> Self {
        Self::Sysfs(e)
    }
}

/// One hwmon temperature input (`temp1_input`), located by chip name.
#[derive(Clone, Debug)]
pub struct HwmonSensor {
    sysfs: PathBuf,
    chip: String,
    input: Option<PathBuf>,
}

impl HwmonSensor {
    /// A sensor for hwmon chip `chip` under the sysfs mount `sysfs` (normally `/sys`).
    ///
    /// Nothing is touched until the first [`read`](Self::read).
    pub fn new(sysfs: impl Into<PathBuf>, chip: impl Into<String>) -> Self {
        Self { sysfs: sysfs.into(), chip: chip.into(), input: None }
    }

    /// The hwmon chip name this sensor looks for.
    #[must_use]
    pub fn chip(&self) -> &str {
        &self.chip
    }

    /// The `temp1_input` path currently in use, if one has been found.
    #[must_use]
    pub fn input(&self) -> Option<&Path> {
        self.input.as_deref()
    }

    /// Reads the temperature in °C.
    ///
    /// On any failure the cached path is dropped, so the next call searches again.
    ///
    /// # Errors
    ///
    /// Returns [`SensorError`] if the chip is missing or the reading is unusable.
    pub fn read(&mut self) -> Result<Celsius, SensorError> {
        let path = match &self.input {
            Some(p) => p.clone(),
            None => self.input.insert(self.discover()?).clone(),
        };
        let result = read_celsius(&path);
        if result.is_err() {
            self.input = None;
        }
        result
    }

    fn discover(&self) -> Result<PathBuf, SensorError> {
        let class = self.sysfs.join("class/hwmon");
        sysfs::list_dir(&class)?
            .into_iter()
            .find(|dir| sysfs::read_trimmed(&dir.join("name")).is_ok_and(|n| n == self.chip))
            .map(|dir| dir.join("temp1_input"))
            .ok_or_else(|| SensorError::NotFound { chip: self.chip.clone() })
    }
}

fn read_celsius(path: &Path) -> Result<Celsius, SensorError> {
    let raw = sysfs::read_trimmed(path)?;
    let milli: i32 = raw
        .parse()
        .map_err(|_| SensorError::Parse { path: path.to_path_buf(), value: raw.clone() })?;
    // |milli| for any real temperature is far below 2^24, where f32 is exact.
    #[allow(clippy::cast_precision_loss)]
    let celsius = milli as f32 / 1000.0;
    if PLAUSIBLE.contains(&celsius) {
        Ok(celsius)
    } else {
        Err(SensorError::Implausible { path: path.to_path_buf(), celsius })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakeSys;

    fn hwmon(sys: &FakeSys, n: u32, name: &str, milli: &str) {
        sys.file(&format!("class/hwmon/hwmon{n}/name"), &format!("{name}\n"));
        sys.file(&format!("class/hwmon/hwmon{n}/temp1_input"), &format!("{milli}\n"));
    }

    /// The layout seen on the real Pi 5 (2026-11-20).
    fn pi5(sys: &FakeSys) {
        hwmon(sys, 0, "cpu_thermal", "49600");
        hwmon(sys, 1, "nvme", "61850");
        hwmon(sys, 2, "rp1_adc", "0");
        hwmon(sys, 3, "pwmfan", "0");
    }

    #[test]
    fn finds_chips_by_name() {
        let sys = FakeSys::new();
        pi5(&sys);
        let mut cpu = HwmonSensor::new(sys.path(), CPU_CHIP);
        let mut nvme = HwmonSensor::new(sys.path(), NVME_CHIP);
        assert!((cpu.read().unwrap() - 49.6).abs() < 1e-4);
        assert!((nvme.read().unwrap() - 61.85).abs() < 1e-4);
        assert!(nvme.input().unwrap().ends_with("hwmon1/temp1_input"));
        assert_eq!(nvme.chip(), "nvme");
    }

    #[test]
    fn follows_renumbering() {
        let sys = FakeSys::new();
        pi5(&sys);
        let mut nvme = HwmonSensor::new(sys.path(), NVME_CHIP);
        nvme.read().unwrap();
        // NVMe reset: hwmon1 disappears, the drive comes back as hwmon7.
        sys.remove("class/hwmon/hwmon1");
        assert!(matches!(nvme.read(), Err(SensorError::Sysfs(_))));
        assert_eq!(nvme.input(), None);
        hwmon(&sys, 7, "nvme", "58850");
        assert!((nvme.read().unwrap() - 58.85).abs() < 1e-4);
        assert!(nvme.input().unwrap().ends_with("hwmon7/temp1_input"));
    }

    #[test]
    fn missing_chip_is_not_found() {
        let sys = FakeSys::new();
        hwmon(&sys, 0, "cpu_thermal", "40000");
        let err = HwmonSensor::new(sys.path(), NVME_CHIP).read().unwrap_err();
        assert!(matches!(err, SensorError::NotFound { ref chip } if chip == "nvme"));
        assert_eq!(err.to_string(), "no hwmon sensor named \"nvme\"");
    }

    #[test]
    fn missing_hwmon_class_is_sysfs_error() {
        let sys = FakeSys::new();
        let err = HwmonSensor::new(sys.path(), CPU_CHIP).read().unwrap_err();
        assert!(matches!(err, SensorError::Sysfs(_)));
        assert!(err.source().is_some());
    }

    #[test]
    fn garbage_is_parse_error() {
        let sys = FakeSys::new();
        hwmon(&sys, 0, "nvme", "hot");
        let err = HwmonSensor::new(sys.path(), NVME_CHIP).read().unwrap_err();
        assert!(matches!(err, SensorError::Parse { ref value, .. } if value == "hot"));
    }

    #[test]
    fn out_of_range_is_implausible() {
        let sys = FakeSys::new();
        // 0 K, as some drives report when the sensor is not ready.
        hwmon(&sys, 0, "nvme", "-273150");
        hwmon(&sys, 1, "cpu_thermal", "200000");
        assert!(matches!(
            HwmonSensor::new(sys.path(), NVME_CHIP).read(),
            Err(SensorError::Implausible { .. })
        ));
        assert!(matches!(
            HwmonSensor::new(sys.path(), CPU_CHIP).read(),
            Err(SensorError::Implausible { .. })
        ));
    }

    #[test]
    fn unreadable_name_files_are_skipped() {
        let sys = FakeSys::new();
        sys.file("class/hwmon/hwmon0/temp1_input", "1\n"); // no `name` file
        hwmon(&sys, 1, "nvme", "45000");
        assert!((HwmonSensor::new(sys.path(), NVME_CHIP).read().unwrap() - 45.0).abs() < 1e-4);
    }
}
