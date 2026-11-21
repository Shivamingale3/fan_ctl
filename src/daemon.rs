//! The control loop: read both sensors, decide a duty, drive the fan, sleep.
//!
//! Hardware is behind small traits ([`TempSource`], [`FanOutput`], [`Sleeper`])
//! so the loop is tested with fakes and `--dry-run` can swap the fan out.
//!
//! - The PWM is written **only when the duty changes**.
//! - Starting the fan (at start-up, or from 0 %) first runs it at 100 % for `kick`.
//! - Polls every `interval_idle` at minimum speed, `interval_active` above it.
//! - Sensor errors are logged once per distinct error (not every tick) and
//!   make the controller run the fan at 100 % until the sensor recovers.
//! - A PWM write error ends the loop with an error. systemd restarts the
//!   service, and its `ExecStopPost` leaves the fan at full speed meanwhile.

use std::{convert::Infallible, thread, time::Duration};

use crate::config::Config;
use crate::controller::{Controller, Readings, needs_kick};
use crate::curve::Celsius;
use crate::duty::Duty;
use crate::log;
use crate::pwm::{PwmChannel, PwmError};
use crate::sensor::{HwmonSensor, SensorError};

/// Something that yields a temperature.
pub trait TempSource {
    /// Short name for log lines (`cpu_thermal`, `nvme`).
    fn name(&self) -> &str;
    /// Reads the current temperature.
    ///
    /// # Errors
    ///
    /// Returns [`SensorError`] if no usable reading is available.
    fn read(&mut self) -> Result<Celsius, SensorError>;
}

impl TempSource for HwmonSensor {
    fn name(&self) -> &str {
        self.chip()
    }

    fn read(&mut self) -> Result<Celsius, SensorError> {
        Self::read(self)
    }
}

/// Something that can be set to a duty.
pub trait FanOutput {
    /// Applies `duty`.
    ///
    /// # Errors
    ///
    /// Returns [`PwmError`] if the duty could not be applied.
    fn set(&mut self, duty: Duty) -> Result<(), PwmError>;
}

impl FanOutput for PwmChannel {
    fn set(&mut self, duty: Duty) -> Result<(), PwmError> {
        Self::set(self, duty)
    }
}

/// A fan that only logs what it would do (`--dry-run`).
#[derive(Clone, Copy, Debug, Default)]
pub struct DryRunFan;

impl FanOutput for DryRunFan {
    fn set(&mut self, duty: Duty) -> Result<(), PwmError> {
        log::info(format_args!("dry-run: would set fan to {duty}"));
        Ok(())
    }
}

/// Waits between ticks.
pub trait Sleeper {
    /// Blocks for `duration`.
    fn sleep(&mut self, duration: Duration);
}

/// [`Sleeper`] backed by [`thread::sleep`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}

/// What one [`Daemon::tick`] observed and decided.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tick {
    /// The temperatures read (`None` = sensor failed).
    pub readings: Readings,
    /// The duty now applied.
    pub duty: Duty,
    /// How long to wait before the next tick.
    pub next_in: Duration,
}

/// The fan control loop.
#[derive(Debug)]
pub struct Daemon<C, N, F, S> {
    cpu: C,
    nvme: N,
    fan: F,
    sleeper: S,
    controller: Controller,
    min: Duty,
    kick: Duration,
    interval_idle: Duration,
    interval_active: Duration,
    applied: Option<Duty>,
    cpu_error: Option<String>,
    nvme_error: Option<String>,
}

impl<C: TempSource, N: TempSource, F: FanOutput, S: Sleeper> Daemon<C, N, F, S> {
    /// Builds a loop from its parts and the curves, settings and timing in `config`.
    pub fn new(cpu: C, nvme: N, fan: F, sleeper: S, config: &Config) -> Self {
        Self {
            cpu,
            nvme,
            fan,
            sleeper,
            controller: Controller::new(config.cpu_curve, config.nvme_curve, config.settings),
            min: config.settings.min(),
            kick: config.kick,
            interval_idle: config.interval_idle,
            interval_active: config.interval_active,
            applied: None,
            cpu_error: None,
            nvme_error: None,
        }
    }

    /// The duty last written to the fan, if any.
    #[must_use]
    pub const fn applied(&self) -> Option<Duty> {
        self.applied
    }

    /// The fan output (for inspection in tests and tools).
    #[must_use]
    pub const fn fan(&self) -> &F {
        &self.fan
    }

    /// The sleeper (for inspection in tests and tools).
    #[must_use]
    pub const fn sleeper(&self) -> &S {
        &self.sleeper
    }

    /// Runs one control step.
    ///
    /// # Errors
    ///
    /// Returns [`PwmError`] if the fan could not be driven.
    pub fn tick(&mut self) -> Result<Tick, PwmError> {
        let readings = Readings {
            cpu: read_logged(&mut self.cpu, &mut self.cpu_error),
            nvme: read_logged(&mut self.nvme, &mut self.nvme_error),
        };
        let duty = self.controller.update(readings);

        if self.applied == Some(duty) {
            log::debug(format_args!("fan {duty} ({})", Shown(readings)));
        } else {
            if needs_kick(self.applied, duty) && !self.kick.is_zero() {
                log::debug(format_args!("kick-start at {} for {:?}", Duty::FULL, self.kick));
                self.fan.set(Duty::FULL)?;
                self.sleeper.sleep(self.kick);
            }
            self.fan.set(duty)?;
            log::info(format_args!("fan {duty} ({})", Shown(readings)));
            self.applied = Some(duty);
        }

        let next_in = if duty <= self.min { self.interval_idle } else { self.interval_active };
        Ok(Tick { readings, duty, next_in })
    }

    /// Runs [`tick`](Self::tick) forever, sleeping in between.
    ///
    /// # Errors
    ///
    /// Only returns, with the [`PwmError`], when the fan can no longer be driven.
    pub fn run(&mut self) -> Result<Infallible, PwmError> {
        loop {
            let tick = self.tick()?;
            self.sleeper.sleep(tick.next_in);
        }
    }
}

/// Reads `source`, logging only when its error state changes.
fn read_logged(source: &mut impl TempSource, last_error: &mut Option<String>) -> Option<Celsius> {
    match source.read() {
        Ok(t) => {
            if last_error.take().is_some() {
                log::info(format_args!("{}: sensor recovered ({t:.1} °C)", source.name()));
            }
            Some(t)
        }
        Err(e) => {
            let msg = e.to_string();
            if last_error.as_deref() != Some(msg.as_str()) {
                log::warn(format_args!("{}: {msg}; fan forced to 100%", source.name()));
                *last_error = Some(msg);
            }
            None
        }
    }
}

/// Formats readings as `cpu 52.1 °C, nvme 58.9 °C` (`?` for a failed sensor).
struct Shown(Readings);

impl std::fmt::Display for Shown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let show = |t: Option<Celsius>| t.map_or_else(|| "?".to_owned(), |t| format!("{t:.1} °C"));
        write!(f, "cpu {}, nvme {}", show(self.0.cpu), show(self.0.nvme))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// Replays scripted readings; `None` = sensor missing. Repeats the last value.
    struct Script {
        values: VecDeque<Option<Celsius>>,
        last: Option<Celsius>,
    }

    fn script(values: &[Option<Celsius>]) -> Script {
        Script { values: values.iter().copied().collect(), last: None }
    }

    impl TempSource for Script {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn read(&mut self) -> Result<Celsius, SensorError> {
            if let Some(v) = self.values.pop_front() {
                self.last = v;
            }
            self.last.ok_or(SensorError::NotFound { chip: "fake".into() })
        }
    }

    #[derive(Default)]
    struct RecordingFan {
        writes: Vec<u8>,
        fail_after: Option<usize>,
    }

    impl FanOutput for RecordingFan {
        fn set(&mut self, duty: Duty) -> Result<(), PwmError> {
            if self.fail_after.is_some_and(|n| self.writes.len() >= n) {
                return Err(PwmError::ZeroPeriod);
            }
            self.writes.push(duty.percent());
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingSleeper(Vec<Duration>);

    impl Sleeper for RecordingSleeper {
        fn sleep(&mut self, d: Duration) {
            self.0.push(d);
        }
    }

    type TestDaemon = Daemon<Script, Script, RecordingFan, RecordingSleeper>;

    fn daemon(cpu: &[Option<Celsius>], nvme: &[Option<Celsius>]) -> TestDaemon {
        daemon_with(cpu, nvme, &Config::default())
    }

    fn daemon_with(cpu: &[Option<Celsius>], nvme: &[Option<Celsius>], cfg: &Config) -> TestDaemon {
        Daemon::new(
            script(cpu),
            script(nvme),
            RecordingFan::default(),
            RecordingSleeper::default(),
            cfg,
        )
    }

    const S: fn(u64) -> Duration = Duration::from_secs;

    #[test]
    fn start_up_kicks_then_settles_at_minimum() {
        let mut d = daemon(&[Some(40.0)], &[Some(45.0)]);
        let t = d.tick().unwrap();
        assert_eq!(d.fan().writes, [100, 60]);
        assert_eq!(d.sleeper().0, [S(2)], "2 s kick");
        assert_eq!(t.duty, Duty::saturating(60));
        assert_eq!(t.next_in, S(15), "idle interval at minimum");
        assert_eq!(d.applied(), Some(Duty::saturating(60)));
    }

    #[test]
    fn unchanged_duty_is_not_rewritten() {
        let mut d = daemon(&[Some(40.0)], &[Some(45.0), Some(46.0), Some(45.5)]);
        for _ in 0..3 {
            d.tick().unwrap();
        }
        assert_eq!(d.fan().writes, [100, 60]);
    }

    #[test]
    fn heating_speeds_up_without_kick_and_polls_faster() {
        let mut d = daemon(&[Some(40.0)], &[Some(45.0), Some(63.5)]);
        d.tick().unwrap();
        let t = d.tick().unwrap();
        assert_eq!(d.fan().writes, [100, 60, 80]);
        assert_eq!(d.sleeper().0, [S(2)], "no second kick");
        assert_eq!(t.next_in, S(5));
    }

    #[test]
    fn sensor_failure_forces_full_then_recovers() {
        let mut d = daemon(&[Some(40.0)], &[Some(45.0), None, None, Some(45.0)]);
        d.tick().unwrap();
        let failed = d.tick().unwrap();
        assert_eq!(failed.readings.nvme, None);
        assert_eq!(failed.duty, Duty::FULL);
        assert_eq!(failed.next_in, S(5));
        d.tick().unwrap(); // still failed: no rewrite
        d.tick().unwrap(); // recovered and cool: back to minimum
        assert_eq!(d.fan().writes, [100, 60, 100, 60]);
    }

    #[test]
    fn missing_sensor_at_start_runs_full_without_kick() {
        let mut d = daemon(&[Some(40.0)], &[None]);
        assert_eq!(d.tick().unwrap().duty, Duty::FULL);
        assert_eq!(d.fan().writes, [100]);
        assert_eq!(d.sleeper().0, Vec::<Duration>::new());
    }

    #[test]
    fn fan_error_is_returned() {
        let mut d = daemon(&[Some(40.0)], &[Some(45.0)]);
        d.fan.fail_after = Some(1); // kick write succeeds, duty write fails
        assert!(matches!(d.tick(), Err(PwmError::ZeroPeriod)));
        assert_eq!(d.applied(), None);
    }

    #[test]
    fn zero_kick_skips_kick() {
        let cfg = Config::parse("kick_ms = 0").unwrap();
        let mut d = daemon_with(&[Some(40.0)], &[Some(45.0)], &cfg);
        d.tick().unwrap();
        assert_eq!(d.fan().writes, [60]);
        assert_eq!(d.sleeper().0, Vec::<Duration>::new());
    }

    #[test]
    fn leaving_standstill_kicks_when_min_is_off() {
        let cfg = Config::parse("min_duty = 0").unwrap();
        let mut d = daemon_with(&[Some(40.0)], &[Some(45.0), Some(45.0), Some(60.0)], &cfg);
        d.tick().unwrap(); // start at 0 %: no kick needed
        d.tick().unwrap();
        d.tick().unwrap(); // 60 °C → leaves 0 %: kick
        assert_eq!(d.fan().writes, [0, 100, 30]);
        assert_eq!(d.sleeper().0, [S(2)]);
    }

    #[test]
    fn run_sleeps_between_ticks_and_stops_on_fan_error() {
        let mut d = daemon(&[Some(40.0)], &[Some(45.0), Some(45.0), Some(63.5)]);
        d.fan.fail_after = Some(2); // 100, 60 succeed; the 80 % write fails
        assert!(d.run().is_err());
        assert_eq!(d.fan().writes, [100, 60]);
        assert_eq!(d.sleeper().0, [S(2), S(15), S(15)]);
    }

    #[test]
    fn shown_formats_readings() {
        let r = Readings { cpu: Some(52.06), nvme: None };
        assert_eq!(Shown(r).to_string(), "cpu 52.1 °C, nvme ?");
    }
}
