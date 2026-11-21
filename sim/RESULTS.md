# Simulation results (2026-11-21)

Model: two-node (NVMe chip + case air), fitted to `tests_data/run1.csv` + `run3.csv`. See `model.py`.

- Fit error: **0.45 °C rms** over both runs.
- Holdout check: model predicts idle with fan 100 % = 49.9 °C; measured overnight = 45.9 °C. The room was cooler at night, so this is plausible.
- The fan mostly **ventilates the case air** (Gc1 0.91 W/K) and barely blows on the NVMe chip itself (Gn1 ≈ 0).
- Assumptions: airflow is linear in duty above a 20 % stall point (check in the bench test). Powers are typical figures, so "Troom" (38 °C) is a lumped value, not the real room temperature.

## Steady NVMe °C by duty (`curve.py`)

| duty | idle | 25 % load | 50 % load | full load |
|---|---|---|---|---|
| 0 % | 61.0 | 71.2 | 81.5 | 102 (would throttle) |
| 30 % | 55.8 | 63.6 | 71.4 | 87.1 |
| 40 % | 53.8 | 60.7 | 67.6 | 81.3 |
| 50 % | 52.6 | 58.9 | 65.2 | 77.9 |
| 70 % | 51.1 | 56.8 | 62.4 | 73.6 |
| 100 % | 49.9 | 54.9 | 60.0 | 70.1 |

- **Idle:** above ~40 % the fan gains only ~1–4 °C, so a low speed is enough.
- **Full load:** only 100 % keeps the NVMe ~70 °C.

## Closed-loop test of candidate curves

Profile: idle 15 min → 50 % load 15 min → full load 20 min → idle 20 min. 5 s / 15 s sampling, 5 % steps, 3 °C hysteresis, min 30 %.

| curve (30 % at → 100 % at) | idle °C / duty | 50 % load °C / duty | full-load peak °C / duty |
|---|---|---|---|
| A 50 → 70 | 52.7 / 50 % | 61.8 / 75 % | 70.0 / 100 % |
| B 45 → 65 | 51.6 / 65 % | 60.7 / 88 % | 70.0 / 100 % |
| **C 55 → 72** | **54.0 / 40 %** | **62.8 / 65 %** | **70.5 / 95 %** |
| D 45 → 70 | 51.9 / 60 % | 61.3 / 80 % | 70.0 / 100 % |

**Chosen: C.** It's the quietest at idle and medium load, only ~1 °C warmer than the others, and it reaches 100 % at full load. On a hot day all temperatures shift up, so the fan simply reaches 100 % sooner.

CPU curve: 60 → 80 °C. The Active Cooler handles the CPU; this curve is a backstop for a hot case.

---

# Bench test (2026-11-21, by ear) → curve updated

| Setting | Heard |
|---|---|
| 25 kHz, 100 % | full speed, silent PWM |
| 25 kHz, 70 % | ≈ half speed |
| 25 kHz, 60 % | slower than 70 %, still useful |
| 25 kHz, 50 % | ≈ half of 70 %, "almost useless" |
| 5 kHz, 50 % | faster, but **whines** |
| 1 kHz, 50 % | faster, but **whines** |
| 25 kHz, 0 → 60 % from standstill | short hesitation ("trying to start"), then spins |

Decisions:
- **Frequency 25 kHz** (silent). Lower frequencies spin faster but whine.
- **MIN duty 60 %.** It starts from a standstill, but only just → kick-start at 100 % for 2 s at service start.
- The duty → airflow map from these points is now in `model.py`.

Re-simulated (`python3 sim/curve.py`), MIN 60 %:

| curve (60 % at → 100 % at) | idle °C / duty | 50 % load °C / duty | full-load peak °C / duty | duty changes in 70 min |
|---|---|---|---|---|
| **C 55 → 72 (chosen)** | **52.4 / 65 %** | **61.9 / 80 %** | **70.1 / 100 %** | 15 |
| E 52 → 70 | 52.0 / 70 % | 61.3 / 85 % | 70.0 / 100 % | 14 |
| F 50 → 68 | 51.5 / 75 % | 61.2 / 85 % | 70.0 / 100 % | 12 |
| G 55 → 70 | 52.4 / 65 % | 61.8 / 80 % | 70.0 / 100 % | 16 |

C keeps the same temperatures as the others with the quietest idle speed.

---

# Validation: run 4 with `fan_ctl` v0.1.0 in charge (2026-11-21)

Same load as runs 1 and 3 (4 × sha256 + NVMe direct-I/O reads + periodic 1 GB writes), same sealed case. The installed systemd service drove the fan; the test only logged. Raw log: `tests_data/run4.csv`.

| t | NVMe | CPU | fan (chosen by fan_ctl) |
|---|---|---|---|
| idle | 43.8 °C | 33.6 °C | 60 % |
| +0:20 load | 56.8 °C | 44.7 °C | 65 % |
| +1:00 | 61.8 °C | 50.2 °C | 80 % |
| +2:40 | 65.8 °C | 54.0 °C | 90 % |
| +4:15 | 67.8 °C | 56.8 °C | 95 % |
| +12:00 (end of load) | 69.8 °C | 57.8 °C | 95 % |
| +4:00 after load | 65.8 °C | 37.5 °C | 95 % (hysteresis holding) |

| | run 1: fan off | run 3: fan 100 % fixed | **run 4: fan_ctl** | model (curve C) |
|---|---|---|---|---|
| NVMe peak under load | 78.8 °C at 2.5 min (abort) | ~70 °C | **69.8 °C** | 70.1 °C |
| CPU peak | 67 °C | ~57 °C | **59.5 °C** | — |
| Active Cooler | high (175) | low (75) | low (75) | — |

- The steady state matches the model to within 0.3 °C.
- The fan climbed in single 5 % steps (60 → 95 %) with no hunting.
- After the load, the NVMe cools slowly (case air holds heat). The 3 °C hysteresis kept 95 % until the NVMe fell below ~66 °C.
- Service footprint during the test: VmRSS 512 kB; CPU time after 19 min still rounds to 0 s.

## Reboot test (2026-11-21)

```
[ 3.754] systemd: Starting fan_ctl.service
[ 3.832] fan_ctl: fan_ctl 0.1.0 starting: nvme 55→72 °C, cpu 60→80 °C, min 60%, 25000 Hz
[ 3.833] fan_ctl: driving /sys/class/pwm/pwmchip2/pwm0
[ 5.833] fan_ctl: fan 85% (cpu 40.2 °C, nvme 63.8 °C)     ← after the 2 s kick
[20.844] fan_ctl: fan 90% (cpu 43.0 °C, nvme 65.8 °C)
```

- The overlay came from `config.txt` at boot (`dtoverlay -l` shows no runtime overlays), and GPIO27 is `PIO27`.
- The fan is under control **~5.8 s after power-on**. Before the service starts, the pin sits low, so the fan is off for those few seconds.
- No failed units. The other services were unchanged apart from on-demand ones (`getty@tty2`, `systemd-hostnamed`).
