# fan_ctl — Plan

Status: **DRAFT, waiting for "go".** No code yet. Facts come from [RESEARCH.md](RESEARCH.md).

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

## 3. Speed curves (first guess, tuned after the bench test)

| NVMe °C | CPU °C | Fan duty |
|---|---|---|
| < 50 | < 55 | 0 % (off) |
| 50 | 55 | MIN (≈35 %, from bench test) |
| 50 → 70 | 55 → 75 | linear MIN → 100 % |
| ≥ 70 | ≥ 75 | 100 % |

- Final duty = the higher of the two.
- **Hysteresis 3 °C:** speed only drops once the temperature is 3 °C below the point that raised it, so the fan doesn't hunt.
- **Kick-start:** going from 0 % to anything, run 100 % for 1 s first so the fan starts reliably.
- Duty is rounded **up** to 5 % steps, so nothing is written for tiny changes.
- **Open:** fan floor. NVMe idles ~61 °C, so with these numbers the fan will rarely be off at all. Option: a fixed low floor (e.g. MIN) and never fully off.

## 4. Fail-safe rules

| Situation | Action |
|---|---|
| Sensor file missing / unreadable | 100 %, rescan sensors next loop |
| NVMe hwmon not there yet at boot | use CPU only + 100 % until NVMe appears |
| PWM chip missing (overlay not loaded) | log error, exit non-zero → systemd restarts every 10 s |
| Service stopped or crashed | `ExecStopPost` writes duty = period (100 %) |
| Temp ≥ 75 °C either sensor | 100 % (curve covers it) |

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
└── README.md               # pins, curve, how to tune, how to uninstall
```

- Settings are constants at the top of `main.rs`. Changing them means a new release. Optional later: read `/etc/fan_ctl.conf`.
- Logging: one line to journald **only when the duty changes**, plus errors.

## 6. systemd unit (outline)

- `Type=simple`, `Restart=always`, `RestartSec=10`
- `ExecStart=/usr/local/bin/fan_ctl`
- `ExecStopPost=` shell one-liner: find `pwm_pio` chip, `cat period > duty_cycle`
- Hardening: `NoNewPrivileges`, `ProtectSystem=strict`, `ReadWritePaths=/sys/class/pwm /sys/devices/platform`, `ProtectHome`, `PrivateTmp`, `MemoryMax=8M`
- `WantedBy=multi-user.target`

## 7. Build & release (GitHub Actions)

- Trigger: push of a tag `v*`.
- Runner: `ubuntu-24.04-arm` (native arm64), target `aarch64-unknown-linux-musl` → one static binary that runs on any Pi 5 OS.
  - Fallback if the arm runner is unavailable: `ubuntu-latest` + rust-lld cross-link.
- Steps: checkout → install the stable Rust toolchain with `rustup` → `cargo build --release --target aarch64-unknown-linux-musl` → check that `file` reports a static aarch64 binary → `sha256sum` → `gh release create $TAG fan_ctl fan_ctl.sha256 fan_ctl.service install.sh`.
- Permissions: `contents: write`. No third-party actions.

## 8. Rollout on the Pi

1. Bench test (before final constants), user listens:
   - 25 kHz at 100 / 70 / 50 / 35 / 25 % → find MIN that spins from a standstill and keeps spinning.
   - If it doesn't spin cleanly at 25 kHz, repeat at 5 kHz and 1 kHz.
2. Set MIN and frequency in `main.rs`, commit, tag `v0.1.0` → CI builds the release.
3. On the Pi: add `dtoverlay=pwm-pio,gpio=27` to `/boot/firmware/config.txt` (back up first).
4. Run `install.sh` → binary to `/usr/local/bin/fan_ctl`, unit to `/etc/systemd/system/`, `systemctl enable --now`.
5. Reboot test: fan comes on at boot, journald shows the duty lines.
6. Re-run the load test with the controller in charge. Pass = NVMe stays ≤ 72 °C under the same load, no fan hunting.
7. Measure: `ps -o rss` (target < 1.5 MB) and CPU time after 1 h (target ~0).

**Rollback:** `systemctl disable --now fan_ctl`, remove the overlay line and reboot → pin back to default (fan off), or use the `pinctrl set 27 op dh` line for always-on.

## 9. Decisions needed from you

1. **GitHub repo:** new repo `fan_ctl`, public (the Pi downloads with no token) or private (needs a token on the Pi)? Or a folder in the existing private repo?
2. **Fan floor:** allow fully off below 50 °C NVMe / 55 °C CPU, or always at least MIN?
3. **Curve numbers** in §3: OK as a starting point?
