//! `fan_ctl`: temperature-driven case fan controller for the Raspberry Pi 5.
//!
//! The fan speed follows whichever of the CPU and NVMe temperatures needs more
//! cooling. Modules are added phase by phase; see `PLAN.md`.

pub mod controller;
pub mod curve;
pub mod duty;
pub mod sensor;
pub mod sysfs;

#[cfg(test)]
mod testutil;
