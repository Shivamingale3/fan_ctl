//! Hardware probe for a real Pi: shows what `fan_ctl` would see.
//!
//! ```sh
//! probe            # read-only: sensors + PWM chip state
//! probe --set 100  # also open the channel and set the duty (needs root)
//! ```
#![allow(clippy::print_stdout)]

use std::{env, path::Path, process::ExitCode};

use fan_ctl::duty::Duty;
use fan_ctl::pwm::{self, PIO_DEVICE, PwmChannel};
use fan_ctl::sensor::{CPU_CHIP, HwmonSensor, NVME_CHIP};
use fan_ctl::sysfs::{self, DEFAULT_ROOT};

const PERIOD_NS: u32 = 40_000;

fn main() -> ExitCode {
    let root = Path::new(DEFAULT_ROOT);
    for chip in [CPU_CHIP, NVME_CHIP] {
        let mut sensor = HwmonSensor::new(root, chip);
        match sensor.read() {
            Ok(t) => {
                println!("{chip:12} {t:6.2} °C  ({})", sensor.input().unwrap_or(root).display());
            }
            Err(e) => println!("{chip:12} ERROR {e}"),
        }
    }

    let chip = match pwm::find_chip(root, PIO_DEVICE) {
        Ok(c) => c,
        Err(e) => {
            println!("pwm          ERROR {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("pwm chip     {}", chip.display());
    for attr in ["period", "duty_cycle", "enable"] {
        let v = sysfs::read_trimmed(&chip.join("pwm0").join(attr))
            .unwrap_or_else(|_| "(not exported)".into());
        println!("  pwm0/{attr:10} {v}");
    }

    let mut args = env::args().skip(1);
    if args.next().as_deref() == Some("--set") {
        let Some(duty) = args.next().and_then(|s| s.parse().ok()).and_then(Duty::new) else {
            println!("usage: probe --set <0-100>");
            return ExitCode::FAILURE;
        };
        match PwmChannel::open(&chip, 0, PERIOD_NS).and_then(|p| p.set(duty).map(|()| p)) {
            Ok(p) => println!("set {duty} on {} (period {} ns)", p.path().display(), p.period_ns()),
            Err(e) => {
                println!("set          ERROR {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}
