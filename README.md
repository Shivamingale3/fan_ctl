# fan_ctl

Temperature-driven case fan controller for the **Raspberry Pi 5**.

It sets the speed of a 2-wire case fan from **whichever of the CPU and NVMe temperatures needs more cooling**. The PWM comes from the RP1 PIO block, so it works on any GPIO, including pins with no hardware PWM.

> Design: [PLAN.md](PLAN.md). Measurements behind it: [RESEARCH.md](RESEARCH.md) and [sim/RESULTS.md](sim/RESULTS.md).

## Why

In a sealed case the NVMe heats up much faster than the CPU, and the official Active Cooler only reacts to the CPU. Measured under sustained load:

| | case fan off | case fan on |
|---|---|---|
| NVMe | 61 → 78.8 °C in 2.5 min | flat at ~70 °C |
| CPU | 67 °C, rising | ~57 °C |

## Design goals

- **Tiny:** std-only Rust, one static binary, ~0 CPU, sub-MB memory.
- **Fail-safe:** any sensor or PWM error drives the fan to 100 %.
- **Quiet:** 25 kHz PWM, hysteresis, and speed changes in 5 % steps.

## Hardware assumed

| Item | Value |
|---|---|
| Fan | 5 V 2-wire, switched low-side by an NPN transistor with a flyback diode |
| Control pin | GPIO27 (physical pin 13), configurable |
| Overlay | `dtoverlay=pwm-pio,gpio=27` in `/boot/firmware/config.txt` |

## Install

On the Pi (64-bit Raspberry Pi OS), as root:

```sh
curl -fsSL https://github.com/Shivamingale3/fan_ctl/releases/latest/download/install.sh | sudo sh
```

The installer:
- downloads the release and verifies its SHA-256 checksums;
- installs `/usr/local/bin/fan_ctl`, `/etc/systemd/system/fan_ctl.service` and `/etc/fan_ctl.conf.example`;
- adds `dtoverlay=pwm-pio,gpio=27` to `/boot/firmware/config.txt` (with a backup) and loads it right away, so no reboot is needed;
- self-tests, then enables and starts the service.

Options: `--version vX.Y.Z`, `--gpio N` (fan on another pin), `--no-overlay`, `--from DIR` (offline install from downloaded assets).

Run the same command again to upgrade.

## Use

```sh
journalctl -u fan_ctl -f            # watch decisions: "fan 65% (cpu 52.1 °C, nvme 58.9 °C)"
fan_ctl --dry-run --once -v         # what it would do now, touching nothing (no root needed)
sudo systemctl restart fan_ctl      # after editing /etc/fan_ctl.conf
```

### Manual control

```sh
fan_ctl status                     # mode, duty, temperatures, curves (no root needed)
sudo fan_ctl set 80                # 80 % for 1 h, then back to auto
sudo fan_ctl set 0 --for 30m       # off for 30 min
sudo fan_ctl set 100 --for forever # until fan_ctl stops or the Pi reboots
sudo fan_ctl auto                  # back to the curves now
```

Manual never beats the safety rules: a sensor error, or the NVMe / CPU reaching its full temperature (72 / 80 °C by default), still runs the fan at 100 % (released 3 °C below). A change takes effect within about a second.

Other programs can use the same two files in `/run/fan_ctl/` (removed when the service stops):

| File | Who writes | Content |
|---|---|---|
| `status` | fan_ctl, every step | `mode = auto\|manual\|safety\|failsafe`, `duty`, `auto`, `manual`, `until`, `cpu`, `nvme`, `min_duty`, `cpu_curve`, `nvme_curve`, `updated` (one `key = value` per line) |
| `override` | root | `duty = 0..100` and `until = <unix seconds>` (`0` = no end). Write it to a temporary name and rename it into place. A missing or invalid file means auto |

Tuning: `sudo cp /etc/fan_ctl.conf.example /etc/fan_ctl.conf`, uncomment what you want to change, then restart. Unknown keys are rejected with the line number.

### Behaviour

| Hotter of NVMe / CPU | Fan |
|---|---|
| ≤ 55 °C / ≤ 60 °C | 60 % (never off) |
| 55 → 72 °C / 60 → 80 °C | 60 → 100 % in 5 % steps |
| ≥ 72 °C / ≥ 80 °C | 100 % |

- **Fail-safe:** a sensor error, or the service stopping or crashing, leaves the fan at **100 %**. systemd restarts the daemon after 5 s.
- **Hysteresis:** slowing down needs a 3 °C drop, so the fan doesn't hunt.
- **Polling:** every 15 s at minimum speed, every 5 s above it. The PWM is only written on change.
- **Footprint:** ~0.5 MB RSS (96 kB private), ~0 % CPU, 420 KB static binary.

## Uninstall

```sh
curl -fsSL https://github.com/Shivamingale3/fan_ctl/releases/latest/download/install.sh | sudo sh -s -- --uninstall
```

This removes everything the installer added, including its `config.txt` lines, and leaves the fan at 100 % until reboot. `/etc/fan_ctl.conf` is kept.

## Building

```sh
# Static aarch64 binary from any Linux host, no C cross-toolchain needed:
rustup target add aarch64-unknown-linux-musl
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
  cargo build --release --target aarch64-unknown-linux-musl
```

Releases: push a tag `vX.Y.Z` that matches `Cargo.toml`. CI tests, builds and publishes the GitHub Release.

## License

[BSD-3-Clause](LICENSE), the same licence as Raspberry Pi's own tools (e.g. `raspi-utils`).
