# fan_ctl — Plan

Status: **implemented** (phases 1–5, PRs #1–#5). Facts come from [RESEARCH.md](RESEARCH.md).

## 1. Goal

Run the case fan (GPIO27, pin 13) at a speed set by **whichever of the NVMe and CPU is hotter**, mainly to protect the NVMe. It should use almost no resources and fail safe (fan at 100 % on any error).

## 2. How it works

```
 boot ─► config.txt: dtoverlay=pwm-pio,gpio=27   (PIO makes 25 kHz PWM on pin 13, fan off until service starts)
          │
          ▼
 systemd: fan_ctl.service (root, starts early, restarts on crash)
          │
          ▼
 ┌───────────── loop ─────────────┐
 │ read CPU temp  (hwmon cpu_thermal)
 │ read NVMe temp (hwmon nvme)
 │ duty = max(curve_nvme(T_nvme), curve_cpu(T_cpu))
 │ apply hysteresis + 5 % steps
 │ write duty_cycle ONLY if changed
 │ sleep 15 s (fan off/low) or 5 s (fan above low)
 └────────────────────────────────┘
          │ any read/write error ─► duty = 100 %, keep retrying
          ▼
 service stop / crash ─► ExecStopPost sets 100 %  (sysfs PWM keeps the value after exit)
```

## 3. Speed curves (simulation + bench test, see [sim/RESULTS.md](sim/RESULTS.md))

| NVMe °C | CPU °C | Fan duty |
|---|---|---|
| ≤ 55 | ≤ 60 | MIN = **60 %**, never off |
| 55 → 72 | 60 → 80 | linear MIN → 100 % |
| ≥ 72 | ≥ 80 | 100 % |

Expected (model + bench map): idle ≈ 52 °C at 65 %, 50 % load ≈ 62 °C at 80 %, full load ≈ 70 °C at 100 %.

**PWM: 25 kHz** (period 40000 ns). Lower frequencies whine (bench test, see sim/RESULTS.md).

- Final duty = the higher of the two.
- **Hysteresis 3 °C:** speed only drops once the temperature is 3 °C below the point that raised it, so the fan doesn't hunt.
- **Kick-start:** at service start, run 100 % for 2 s so the fan starts reliably from a standstill.
- Duty is rounded **up** to 5 % steps, so nothing is written for tiny changes.
- Fan floor: never off (user decision).

## 4. Fail-safe rules

| Situation | Action |
|---|---|
| Sensor file missing / unreadable | 100 %, rescan sensors next loop |
| NVMe hwmon not there yet at boot | use CPU only + 100 % until NVMe appears |
| PWM chip missing (overlay not loaded) | log error, exit non-zero → systemd restarts every 10 s |
| Service stopped or crashed | `ExecStopPost` writes duty = period (100 %) |
| NVMe ≥ 72 °C or CPU ≥ 80 °C | 100 % (curve covers it) |

## 5. Files

```
fan_ctl/
├── RESEARCH.md
├── PLAN.md
├── Cargo.toml              # no dependencies; release: opt-level="z", lto, codegen-units=1, panic="abort", strip
├── src/main.rs             # whole program, ~150 lines, std only
├── fan_ctl.service         # systemd unit (hardened, MemoryMax=8M)
├── install.sh              # run on the Pi: download release, verify sha256, install, enable
├── .github/workflows/release.yml
├── tests_data/             # thermal test logs
├── sim/                    # thermal model + curve simulation (model.py, curve.py, RESULTS.md)
└── README.md               # pins, curve, how to tune, how to uninstall
```

- Settings are constants at the top of `main.rs`. Changing them means a new release. Optional later: read `/etc/fan_ctl.conf`.
- Logging: one line to journald **only when the duty changes**, plus errors.

## 6. systemd unit (outline)

- `Type=simple`, `Restart=always`, `RestartSec=10`
- `ExecStart=/usr/local/bin/fan_ctl`
- `ExecStopPost=/usr/local/bin/fan_ctl --full` (fan to 100 % on any stop)
- Hardening: `NoNewPrivileges`, `ProtectSystem=strict`, `ReadWritePaths=/sys/class/pwm /sys/devices/platform`, `ProtectHome`, `PrivateTmp`, `MemoryMax=8M`
- `WantedBy=multi-user.target`

## 7. Build & release (GitHub Actions)

- Trigger: push of a tag `v*`.
- Runner: `ubuntu-24.04-arm` (native arm64), target `aarch64-unknown-linux-musl` → one static binary that runs on any Pi 5 OS.
  - Fallback if the arm runner is unavailable: `ubuntu-latest` + rust-lld cross-link.
- Steps: checkout → install the stable Rust toolchain with `rustup` → `cargo build --release --target aarch64-unknown-linux-musl` → check that `file` reports a static aarch64 binary → `sha256sum` → `gh release create $TAG fan_ctl fan_ctl.sha256 fan_ctl.service install.sh`.
- Permissions: `contents: write`. No third-party actions.

## 8. Rollout on the Pi

1. ✅ Bench test done 2026-11-21 (25 kHz, MIN 60 %, kick-start). Original steps:
   - 25 kHz at 100 / 70 / 50 / 35 / 25 % → find MIN that spins from a standstill and keeps spinning.
   - If it doesn't spin cleanly at 25 kHz, repeat at 5 kHz and 1 kHz.
2. Set MIN and frequency in `main.rs`, commit, tag `v0.1.0` → CI builds the release.
3. On the Pi: add `dtoverlay=pwm-pio,gpio=27` to `/boot/firmware/config.txt` (back up first).
4. Run `install.sh` → binary to `/usr/local/bin/fan_ctl`, unit to `/etc/systemd/system/`, `systemctl enable --now`.
5. Reboot test: fan comes on at boot, journald shows the duty lines.
6. Re-run the load test with the controller in charge. Pass = NVMe stays ≤ 72 °C under the same load, no fan hunting.
7. Measure: `ps -o rss` (target < 1.5 MB) and CPU time after 1 h (target ~0).

**Rollback:** `systemctl disable --now fan_ctl`, remove the overlay line and reboot → pin back to default (fan off), or use the `pinctrl set 27 op dh` line for always-on.

## 9. Decisions (2026-11-21)

1. Repo: **new public GitHub repo** `fan_ctl`. The Pi downloads releases with no token.
2. Fan floor: **never off**, MIN 60 % (bench test).
3. Curve: chosen by simulation (C), see §3.

## 10. Implementation phases (one PR each, one commit per logical change)

| Phase | PR | Contents |
|---|---|---|
| 1 | Scaffold | Cargo manifest (edition 2024, zero deps, size-optimised release profile), lint/format config, licence, README, CI (fmt, clippy, test, aarch64-musl build) |
| 2 | Control logic | `curve` (temperature → duty), `controller` (min duty, 5 % steps, hysteresis, kick-start, fail-safe). Pure code, fully unit-tested |
| 3 | Hardware I/O | `sensor` (hwmon discovery by name, read °C), `pwm` (find `pwm_pio` chip, export, period, duty). Sysfs root is injectable → tested against fake sysfs trees |
| 4 | Daemon | `config` (defaults + `/etc/fan_ctl.conf`, key = value), `log` (journald priority prefixes), `daemon` loop, CLI (`--config`, `--dry-run`, `--once`, `--version`) |
| 5 | Packaging & release | systemd unit, `install.sh` / uninstall, release workflow (tag `v*` → GitHub Release with static binary + sha256) |
| 6 | (later) Debian package | `.deb` for Raspberry Pi OS / apt |

Module layout: `src/{main,lib,config,curve,controller,sensor,pwm,daemon,log}.rs`, integration tests in `tests/`.
