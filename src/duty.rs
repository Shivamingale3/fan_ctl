//! Fan duty cycle.

use std::fmt;

/// A fan duty cycle in whole percent, guaranteed to be within `0..=100`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duty(u8);

impl Duty {
    /// Fan stopped.
    pub const OFF: Self = Self(0);
    /// Full speed.
    pub const FULL: Self = Self(100);

    /// Creates a duty from a percentage, or `None` if it is above 100.
    #[must_use]
    pub const fn new(percent: u8) -> Option<Self> {
        if percent <= 100 { Some(Self(percent)) } else { None }
    }

    /// Creates a duty from a percentage, clamping anything above 100 to [`Duty::FULL`].
    #[must_use]
    pub const fn saturating(percent: u8) -> Self {
        if percent <= 100 { Self(percent) } else { Self::FULL }
    }

    /// The duty in percent.
    #[must_use]
    pub const fn percent(self) -> u8 {
        self.0
    }

    /// The PWM on-time in nanoseconds for a period of `period_ns`.
    #[must_use]
    pub fn on_time_ns(self, period_ns: u32) -> u32 {
        // Widen first: `period_ns * 100` can overflow u32. The result is at most
        // `period_ns`, so narrowing back cannot fail.
        let on = u64::from(period_ns) * u64::from(self.0) / 100;
        u32::try_from(on).unwrap_or(period_ns)
    }
}

impl fmt::Display for Duty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}%", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_0_to_100_only() {
        assert_eq!(Duty::new(0), Some(Duty::OFF));
        assert_eq!(Duty::new(100), Some(Duty::FULL));
        assert_eq!(Duty::new(101), None);
        assert_eq!(Duty::new(u8::MAX), None);
    }

    #[test]
    fn saturating_clamps_to_full() {
        assert_eq!(Duty::saturating(55).percent(), 55);
        assert_eq!(Duty::saturating(200), Duty::FULL);
    }

    #[test]
    fn on_time_scales_period() {
        let period = 40_000; // 25 kHz
        assert_eq!(Duty::OFF.on_time_ns(period), 0);
        assert_eq!(Duty::saturating(60).on_time_ns(period), 24_000);
        assert_eq!(Duty::FULL.on_time_ns(period), period);
    }

    #[test]
    fn on_time_does_not_overflow_on_long_periods() {
        assert_eq!(Duty::FULL.on_time_ns(u32::MAX), u32::MAX);
        assert_eq!(Duty::saturating(50).on_time_ns(u32::MAX), u32::MAX / 2);
    }

    #[test]
    fn displays_as_percent() {
        assert_eq!(Duty::saturating(65).to_string(), "65%");
    }
}
