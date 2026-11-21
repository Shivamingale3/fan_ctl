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
