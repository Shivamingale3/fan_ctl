# fan_ctl — Research

Research date: 2026-11-20. Everything marked **verified** was checked on the Pi itself.

## 1. Hardware

| Item | Fact | Source |
|---|---|---|
| Fan | 4010 (4 × 4 × 1 cm), 2-wire, DC 5 V, 0.1 A (0.5 W max), no tachometer | fan label, `wiring/` |
| Fan + | Pi pin 4 (5 V) | user |
| Fan − / emitter | Pi pin 9 (GND) through NPN low-side switch + flyback diode | user |
| Fan control | **GPIO27 = physical pin 13** (transistor base resistor) | **verified**: fan spins with `pinctrl set 27 op dh`; journal shows same command used 2026-11-19 |
| NOT the fan | GPIO14 / pin 8: driven high + 25 kHz PWM, fan did not move | **verified** |
| Case | Sealed. Wiring cannot be changed, transistor/resistor values unknown (taped) | user |
| SSD | PT900NVG3-512G, firmware VC400618, on PCIe 2.0 x1 M.2 HAT | **verified** |
| CPU cooler | Official Active Cooler, firmware/kernel controlled (`pwm-fan`, cooling_device0) | **verified** |

## 2. Pi software state

| Item | Value | Verified |
|---|---|---|
| OS | Debian 13 trixie, kernel 6.18.50+rpt-rpi-2712, aarch64, 16 KB pages | ✓ |
| libc | glibc 2.41 (irrelevant if we ship a static musl binary) | ✓ |
| Rust/Go/compilers on Pi | none installed (only python3) | ✓ |
| Free RAM | ~6.7 GB of 8 GB | ✓ |
| Existing similar service | `case-ring-breathe.service` (Python, GPIO13 PWM): 11.2 MB RSS, 0.1 % CPU | ✓ |
| GPIO used by others | 7, 8, 9–11 (SPI0 display), 12 (backlight), 13 (LED ring, PWM0 ch1), 17–21 (touch SPI1), 24, 25 | ✓ |

## 3. Temperature sensors

| Sensor | Path | Notes | Verified |
|---|---|---|---|
| CPU | `/sys/class/hwmon/hwmonX/temp1_input` where `name` = `cpu_thermal` (also `/sys/class/thermal/thermal_zone0/temp`) | millidegrees, ~0.5 °C steps | ✓ |
| NVMe | `/sys/class/hwmon/hwmonX/temp1_input` where `name` = `nvme` | millidegrees; drive reports **whole °C** (values end in .85 → Kelvin conversion) | ✓ |
| NVMe limits | `temp1_max` 99.85 °C, `temp1_crit` 109.85 °C | look like placeholders; treat ~75 °C as the practical ceiling | ✓ |

- hwmon numbers (`hwmon0…5`) can change between boots → **find by `name`, never hard-code the number.**
- Each NVMe read = one NVMe admin command (Get Log Page, SMART log) inside the kernel. APST is enabled (`default_ps_max_latency_us` = 100000), so very frequent reads could keep the drive out of low-power states. Reading every 5–15 s is harmless. [kernel hwmon.c](https://codebrowser.dev/linux/linux/drivers/nvme/host/hwmon.c.html)

## 4. Can it be event-driven instead of polling?

| Source | Pushes events? | Detail |
|---|---|---|
| NVMe | **No.** | Only a critical-warning bit / async event at over-temperature (too late). The NVMe thermal-zone patch was never merged: only `thermal_zone0` (cpu-thermal) exists. ✓ |
| CPU | Partly | Kernel thermal netlink sends trip-crossing events, but the kernel itself polls the sensor to produce them. |
| Kernel-only fan control | CPU only | `pwm-fan` / `pwm-gpio-fan` overlays bind to cpu-thermal; can't see the NVMe. |

**Conclusion:** a tiny sleeping userspace loop is required for NVMe. A sleeping process uses 0 CPU; cost is only its wake-ups.

## 5. Driving GPIO27 with PWM

GPIO27 has **no hardware PWM** (RP1 PWM only on GPIO12–15, 18, 19). Options:

| Method | Verified | Notes |
|---|---|---|
| **`dtoverlay=pwm-pio,gpio=27`** (RP1 PIO state machine) | ✓ loaded at runtime → `/sys/class/pwm/pwmchip2`, `npwm`=1, device `platform/pwm_pio@1b`, pin func `a7 PIO27` | Requested 40000/40000 ns → actual 39990/40005 ns (**25 kHz works**). 100 kHz period also accepted. Pi 5 only, up to 4 instances. 0 CPU. |
| `pwm-gpio` (kernel bit-bang) | not tested | hrtimer jitter, possible fan ticking |
| Plain on/off (`pinctrl`, `gpio-fan` overlay) | ✓ on/off works | no speed control |

- pwmchip **number** depends on load order → find the chip whose `device` link contains `pwm_pio`.
- Sysfs PWM state persists after the process exits (good for fail-safe: set 100 % and leave).
- At boot, before our service starts, the pin is PIO-owned and **low → fan off** for a few seconds. Acceptable (NVMe idles ~61 °C).
- Duty that still spins this fan from a standstill: **unknown**. Needs a bench test (user listens).

## 6. Thermal test results (sealed case, 4 × sha256 CPU load + NVMe direct-I/O reads + periodic 1 GB writes)

Raw logs: `tests_data/run1.csv` (fan off), `tests_data/run3.csv` (fan on/off/on).

| | Fan off | Fan on 100 % |
|---|---|---|
| NVMe start | 61 °C | 61 °C |
| NVMe after 1 min | 74.8 °C | 68.8 °C |
| NVMe steady | none, hit 78.8 °C at 2.5 min (safety abort) | **~70 °C flat for 10 min** |
| CPU | 67 °C, rising | ~57 °C flat |
| Active Cooler PWM | 175 (high) | 75 (lowest) |

Same run, load never stopped: fan off → NVMe 69.8 → 76.8 °C in 4 min; fan back on → 73.8 °C and CPU 65.5 → 56.2 °C in 4 min.

**Takeaways**
1. NVMe is the critical part; it overheats in <3 min of sustained load with no airflow.
2. At 100 % the fan holds NVMe ~70 °C: some, but not a lot, of headroom → heavy load must mean 100 %.
3. Case fan also cuts CPU ~10 °C and keeps the Active Cooler quiet → both temps matter.
4. Idle NVMe ~61 °C with fan off → the fan should probably never be fully off; keep a low floor.

## 7. Language / footprint

| Language | Typical RSS for a sleep-read-write loop | Deploy |
|---|---|---|
| Rust (std only, static musl, `opt-level="z"`, LTO, strip, `panic=abort`) | ~0.3 MB private, ~1 MB RSS incl. shared pages | 1 static file, ~300–500 KB |
| Go | ~6–8 MB | 1 static file, ~2 MB |
| Python | ~11 MB (measured, ring service) | needs interpreter |

CPU cost is ~0 for all at a 5–15 s interval. **Rust chosen** (user decision).

## 8. Build & release via GitHub Actions

- arm64 hosted runners (`ubuntu-24.04-arm`) **work in private repos since Jan 2026**; 2 vCPU/8 GB; minutes count against the plan's free minutes. [changelog](https://github.blog/changelog/2026-01-29-arm64-standard-runners-are-now-available-in-private-repositories), [GA](https://github.blog/changelog/2026-09-03-github-actions-arm64-linux-and-windows-runners-are-now-generally-available/)
- Alternative: x86 `ubuntu-latest` + `rustup target add aarch64-unknown-linux-musl` + `-C linker=rust-lld -C link-self-contained=yes`. Works for std-only crates (no C deps). [Rust forum](https://users.rust-lang.org/t/cross-compilation-linux-x86-64-host-aarch64-target/105680)
- Release upload: `gh release create` with the built-in `GITHUB_TOKEN` (`permissions: contents: write`). No third-party actions needed.
- Repo: the parent repo `Shivamingale3/rpi5-pebble-case` is **private**. Downloading a private release asset on the Pi needs a token; a public repo needs none.

## 9. Open questions

1. Lowest duty that starts / keeps this fan spinning (bench test).
2. Is 25 kHz OK with the unknown transistor, or does it need a lower frequency (e.g. 1–5 kHz)? A slow transistor at 25 kHz runs hot and may not switch cleanly. Bench test both.
3. Separate GitHub repo for `fan_ctl` (public or private?) or a folder in the existing repo?
