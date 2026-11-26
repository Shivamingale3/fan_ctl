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
//! - A manual override ([`Commands`]) replaces the curves' duty, except that a
//!   sensor error or a sensor at its full temperature still means 100 %.
//!   While waiting, the override is checked about once a second, so a change
//!   takes effect quickly; an override's end time also wakes the loop.
//! - Every step publishes the status (see [`Status::render`]).
//! - A PWM write error ends the loop with an error. systemd restarts the
//!   service, and its `ExecStopPost` leaves the fan at full speed meanwhile.

use std::{convert::Infallible, thread, time::Duration};

use crate::config::Config;
use crate::control::{Commands, Mode, Override, Status};
use crate::controller::{Controller, Readings, needs_kick};
use crate::curve::{Celsius, Curve};
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

/// Waits between ticks and tells the time.
pub trait Sleeper {
    /// Blocks for `duration`.
    fn sleep(&mut self, duration: Duration);
    /// Wall-clock time, unix seconds (for override end times).
    fn now(&self) -> u64;
}

/// [`Sleeper`] backed by [`thread::sleep`] and the system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
    fn now(&self) -> u64 {
        crate::control::unix_now()
    }
}

/// How often the override is checked while waiting.
const COMMAND_POLL: Duration = Duration::from_secs(1);

/// What one [`Daemon::tick`] observed and decided.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tick {
    /// The temperatures read (`None` = sensor failed).
    pub readings: Readings,
    /// The duty now applied.
    pub duty: Duty,
    /// Who decided it.
    pub mode: Mode,
    /// How long to wait before the next tick.
    pub next_in: Duration,
}

/// The fan control loop.
#[derive(Debug)]
pub struct Daemon<C, N, F, S, K> {
    cpu: C,
    nvme: N,
    fan: F,
    sleeper: S,
    commands: K,
    controller: Controller,
    cpu_curve: Curve,
    nvme_curve: Curve,
    hysteresis: Celsius,
    min: Duty,
    kick: Duration,
    interval_idle: Duration,
    interval_active: Duration,
    applied: Option<Duty>,
    mode: Option<Mode>,
    manual: Option<Override>,
    logged: Option<Override>,
    loaded: bool,
    hot: bool,
    cpu_error: Option<String>,
    nvme_error: Option<String>,
}

impl<C: TempSource, N: TempSource, F: FanOutput, S: Sleeper, K: Commands> Daemon<C, N, F, S, K> {
    /// Builds a loop from its parts and the curves, settings and timing in `config`.
    pub fn new(cpu: C, nvme: N, fan: F, sleeper: S, commands: K, config: &Config) -> Self {
        Self {
            cpu,
            nvme,
            fan,
            sleeper,
            commands,
            controller: Controller::new(config.cpu_curve, config.nvme_curve, config.settings),
            cpu_curve: config.cpu_curve,
            nvme_curve: config.nvme_curve,
            hysteresis: config.settings.hysteresis(),
            min: config.settings.min(),
            kick: config.kick,
            interval_idle: config.interval_idle,
            interval_active: config.interval_active,
            applied: None,
            mode: None,
            manual: None,
            logged: None,
            loaded: false,
            hot: false,
            cpu_error: None,
            nvme_error: None,
        }
    }

    /// The override source (for inspection in tests and tools).
    #[must_use]
    pub const fn commands(&self) -> &K {
        &self.commands
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
        let auto = self.controller.update(readings);
        let now = self.sleeper.now();
        let manual = self.manual(now);
        let (mode, duty) = match (readings.cpu, readings.nvme) {
            (Some(cpu), Some(nvme)) if cpu.is_finite() && nvme.is_finite() => {
                self.hot = self.is_hot(cpu, nvme);
                match manual {
                    Some(_) if self.hot => (Mode::Safety, Duty::FULL),
                    Some(o) => (Mode::Manual, o.duty),
                    None => (Mode::Auto, auto),
                }
            }
            _ => (Mode::FailSafe, Duty::FULL),
        };
        if self.mode != Some(mode) || (mode == Mode::Manual && manual != self.logged) {
            self.log_mode(mode, manual, now);
            self.mode = Some(mode);
            self.logged = manual;
        }

        if self.applied == Some(duty) {
            log::debug(format_args!("fan {duty} {mode} ({})", Shown(readings)));
        } else {
            if needs_kick(self.applied, duty) && !self.kick.is_zero() {
                log::debug(format_args!("kick-start at {} for {:?}", Duty::FULL, self.kick));
                self.fan.set(Duty::FULL)?;
                self.sleeper.sleep(self.kick);
            }
            self.fan.set(duty)?;
            log::info(format_args!("fan {duty} {mode} ({})", Shown(readings)));
            self.applied = Some(duty);
        }

        self.commands.publish(
            &Status {
                mode,
                duty,
                auto,
                manual,
                readings,
                min: self.min,
                cpu_curve: self.cpu_curve,
                nvme_curve: self.nvme_curve,
                updated: now,
            }
            .render(),
        );

        let mut next_in = if duty <= self.min { self.interval_idle } else { self.interval_active };
        if let Some(until) = manual.and_then(|o| o.until) {
            next_in = next_in.min(Duration::from_secs(until.saturating_sub(now).max(1)));
        }
        Ok(Tick { readings, duty, mode, next_in })
    }

    /// The override in force at `now`: reloaded when its file changed, cleared once expired.
    fn manual(&mut self, now: u64) -> Option<Override> {
        if !self.loaded || self.commands.changed() {
            self.manual = self.commands.load();
            self.loaded = true;
        }
        if self.manual.is_some_and(|o| o.expired(now)) {
            self.commands.clear();
            self.manual = None;
        }
        self.manual
    }

    /// Either sensor at its full temperature; released `hysteresis` °C below it.
    fn is_hot(&self, cpu: Celsius, nvme: Celsius) -> bool {
        let margin = if self.hot { self.hysteresis } else { 0.0 };
        cpu >= self.cpu_curve.full() - margin || nvme >= self.nvme_curve.full() - margin
    }

    fn log_mode(&self, mode: Mode, manual: Option<Override>, now: u64) {
        match (mode, manual) {
            (Mode::Manual, Some(o)) => log::info(format_args!(
                "manual {}{}",
                o.duty,
                o.until.map_or_else(
                    || " until fan_ctl stops".to_owned(),
                    |u| format!(" for {} s", u.saturating_sub(now))
                )
            )),
            (Mode::Safety, _) => log::warn(format_args!(
                "manual overruled: a sensor reached full temperature; fan {}",
                Duty::FULL
            )),
            (Mode::Auto, _) if self.mode.is_some() => log::info(format_args!("back to auto")),
            _ => {}
        }
    }

    /// Sleeps up to `duration`, in [`COMMAND_POLL`] slices, returning early when
    /// the override changes.
    fn wait(&mut self, duration: Duration) {
        let mut left = duration;
        while !left.is_zero() {
            let slice = left.min(COMMAND_POLL);
            self.sleeper.sleep(slice);
            left -= slice;
            if self.commands.changed() {
                return;
            }
        }
    }

    /// Runs [`tick`](Self::tick) forever, sleeping in between.
    ///
    /// # Errors
    ///
    /// Only returns, with the [`PwmError`], when the fan can no longer be driven.
    pub fn run(&mut self) -> Result<Infallible, PwmError> {
        loop {
            let tick = self.tick()?;
            self.wait(tick.next_in);
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

    /// Records sleeps; its clock starts at `T0` and advances with each sleep.
    #[derive(Default)]
    struct RecordingSleeper(Vec<Duration>);

    const T0: u64 = 1_000_000;

    impl Sleeper for RecordingSleeper {
        fn sleep(&mut self, d: Duration) {
            self.0.push(d);
        }
        fn now(&self) -> u64 {
            T0 + self.0.iter().map(Duration::as_secs).sum::<u64>()
        }
    }

    /// A pending change to the scripted override.
    enum Next {
        Set(Override),
        Remove,
    }

    /// Scripted override: `next` becomes current on the next `load`.
    #[derive(Default)]
    struct FakeCommands {
        current: Option<Override>,
        next: Option<Next>,
        loads: usize,
        cleared: usize,
        published: Vec<String>,
    }

    impl FakeCommands {
        fn set(&mut self, o: Option<Override>) {
            self.next = Some(o.map_or(Next::Remove, Next::Set));
        }
    }

    impl Commands for FakeCommands {
        fn changed(&mut self) -> bool {
            self.next.is_some()
        }
        fn load(&mut self) -> Option<Override> {
            self.loads += 1;
            match self.next.take() {
                Some(Next::Set(o)) => self.current = Some(o),
                Some(Next::Remove) => self.current = None,
                None => {}
            }
            self.current
        }
        fn clear(&mut self) {
            self.cleared += 1;
            self.current = None;
        }
        fn publish(&mut self, status: &str) {
            self.published.push(status.to_owned());
        }
    }

    type TestDaemon = Daemon<Script, Script, RecordingFan, RecordingSleeper, FakeCommands>;

    fn daemon(cpu: &[Option<Celsius>], nvme: &[Option<Celsius>]) -> TestDaemon {
        daemon_with(cpu, nvme, &Config::default())
    }

    fn daemon_with(cpu: &[Option<Celsius>], nvme: &[Option<Celsius>], cfg: &Config) -> TestDaemon {
        Daemon::new(
            script(cpu),
            script(nvme),
            RecordingFan::default(),
            RecordingSleeper::default(),
            FakeCommands::default(),
            cfg,
        )
    }

    fn manual(p: u8, until: Option<u64>) -> Override {
        Override { duty: Duty::new(p).unwrap(), until }
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
        let mut want = vec![S(2)];
        want.extend([S(1); 30]); // two 15 s waits, in 1 s slices
        assert_eq!(d.sleeper().0, want);
    }

    #[test]
    fn manual_replaces_the_curve_and_is_published() {
        let mut d = daemon(&[Some(40.0)], &[Some(45.0)]);
        d.commands.set(Some(manual(85, None)));
        let t = d.tick().unwrap();
        assert_eq!((t.mode, t.duty), (Mode::Manual, Duty::new(85).unwrap()));
        assert_eq!(d.fan().writes, [100, 85], "kick at start, then manual");
        let status = d.commands().published.last().unwrap();
        assert!(status.contains("mode = manual\nduty = 85\nauto = 60\nmanual = 85\nuntil = 0\n"));
        assert!(status.contains(&format!("updated = {T0}")), "time at the start of the step");
    }

    #[test]
    fn manual_off_and_back_to_auto() {
        let mut d = daemon(&[Some(40.0)], &[Some(45.0)]);
        d.tick().unwrap();
        d.commands.set(Some(manual(0, None)));
        assert_eq!(d.tick().unwrap().duty, Duty::OFF);
        d.commands.set(None);
        let t = d.tick().unwrap();
        assert_eq!((t.mode, t.duty), (Mode::Auto, Duty::new(60).unwrap()));
        assert_eq!(d.fan().writes, [100, 60, 0, 100, 60], "kick when leaving 0 %");
    }

    #[test]
    fn override_is_loaded_once_until_it_changes() {
        let mut d = daemon(&[Some(40.0)], &[Some(45.0)]);
        for _ in 0..3 {
            d.tick().unwrap();
        }
        assert_eq!(d.commands().loads, 1);
    }

    #[test]
    fn heat_overrules_manual_with_hysteresis() {
        // NVMe full is 72 °C, hysteresis 3 °C.
        let nvme = [Some(60.0), Some(72.0), Some(70.0), Some(68.9)];
        let mut d = daemon(&[Some(40.0)], &nvme);
        d.commands.set(Some(manual(60, None)));
        assert_eq!(d.tick().unwrap().mode, Mode::Manual);
        let hot = d.tick().unwrap();
        assert_eq!((hot.mode, hot.duty), (Mode::Safety, Duty::FULL));
        assert_eq!(d.tick().unwrap().mode, Mode::Safety, "70 °C: within hysteresis");
        assert_eq!(d.tick().unwrap().mode, Mode::Manual, "68.9 °C: released");
        // CPU at its full temperature (80 °C) also overrules.
        let mut d = daemon(&[Some(80.0)], &[Some(40.0)]);
        d.commands.set(Some(manual(0, None)));
        assert_eq!(d.tick().unwrap().duty, Duty::FULL);
    }

    #[test]
    fn sensor_error_overrules_manual() {
        let mut d = daemon(&[Some(40.0)], &[None]);
        d.commands.set(Some(manual(0, None)));
        let t = d.tick().unwrap();
        assert_eq!((t.mode, t.duty), (Mode::FailSafe, Duty::FULL));
        assert!(d.commands().published[0].contains("nvme = ?"));
    }

    #[test]
    fn override_expires_and_wakes_the_loop() {
        let mut d = daemon(&[Some(40.0)], &[Some(45.0)]);
        d.commands.set(Some(manual(90, Some(T0 + 2 + 7))));
        let t = d.tick().unwrap(); // clock T0 + 2 after the kick
        assert_eq!(t.mode, Mode::Manual);
        assert_eq!(t.next_in, S(5), "active interval 5 s < 7 s left");
        d.sleeper.0.push(S(5));
        assert_eq!(d.tick().unwrap().next_in, S(2), "woken at the end time");
        d.sleeper.0.push(S(2));
        let t = d.tick().unwrap();
        assert_eq!((t.mode, t.duty), (Mode::Auto, Duty::new(60).unwrap()));
        assert_eq!(d.commands().cleared, 1, "expired file removed");
    }

    #[test]
    fn wait_returns_early_on_a_new_override() {
        let mut d = daemon(&[Some(40.0)], &[Some(45.0)]);
        d.tick().unwrap();
        d.wait(S(3));
        assert_eq!(d.sleeper().0, [S(2), S(1), S(1), S(1)]);
        d.commands.set(Some(manual(70, None)));
        d.wait(S(15));
        assert_eq!(d.sleeper().0.len(), 5, "stopped after one slice");
    }

    #[test]
    fn shown_formats_readings() {
        let r = Readings { cpu: Some(52.06), nvme: None };
        assert_eq!(Shown(r).to_string(), "cpu 52.1 °C, nvme ?");
    }
}
