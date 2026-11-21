//! Temperature → cooling demand.

use std::{error::Error, fmt};

/// Temperature in degrees Celsius.
pub type Celsius = f32;

/// A linear cooling ramp for one sensor.
///
/// Demand is `0.0` at or below [`start`](Curve::start), `1.0` at or above
/// [`full`](Curve::full), and linear in between. The controller maps that
/// demand onto the fan's usable duty range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Curve {
    start: Celsius,
    full: Celsius,
}

/// Why a [`Curve`] could not be built.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CurveError {
    /// A temperature was NaN or infinite.
    NotFinite,
    /// `start` was not strictly below `full`.
    NotIncreasing {
        /// Requested ramp start.
        start: Celsius,
        /// Requested full-speed temperature.
        full: Celsius,
    },
}

impl fmt::Display for CurveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFinite => f.write_str("curve temperatures must be finite numbers"),
            Self::NotIncreasing { start, full } => {
                write!(f, "curve start ({start} °C) must be below full ({full} °C)")
            }
        }
    }
}

impl Error for CurveError {}

impl Curve {
    /// Creates a ramp from `start` (lowest speed) to `full` (100 %).
    ///
    /// # Errors
    ///
    /// Returns [`CurveError`] if either value is not finite or `start >= full`.
    pub const fn new(start: Celsius, full: Celsius) -> Result<Self, CurveError> {
        if !start.is_finite() || !full.is_finite() {
            return Err(CurveError::NotFinite);
        }
        if start >= full {
            return Err(CurveError::NotIncreasing { start, full });
        }
        Ok(Self { start, full })
    }

    /// Temperature at which the ramp starts rising above the minimum speed.
    #[must_use]
    pub const fn start(&self) -> Celsius {
        self.start
    }

    /// Temperature at and above which the fan runs at full speed.
    #[must_use]
    pub const fn full(&self) -> Celsius {
        self.full
    }

    /// Cooling demand at temperature `t`, in `0.0..=1.0`.
    ///
    /// A non-finite reading (NaN or ±∞) yields `1.0`: fail safe, never quiet.
    #[must_use]
    pub fn demand(&self, t: Celsius) -> f32 {
        if !t.is_finite() {
            return 1.0;
        }
        ((t - self.start) / (self.full - self.start)).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nvme() -> Curve {
        Curve::new(55.0, 72.0).unwrap()
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn zero_at_or_below_start() {
        assert!(close(nvme().demand(20.0), 0.0));
        assert!(close(nvme().demand(55.0), 0.0));
    }

    #[test]
    fn one_at_or_above_full() {
        assert!(close(nvme().demand(72.0), 1.0));
        assert!(close(nvme().demand(150.0), 1.0));
    }

    #[test]
    fn linear_in_between() {
        assert!(close(nvme().demand(63.5), 0.5));
        assert!(close(nvme().demand(59.25), 0.25));
    }

    #[test]
    fn non_finite_reading_is_full_demand() {
        assert!(close(nvme().demand(f32::NAN), 1.0));
        assert!(close(nvme().demand(f32::INFINITY), 1.0));
        assert!(close(nvme().demand(f32::NEG_INFINITY), 1.0));
    }

    #[test]
    fn rejects_bad_ramps() {
        assert_eq!(
            Curve::new(70.0, 70.0),
            Err(CurveError::NotIncreasing { start: 70.0, full: 70.0 })
        );
        assert!(matches!(Curve::new(80.0, 60.0), Err(CurveError::NotIncreasing { .. })));
        assert_eq!(Curve::new(f32::NAN, 60.0), Err(CurveError::NotFinite));
        assert_eq!(Curve::new(50.0, f32::INFINITY), Err(CurveError::NotFinite));
    }

    #[test]
    fn exposes_bounds() {
        assert!(close(nvme().start(), 55.0));
        assert!(close(nvme().full(), 72.0));
    }
}
