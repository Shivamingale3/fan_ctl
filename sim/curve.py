"""Use the fitted model to (1) map fan duty -> steady NVMe temp and
(2) closed-loop-simulate candidate temp->duty curves exactly as fan_ctl will run them."""
import math, os
from model import Model, P_N, P_S, airflow

HERE = os.path.dirname(os.path.abspath(__file__))
m = Model([float(x) for x in open(os.path.join(HERE, "params.txt")).read().split()])

# mixed load: interpolate powers between idle and full load
def set_load(frac):
    P_N["mid"] = P_N["idle"] + frac * (P_N["load"] - P_N["idle"])
    P_S["mid"] = P_S["idle"] + frac * (P_S["load"] - P_S["idle"])

print("Steady NVMe °C by fan duty (model):")
print("duty   idle   25%load  50%load  full")
for d in (0, 30, 40, 50, 60, 70, 80, 90, 100):
    row = [m.steady(d / 100, "idle")[0]]
    for fr in (0.25, 0.5):
        set_load(fr); row.append(m.steady(d / 100, "mid")[0])
    row.append(m.steady(d / 100, "load")[0])
    print(f"{d:3d}%  " + "  ".join(f"{x:6.1f}" for x in row))

def controller(curve, min_duty=30, step=5, hyst=3.0):
    """Same logic planned for main.rs: duty from curve, round up to step, drop only after hyst."""
    state = {"duty": 100}
    def duty_at(t):
        (t0, d0), (t1, d1) = curve
        if t <= t0: d = d0
        elif t >= t1: d = d1
        else: d = d0 + (d1 - d0) * (t - t0) / (t1 - t0)
        d = max(min_duty, min(100, d))
        return min(100, math.ceil(d / step) * step)
    def ctl(t):
        new = duty_at(t)
        if new < state["duty"]:
            new = max(new, min(state["duty"], duty_at(t + hyst)))
        state["duty"] = new
        return new
    return ctl

PROFILE = [("idle", 0, 900), ("mid", 0.5, 900), ("load", 1, 1200), ("idle", 0, 1200)]

def closed_loop(curve, interval_off=15, interval_on=5):
    ctl = controller(curve)
    tn, tc = m.steady(1.0, "idle"); duty = 100; t = 0; next_sample = 0
    log = {}; changes = 0
    for mode, fr, dur in PROFILE:
        if mode == "mid": set_load(fr)
        peak = 0; ds = []
        for _ in range(dur):
            if t >= next_sample:
                nd = ctl(tn)
                if nd != duty: changes += 1
                duty = nd; next_sample = t + (interval_off if duty <= 30 else interval_on)
            tn, tc = m.step(tn, tc, duty / 100, mode)
            peak = max(peak, tn); ds.append(duty); t += 1
        log[f"{mode}{'' if mode != 'mid' else int(fr*100)}"] = (tn, peak, sum(ds[-300:]) / 300)
    return log, changes

CANDIDATES = {
    "A plan  50→70": ((50, 30), (70, 100)),
    "B early 45→65": ((45, 30), (65, 100)),
    "C late  55→72": ((55, 30), (72, 100)),
    "D wide  45→70": ((45, 30), (70, 100)),
}
print("\nClosed loop: idle 15 min → 50% load 15 min → full load 20 min → idle 20 min")
print("curve            | idle end °C/duty | 50% load end °C/duty | full peak °C/duty | idle-after °C/duty | duty changes")
for name, c in CANDIDATES.items():
    log, ch = closed_loop(c)
    f = lambda k: f"{log[k][0]:5.1f}/{log[k][2]:3.0f}%"
    print(f"{name:16s} | {f('idle'):16s} | {f('mid50'):20s} | {log['load'][1]:5.1f}/{log['load'][2]:3.0f}%       | {log['idle'][0]:5.1f}/{log['idle'][2]:3.0f}%        | {ch}")
