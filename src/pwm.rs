//! Driving a PWM channel through the kernel sysfs PWM interface.
//!
//! On the Pi 5 the fan pin (GPIO27) has no hardware PWM, so the
//! `pwm-pio` overlay provides one using the RP1 PIO block. Its pwmchip
//! number depends on probe order, so the chip is found by its `device` link
//! (`…/pwm_pio@…`) instead of by number.

use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use crate::duty::Duty;
use crate::sysfs::{self, SysfsError};

/// `device` link fragment identifying the RP1 PIO PWM chip.
pub const PIO_DEVICE: &str = "pwm_pio";

/// How long to wait for the kernel to create `pwmN/` after an export.
const EXPORT_TIMEOUT: Duration = Duration::from_millis(500);
const EXPORT_POLL: Duration = Duration::from_millis(10);

/// Why the PWM channel could not be found or driven.
#[derive(Debug)]
pub enum PwmError {
    /// No pwmchip's `device` link contains the requested fragment.
    ChipNotFound {
        /// The fragment searched for (e.g. `pwm_pio`).
        device: String,
    },
    /// A sysfs access failed.
    Sysfs(SysfsError),
    /// The channel directory never appeared after exporting it.
    ExportTimeout {
        /// The directory waited for.
        path: PathBuf,
    },
    /// A zero period is not a valid PWM configuration.
    ZeroPeriod,
}

impl fmt::Display for PwmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChipNotFound { device } => write!(
                f,
                "no PWM chip for device {device:?} (is `dtoverlay=pwm-pio,gpio=…` in config.txt?)"
            ),
            Self::Sysfs(e) => e.fmt(f),
            Self::ExportTimeout { path } => {
                write!(f, "{} did not appear after export", path.display())
            }
            Self::ZeroPeriod => f.write_str("PWM period must be greater than zero"),
        }
    }
}

impl Error for PwmError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sysfs(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SysfsError> for PwmError {
    fn from(e: SysfsError) -> Self {
        Self::Sysfs(e)
    }
}

/// Finds the `pwmchipN` directory whose `device` link contains `device`.
///
/// # Errors
///
/// Returns [`PwmError::ChipNotFound`] if none matches, or [`PwmError::Sysfs`]
/// if `<sysfs>/class/pwm` cannot be listed.
pub fn find_chip(sysfs_root: &Path, device: &str) -> Result<PathBuf, PwmError> {
    sysfs::list_dir(&sysfs_root.join("class/pwm"))?
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("pwmchip")))
        .find(|chip| {
            sysfs::read_link(&chip.join("device"))
                .is_ok_and(|target| target.to_string_lossy().contains(device))
        })
        .ok_or_else(|| PwmError::ChipNotFound { device: device.to_owned() })
}

/// An exported, enabled PWM channel with a fixed period.
#[derive(Debug)]
pub struct PwmChannel {
    dir: PathBuf,
    period_ns: u32,
}

impl PwmChannel {
    /// Exports `channel` on `chip` if needed, sets `period_ns`, and enables output.
    ///
    /// The current duty is kept where possible, so opening never stops a running fan.
    ///
    /// # Errors
    ///
    /// Returns [`PwmError`] if exporting or configuring the channel fails.
    pub fn open(chip: &Path, channel: u32, period_ns: u32) -> Result<Self, PwmError> {
        if period_ns == 0 {
            return Err(PwmError::ZeroPeriod);
        }
        let dir = chip.join(format!("pwm{channel}"));
        if !dir.join("enable").exists() {
            sysfs::write(&chip.join("export"), channel)?;
            wait_for(&dir.join("enable"))?;
        }
        let pwm = Self { dir, period_ns };
        pwm.configure()?;
        Ok(pwm)
    }

    /// Sets the duty cycle.
    ///
    /// # Errors
    ///
    /// Returns [`PwmError::Sysfs`] if the write fails.
    pub fn set(&self, duty: Duty) -> Result<(), PwmError> {
        sysfs::write(&self.dir.join("duty_cycle"), duty.on_time_ns(self.period_ns))?;
        Ok(())
    }

    /// The PWM period in nanoseconds.
    #[must_use]
    pub const fn period_ns(&self) -> u32 {
        self.period_ns
    }

    /// The channel's sysfs directory (`…/pwmchipN/pwmM`).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    fn configure(&self) -> Result<(), PwmError> {
        // The kernel rejects a period shorter than the current duty (EINVAL),
        // so shrink the duty first only when it would not fit.
        let duty_path = self.dir.join("duty_cycle");
        let current: u32 = sysfs::read_trimmed(&duty_path)?.parse().unwrap_or(u32::MAX);
        if current > self.period_ns {
            sysfs::write(&duty_path, self.period_ns)?;
        }
        sysfs::write(&self.dir.join("period"), self.period_ns)?;
        sysfs::write(&self.dir.join("enable"), 1)?;
        Ok(())
    }
}

fn wait_for(path: &Path) -> Result<(), PwmError> {
    let deadline = Instant::now() + EXPORT_TIMEOUT;
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(PwmError::ExportTimeout { path: path.parent().unwrap_or(path).into() });
        }
        thread::sleep(EXPORT_POLL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakeSys;

    const PERIOD: u32 = 40_000;

    /// The layout seen on the real Pi 5 with `dtoverlay=pwm-pio,gpio=27`.
    fn pi5(sys: &FakeSys) {
        sys.symlink(
            "class/pwm/pwmchip0/device",
            "../../../devices/platform/axi/1000120000.pcie/1f00098000.pwm",
        );
        sys.symlink(
            "class/pwm/pwmchip1/device",
            "../../../devices/platform/axi/1000120000.pcie/1f0009c000.pwm",
        );
        sys.symlink("class/pwm/pwmchip2/device", "../../../devices/platform/pwm_pio@1b");
        sys.file("class/pwm/pwmchip2/export", "");
    }

    fn exported(sys: &FakeSys, duty: u32, period: u32) {
        for (f, v) in [("duty_cycle", duty), ("period", period), ("enable", 0)] {
            sys.file(&format!("class/pwm/pwmchip2/pwm0/{f}"), &format!("{v}\n"));
        }
    }

    #[test]
    fn finds_pio_chip_by_device_link() {
        let sys = FakeSys::new();
        pi5(&sys);
        let chip = find_chip(sys.path(), PIO_DEVICE).unwrap();
        assert!(chip.ends_with("pwmchip2"));
    }

    #[test]
    fn missing_overlay_is_chip_not_found() {
        let sys = FakeSys::new();
        sys.symlink("class/pwm/pwmchip0/device", "../../../devices/platform/1f00098000.pwm");
        let err = find_chip(sys.path(), PIO_DEVICE).unwrap_err();
        assert!(matches!(err, PwmError::ChipNotFound { .. }));
        assert!(err.to_string().contains("dtoverlay=pwm-pio"));
    }

    #[test]
    fn open_configures_and_enables_without_stopping_fan() {
        let sys = FakeSys::new();
        pi5(&sys);
        exported(&sys, 40_000, 40_000); // running at 100 %
        let chip = find_chip(sys.path(), PIO_DEVICE).unwrap();
        let pwm = PwmChannel::open(&chip, 0, PERIOD).unwrap();
        assert_eq!(sys.read("class/pwm/pwmchip2/pwm0/period"), "40000");
        assert_eq!(sys.read("class/pwm/pwmchip2/pwm0/enable"), "1");
        assert_eq!(sys.read("class/pwm/pwmchip2/pwm0/duty_cycle"), "40000\n", "duty untouched");
        assert_eq!(sys.read("class/pwm/pwmchip2/export"), "", "no export needed");
        assert_eq!(pwm.period_ns(), PERIOD);
        assert!(pwm.path().ends_with("pwmchip2/pwm0"));
    }

    #[test]
    fn open_shrinks_duty_that_exceeds_new_period() {
        let sys = FakeSys::new();
        pi5(&sys);
        exported(&sys, 500_000, 1_000_000); // left at 1 kHz by the bench test
        let chip = find_chip(sys.path(), PIO_DEVICE).unwrap();
        PwmChannel::open(&chip, 0, PERIOD).unwrap();
        assert_eq!(sys.read("class/pwm/pwmchip2/pwm0/duty_cycle"), "40000", "clamped to 100 %");
        assert_eq!(sys.read("class/pwm/pwmchip2/pwm0/period"), "40000");
    }

    #[test]
    fn set_writes_on_time() {
        let sys = FakeSys::new();
        pi5(&sys);
        exported(&sys, 0, PERIOD);
        let chip = find_chip(sys.path(), PIO_DEVICE).unwrap();
        let pwm = PwmChannel::open(&chip, 0, PERIOD).unwrap();
        pwm.set(Duty::saturating(60)).unwrap();
        assert_eq!(sys.read("class/pwm/pwmchip2/pwm0/duty_cycle"), "24000");
        pwm.set(Duty::FULL).unwrap();
        assert_eq!(sys.read("class/pwm/pwmchip2/pwm0/duty_cycle"), "40000");
    }

    #[test]
    fn unexported_channel_is_exported_then_times_out_in_fake_tree() {
        // A fake tree cannot create pwm0/ in response to the export write,
        // which exercises both the export and the timeout path.
        let sys = FakeSys::new();
        pi5(&sys);
        let chip = find_chip(sys.path(), PIO_DEVICE).unwrap();
        let err = PwmChannel::open(&chip, 0, PERIOD).unwrap_err();
        assert_eq!(sys.read("class/pwm/pwmchip2/export"), "0");
        assert!(matches!(err, PwmError::ExportTimeout { ref path } if path.ends_with("pwm0")));
    }

    #[test]
    fn zero_period_rejected() {
        let sys = FakeSys::new();
        assert!(matches!(PwmChannel::open(sys.path(), 0, 0), Err(PwmError::ZeroPeriod)));
    }
}
