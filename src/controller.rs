//! Temperatures → fan duty.
//!
//! Rules, in order:
//! 1. **Fail-safe:** if any reading is missing or not finite, run at 100 %.
//! 2. **Hotter sensor wins:** demand is the higher of the CPU and NVMe ramps.
//! 3. **Usable range:** demand `0..=1` maps onto `min..=100 %`, because a 2-wire
//!    fan below its minimum barely moves air or stalls.
//! 4. **Steps:** the duty is rounded *up* to a multiple of `step`, so small
//!    temperature wobbles don't rewrite the PWM.
//! 5. **Hysteresis:** speeding up is immediate; slowing down only happens once
//!    the temperatures are `hysteresis` °C below what would justify the
//!    current speed, so the fan doesn't hunt between two steps.

use std::{error::Error, fmt};

use crate::curve::{Celsius, Curve};
use crate::duty::Duty;

/// Tuning that shapes how cooling demand becomes a duty.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    min: Duty,
    step: u8,
    hysteresis: Celsius,
}

/// Why [`Settings`] could not be built.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SettingsError {
    /// `step` must be within `1..=50` percent.
    StepOutOfRange(u8),
    /// `hysteresis` must be a finite number ≥ 0.
    BadHysteresis(Celsius),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StepOutOfRange(s) => write!(f, "step must be 1..=50 %, got {s}"),
            Self::BadHysteresis(h) => write!(f, "hysteresis must be finite and >= 0, got {h}"),
        }
    }
}

impl Error for SettingsError {}

impl Settings {
    /// Creates controller settings.
    ///
    /// - `min`: lowest duty the fan ever runs at (use [`Duty::OFF`] to allow stopping).
    /// - `step`: duty granularity in percent.
    /// - `hysteresis`: °C the temperature must fall before the fan slows down.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError`] if `step` is outside `1..=50` or `hysteresis`
    /// is negative or not finite.
    pub const fn new(min: Duty, step: u8, hysteresis: Celsius) -> Result<Self, SettingsError> {
        if step == 0 || step > 50 {
            return Err(SettingsError::StepOutOfRange(step));
        }
        if !hysteresis.is_finite() || hysteresis < 0.0 {
            return Err(SettingsError::BadHysteresis(hysteresis));
        }
        Ok(Self { min, step, hysteresis })
    }

    /// Lowest duty the fan runs at.
    #[must_use]
    pub const fn min(&self) -> Duty {
        self.min
    }

    /// Duty granularity in percent.
    #[must_use]
    pub const fn step(&self) -> u8 {
        self.step
    }

    /// Temperature drop (°C) needed before slowing down.
    #[must_use]
    pub const fn hysteresis(&self) -> Celsius {
        self.hysteresis
    }

    /// Maps a demand in `0.0..=1.0` onto `min..=100 %`, rounded up to `step`.
    fn duty_for(self, demand: f32) -> Duty {
        let min = f32::from(self.min.percent());
        let raw = min + (100.0 - min) * demand.clamp(0.0, 1.0);
        let step = f32::from(self.step);
        let rounded = ((raw / step).ceil() * step).clamp(0.0, 100.0);
        // `rounded` is a whole number within 0..=100, so the cast is exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Duty::saturating(rounded as u8)
    }
}

/// One sample of both temperatures. `None` means that sensor could not be read.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Readings {
    /// CPU (system-on-chip) temperature.
    pub cpu: Option<Celsius>,
    /// NVMe composite temperature.
    pub nvme: Option<Celsius>,
}

/// Turns temperature readings into fan duties. Holds the last duty for hysteresis.
#[derive(Clone, Debug)]
pub struct Controller {
    cpu: Curve,
    nvme: Curve,
    settings: Settings,
    current: Option<Duty>,
}

impl Controller {
    /// Creates a controller with no duty applied yet.
    #[must_use]
    pub const fn new(cpu: Curve, nvme: Curve, settings: Settings) -> Self {
        Self { cpu, nvme, settings, current: None }
    }

    /// The duty returned by the last [`update`](Self::update), if any.
    #[must_use]
    pub const fn current(&self) -> Option<Duty> {
        self.current
    }

    /// Computes and remembers the duty for `readings`.
    pub fn update(&mut self, readings: Readings) -> Duty {
        let next = match (finite(readings.cpu), finite(readings.nvme)) {
            (Some(cpu), Some(nvme)) => {
                let wanted = self.target(cpu, nvme);
                match self.current {
                    // Only slow down as far as hotter-by-hysteresis readings would allow.
                    Some(current) if wanted < current => {
                        let h = self.settings.hysteresis;
                        current.min(self.target(cpu + h, nvme + h))
                    }
                    _ => wanted,
                }
            }
            _ => Duty::FULL,
        };
        self.current = Some(next);
        next
    }

    fn target(&self, cpu: Celsius, nvme: Celsius) -> Duty {
        let demand = self.cpu.demand(cpu).max(self.nvme.demand(nvme));
        self.settings.duty_for(demand)
    }
}

fn finite(t: Option<Celsius>) -> Option<Celsius> {
    t.filter(|t| t.is_finite())
}

/// Whether the fan should get a short full-speed kick before running at `next`.
///
/// A 2-wire fan may not start from a standstill at low duty, so kick when
/// starting up (`previous` is `None`) or leaving [`Duty::OFF`], unless `next`
/// is already full speed or off.
#[must_use]
pub fn needs_kick(previous: Option<Duty>, next: Duty) -> bool {
    next > Duty::OFF && next < Duty::FULL && previous.is_none_or(|p| p == Duty::OFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pct(p: u8) -> Duty {
        Duty::new(p).unwrap()
    }

    /// The tuned values from the simulation + bench test (sim/RESULTS.md).
    fn controller() -> Controller {
        Controller::new(
            Curve::new(60.0, 80.0).unwrap(),
            Curve::new(55.0, 72.0).unwrap(),
            Settings::new(pct(60), 5, 3.0).unwrap(),
        )
    }

    fn read(cpu: f32, nvme: f32) -> Readings {
        Readings { cpu: Some(cpu), nvme: Some(nvme) }
    }

    #[test]
    fn cool_runs_at_minimum_never_off() {
        assert_eq!(controller().update(read(35.0, 45.0)), pct(60));
    }

    #[test]
    fn hot_runs_full() {
        assert_eq!(controller().update(read(50.0, 72.0)), Duty::FULL);
        assert_eq!(controller().update(read(80.0, 40.0)), Duty::FULL);
        assert_eq!(controller().update(read(95.0, 90.0)), Duty::FULL);
    }

    #[test]
    fn hotter_sensor_wins() {
        // NVMe halfway up its ramp (63.5 °C), CPU cool → 60 + 40 * 0.5 = 80 %.
        assert_eq!(controller().update(read(40.0, 63.5)), pct(80));
        // CPU halfway (70 °C), NVMe cool → 80 % as well.
        assert_eq!(controller().update(read(70.0, 40.0)), pct(80));
    }

    #[test]
    fn rounds_up_to_step() {
        // NVMe 56 °C → demand 1/17 → 62.35 % → rounds up to 65 %.
        assert_eq!(controller().update(read(40.0, 56.0)), pct(65));
    }

    #[test]
    fn missing_or_bad_reading_is_full_speed() {
        let mut c = controller();
        assert_eq!(c.update(Readings { cpu: Some(40.0), nvme: None }), Duty::FULL);
        assert_eq!(c.update(Readings { cpu: None, nvme: Some(40.0) }), Duty::FULL);
        assert_eq!(c.update(Readings::default()), Duty::FULL);
        assert_eq!(c.update(read(f32::NAN, 40.0)), Duty::FULL);
        assert_eq!(c.update(read(40.0, f32::INFINITY)), Duty::FULL);
    }

    #[test]
    fn recovers_after_sensor_returns() {
        let mut c = controller();
        assert_eq!(c.update(Readings::default()), Duty::FULL);
        assert_eq!(c.update(read(35.0, 45.0)), pct(60));
    }

    #[test]
    fn speeds_up_immediately() {
        let mut c = controller();
        assert_eq!(c.update(read(40.0, 50.0)), pct(60));
        assert_eq!(c.update(read(40.0, 63.5)), pct(80));
    }

    #[test]
    fn slows_down_only_after_hysteresis() {
        let mut c = controller();
        assert_eq!(c.update(read(40.0, 63.5)), pct(80));
        // 1 °C cooler: not enough to drop.
        assert_eq!(c.update(read(40.0, 62.5)), pct(80));
        // 2.9 °C cooler: still holding.
        assert_eq!(c.update(read(40.0, 60.6)), pct(80));
        // Well below: drops, but only to what (t + 3 °C) justifies.
        // 57 + 3 = 60 °C → 60 + 40 * 5/17 = 71.8 → 75 %.
        assert_eq!(c.update(read(40.0, 57.0)), pct(75));
        // Cool: back to the minimum.
        assert_eq!(c.update(read(40.0, 45.0)), pct(60));
    }

    #[test]
    fn does_not_hunt_on_small_oscillation() {
        let mut c = controller();
        let first = c.update(read(40.0, 64.0));
        for t in [63.0, 64.0, 62.5, 64.0, 63.2, 63.9] {
            assert_eq!(c.update(read(40.0, t)), first, "changed at {t} °C");
        }
    }

    #[test]
    fn duty_never_decreases_as_temperature_rises() {
        let mut prev = Duty::OFF;
        for tenth in 300..=950 {
            let t = f32::from(u16::try_from(tenth).unwrap()) / 10.0;
            let d = controller().update(read(t, t));
            assert!(d >= prev, "duty fell from {prev} to {d} at {t} °C");
            assert!(d >= pct(60) && d <= Duty::FULL);
            assert_eq!(d.percent() % 5, 0, "{d} at {t} °C is not a 5 % step");
            prev = d;
        }
    }

    #[test]
    fn min_off_allows_stopping() {
        let mut c = Controller::new(
            Curve::new(60.0, 80.0).unwrap(),
            Curve::new(55.0, 72.0).unwrap(),
            Settings::new(Duty::OFF, 5, 3.0).unwrap(),
        );
        assert_eq!(c.update(read(35.0, 45.0)), Duty::OFF);
    }

    #[test]
    fn current_tracks_last_update() {
        let mut c = controller();
        assert_eq!(c.current(), None);
        c.update(read(35.0, 45.0));
        assert_eq!(c.current(), Some(pct(60)));
    }

    #[test]
    fn settings_validation() {
        assert_eq!(Settings::new(pct(60), 0, 3.0), Err(SettingsError::StepOutOfRange(0)));
        assert_eq!(Settings::new(pct(60), 51, 3.0), Err(SettingsError::StepOutOfRange(51)));
        assert_eq!(Settings::new(pct(60), 5, -1.0), Err(SettingsError::BadHysteresis(-1.0)));
        assert!(matches!(
            Settings::new(pct(60), 5, f32::NAN),
            Err(SettingsError::BadHysteresis(_))
        ));
        let s = Settings::new(pct(60), 5, 0.0).unwrap();
        assert_eq!((s.min(), s.step()), (pct(60), 5));
    }

    #[test]
    fn kick_rules() {
        assert!(needs_kick(None, pct(60)));
        assert!(needs_kick(Some(Duty::OFF), pct(60)));
        assert!(!needs_kick(Some(pct(60)), pct(65)));
        assert!(!needs_kick(None, Duty::FULL));
        assert!(!needs_kick(None, Duty::OFF));
        assert!(!needs_kick(Some(pct(80)), Duty::OFF));
    }
}
