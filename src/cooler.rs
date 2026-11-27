//! The Raspberry Pi Active Cooler, for manual control only.
//!
//! Normally the kernel drives it: the `cpu-thermal` zone's `step_wise` governor
//! sets the `pwm-fan` cooling device's level from the CPU trip points, checking
//! every second. A level written by hand is taken back within seconds, so a
//! manual level **pauses the zone** (`mode = disabled`) and sets the level;
//! [`release`](Cooler::release) re-enables the zone and the kernel takes over
//! again within about a second. Firmware throttling (~85 °C) does not depend on
//! the zone and keeps working while it is paused.
//!
//! `fan_ctl` releases the zone whenever manual control ends, when the CPU reaches
//! `cooler_safety`, on a sensor error, on every start, and on every stop
//! (`fan_ctl --full` in `ExecStopPost`).

use std::path::{Path, PathBuf};

use crate::sysfs::{self, SysfsError};

/// Thermal zone `type` of the Pi's CPU sensor.
pub const ZONE_TYPE: &str = "cpu-thermal";
/// Cooling device `type` of the Active Cooler.
pub const CDEV_TYPE: &str = "pwm-fan";
/// hwmon chip name of the Active Cooler (for its tachometer).
pub const HWMON_NAME: &str = "pwmfan";

/// Something with a few speed levels that the kernel normally controls.
pub trait Cooler {
    /// Highest level.
    fn max(&self) -> u8;
    /// Current level, if readable.
    fn level(&mut self) -> Option<u8>;
    /// Current speed in rpm, if it has a tachometer.
    fn rpm(&mut self) -> Option<u32>;
    /// Takes control from the kernel and holds `level` (clamped to [`max`](Self::max)).
    /// Cheap to repeat: only writes what differs.
    ///
    /// # Errors
    ///
    /// Returns [`SysfsError`] if a write fails.
    fn hold(&mut self, level: u8) -> Result<(), SysfsError>;
    /// Gives control back to the kernel. Cheap to repeat: writes only if paused.
    ///
    /// # Errors
    ///
    /// Returns [`SysfsError`] if the write fails.
    fn release(&mut self) -> Result<(), SysfsError>;
}

/// The Active Cooler through `/sys/class/thermal` (and hwmon for rpm).
#[derive(Clone, Debug)]
pub struct ThermalCooler {
    zone: PathBuf,
    cdev: PathBuf,
    tach: Option<PathBuf>,
    max: u8,
}

impl ThermalCooler {
    /// Finds the `cpu-thermal` zone and the `pwm-fan` cooling device under `sysfs_root`.
    /// `None` if either is missing (no Active Cooler).
    #[must_use]
    pub fn find(sysfs_root: &Path) -> Option<Self> {
        let thermal = sysfs::list_dir(&sysfs_root.join("class/thermal")).ok()?;
        let typed = |prefix: &str, ty: &str| {
            thermal.iter().find(|p| {
                p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(prefix))
                    && sysfs::read_trimmed(&p.join("type")).is_ok_and(|t| t == ty)
            })
        };
        let zone = typed("thermal_zone", ZONE_TYPE)?.clone();
        let cdev = typed("cooling_device", CDEV_TYPE)?.clone();
        let max = sysfs::read_trimmed(&cdev.join("max_state")).ok()?.parse().ok()?;
        let tach = sysfs::list_dir(&sysfs_root.join("class/hwmon")).ok().and_then(|dirs| {
            dirs.into_iter()
                .find(|d| sysfs::read_trimmed(&d.join("name")).is_ok_and(|n| n == HWMON_NAME))
                .map(|d| d.join("fan1_input"))
        });
        Some(Self { zone, cdev, tach, max })
    }

    /// The thermal zone directory (for logs).
    #[must_use]
    pub fn zone(&self) -> &Path {
        &self.zone
    }

    fn paused(&self) -> bool {
        sysfs::read_trimmed(&self.zone.join("mode")).is_ok_and(|m| m == "disabled")
    }
}

impl Cooler for ThermalCooler {
    fn max(&self) -> u8 {
        self.max
    }

    fn level(&mut self) -> Option<u8> {
        sysfs::read_trimmed(&self.cdev.join("cur_state")).ok()?.parse().ok()
    }

    fn rpm(&mut self) -> Option<u32> {
        sysfs::read_trimmed(self.tach.as_ref()?).ok()?.parse().ok()
    }

    fn hold(&mut self, level: u8) -> Result<(), SysfsError> {
        let level = level.min(self.max);
        if !self.paused() {
            sysfs::write(&self.zone.join("mode"), "disabled")?;
        }
        if self.level() != Some(level) {
            sysfs::write(&self.cdev.join("cur_state"), level)?;
        }
        Ok(())
    }

    fn release(&mut self) -> Result<(), SysfsError> {
        if self.paused() {
            sysfs::write(&self.zone.join("mode"), "enabled")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakeSys;

    /// The layout on a Pi 5 with the Active Cooler.
    fn pi5(sys: &FakeSys) {
        sys.file("class/thermal/thermal_zone0/type", "cpu-thermal\n");
        sys.file("class/thermal/thermal_zone0/mode", "enabled\n");
        sys.file("class/thermal/cooling_device0/type", "pwm-fan\n");
        sys.file("class/thermal/cooling_device0/max_state", "4\n");
        sys.file("class/thermal/cooling_device0/cur_state", "0\n");
        sys.file("class/hwmon/hwmon2/name", "cpu_thermal\n");
        sys.file("class/hwmon/hwmon3/name", "pwmfan\n");
        sys.file("class/hwmon/hwmon3/fan1_input", "4248\n");
    }

    #[test]
    fn finds_zone_device_and_tachometer() {
        let sys = FakeSys::new();
        pi5(&sys);
        let mut c = ThermalCooler::find(sys.path()).unwrap();
        assert_eq!(c.max(), 4);
        assert_eq!(c.level(), Some(0));
        assert_eq!(c.rpm(), Some(4248));
        assert!(c.zone().ends_with("thermal_zone0"));
    }

    #[test]
    fn missing_cooler_is_none() {
        let sys = FakeSys::new();
        sys.file("class/thermal/thermal_zone0/type", "cpu-thermal\n");
        assert!(ThermalCooler::find(sys.path()).is_none());
        assert!(ThermalCooler::find(&sys.path().join("nope")).is_none());
    }

    #[test]
    fn hold_pauses_zone_and_sets_clamped_level() {
        let sys = FakeSys::new();
        pi5(&sys);
        let mut c = ThermalCooler::find(sys.path()).unwrap();
        c.hold(2).unwrap();
        assert_eq!(sys.read("class/thermal/thermal_zone0/mode"), "disabled");
        assert_eq!(sys.read("class/thermal/cooling_device0/cur_state"), "2");
        c.hold(9).unwrap();
        assert_eq!(sys.read("class/thermal/cooling_device0/cur_state"), "4", "clamped to max");
    }

    #[test]
    fn hold_writes_only_what_differs() {
        let sys = FakeSys::new();
        pi5(&sys);
        sys.file("class/thermal/thermal_zone0/mode", "disabled\n");
        sys.file("class/thermal/cooling_device0/cur_state", "2\n");
        let mut c = ThermalCooler::find(sys.path()).unwrap();
        c.hold(2).unwrap();
        assert_eq!(sys.read("class/thermal/thermal_zone0/mode"), "disabled\n", "untouched");
        assert_eq!(sys.read("class/thermal/cooling_device0/cur_state"), "2\n", "untouched");
    }

    #[test]
    fn release_enables_only_a_paused_zone() {
        let sys = FakeSys::new();
        pi5(&sys);
        let mut c = ThermalCooler::find(sys.path()).unwrap();
        c.release().unwrap();
        assert_eq!(sys.read("class/thermal/thermal_zone0/mode"), "enabled\n", "untouched");
        c.hold(1).unwrap();
        c.release().unwrap();
        assert_eq!(sys.read("class/thermal/thermal_zone0/mode"), "enabled");
    }
}
