"""Two-node thermal model of the sealed case, fitted to tests_data/run1.csv + run3.csv.

  NVMe node : C_n dTn/dt = P_n           - G_n(f) (Tn - Tc)
  Case air  : C_c dTc/dt = P_n + P_sys   - G_c(f) (Tc - Troom)
  G(f) = G0 + G1 * f**0.8          (forced convection ~ airflow^0.8)
  f    = airflow fraction 0..1     (duty->airflow map measured in bench test)

Powers are fixed from typical figures (NVMe idle 1.5 W / load 4 W, rest of Pi
idle 3 W / load 9 W); conductances, heat capacities and room temp are fitted.
Pure Python (no numpy on this machine).
"""
import csv, math, os, random

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "..", "tests_data")

P_N = {"idle": 1.5, "load": 4.0}
P_S = {"idle": 3.0, "load": 9.0}

def load_run(name):
    rows = [r for r in csv.reader(open(os.path.join(DATA, name))) if len(r) > 4 and r[0].isdigit()]
    return [(int(r[0]), r[1], int(r[2]) / 100, int(r[4]) / 10) for r in rows]

def phase_load(ph):
    return "load" if ph.startswith("load") else "idle"

# Measured by ear, bench test 2026-11-21 @ 25 kHz: 100%=full, 70%~half, 60%~0.38, 50%~quarter.
# Fit data only contains 0 % and 100 %, so this map doesn't change the fitted parameters.
AIRFLOW_PTS = [(0.0, 0.0), (0.45, 0.0), (0.50, 0.25), (0.60, 0.38), (0.70, 0.50), (1.0, 1.0)]
def airflow(duty):
    for (x0, y0), (x1, y1) in zip(AIRFLOW_PTS, AIRFLOW_PTS[1:]):
        if duty <= x1: return y0 + (y1 - y0) * (duty - x0) / (x1 - x0)
    return 1.0

class Model:
    names = ["Gn0", "Gn1", "Gc0", "Gc1", "Cn", "Cc", "Troom", "Tc0_run3"]
    def __init__(self, p): self.p = dict(zip(self.names, p))
    def G(self, f):
        p = self.p
        return p["Gn0"] + p["Gn1"] * f ** 0.8, p["Gc0"] + p["Gc1"] * f ** 0.8
    def steady(self, duty, mode):
        gn, gc = self.G(airflow(duty))
        tc = self.p["Troom"] + (P_N[mode] + P_S[mode]) / gc
        return tc + P_N[mode] / gn, tc
    def step(self, tn, tc, duty, mode, dt=1.0):
        p = self.p; gn, gc = self.G(airflow(duty))
        q = gn * (tn - tc)
        tn += dt * (P_N[mode] - q) / p["Cn"]
        tc += dt * (q + P_S[mode] - gc * (tc - p["Troom"])) / p["Cc"]
        return tn, tc

def simulate(m, run, tn0, tc0):
    out = []; tn, tc = tn0, tc0; t_prev = run[0][0]
    for t, ph, duty, meas in run:
        for _ in range(t - t_prev): tn, tc = m.step(tn, tc, duty, phase_load(ph))
        t_prev = t; out.append(tn)
    return out

RUN1, RUN3 = load_run("run1.csv"), load_run("run3.csv")

def cost(p):
    if min(p) <= 0: return 1e9
    m = Model(p)
    tn1, tc1 = m.steady(0.0, "idle")           # run1 starts at idle equilibrium, fan off
    s1 = simulate(m, RUN1, tn1, tc1)
    s3 = simulate(m, RUN3, RUN3[0][3], p[7])
    e = sum((a - b[3]) ** 2 for a, b in zip(s1, RUN1)) + sum((a - b[3]) ** 2 for a, b in zip(s3, RUN3))
    e += 50 * (tn1 - 61.0) ** 2                # idle fan-off steady ~61 °C (measured)
    return e / (len(RUN1) + len(RUN3))

def nelder_mead(f, x0, step, iters=4000):
    n = len(x0); pts = [x0] + [[x + (step[i] if j == i else 0) for j, x in enumerate(x0)] for i in range(n)]
    vals = [f(p) for p in pts]
    for _ in range(iters):
        order = sorted(range(n + 1), key=lambda i: vals[i]); pts = [pts[i] for i in order]; vals = [vals[i] for i in order]
        c = [sum(p[i] for p in pts[:-1]) / n for i in range(n)]
        xr = [c[i] + (c[i] - pts[-1][i]) for i in range(n)]; fr = f(xr)
        if fr < vals[0]:
            xe = [c[i] + 2 * (c[i] - pts[-1][i]) for i in range(n)]; fe = f(xe)
            pts[-1], vals[-1] = (xe, fe) if fe < fr else (xr, fr)
        elif fr < vals[-2]: pts[-1], vals[-1] = xr, fr
        else:
            xc = [c[i] + 0.5 * (pts[-1][i] - c[i]) for i in range(n)]; fc = f(xc)
            if fc < vals[-1]: pts[-1], vals[-1] = xc, fc
            else:
                pts = [pts[0]] + [[pts[0][i] + 0.5 * (p[i] - pts[0][i]) for i in range(n)] for p in pts[1:]]
                vals = [vals[0]] + [f(p) for p in pts[1:]]
    return pts[0], vals[0]

def fit():
    random.seed(1); best = None
    x0 = [0.15, 0.25, 0.25, 0.35, 30.0, 900.0, 27.0, 40.0]
    for k in range(6):
        start = [x * random.uniform(0.6, 1.5) for x in x0] if k else x0
        p, v = nelder_mead(cost, start, [x * 0.3 for x in start])
        if best is None or v < best[1]: best = (p, v)
    return best

if __name__ == "__main__":
    p, v = fit()
    print(f"rms error = {v ** 0.5:.2f} °C")
    for k, x in zip(Model.names, p): print(f"  {k:9s} = {x:8.3f}")
    with open(os.path.join(HERE, "params.txt"), "w") as fh: fh.write(" ".join(f"{x:.6f}" for x in p))
