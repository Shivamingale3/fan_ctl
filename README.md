# fan_ctl

Temperature-driven case fan controller for the **Raspberry Pi 5**.

It sets the speed of a 2-wire case fan from **whichever of the CPU and NVMe temperatures needs more cooling**. The PWM comes from the RP1 PIO block, so it works on any GPIO, including pins with no hardware PWM.

> Status: under development. See [PLAN.md](PLAN.md) for the design and phases, and [RESEARCH.md](RESEARCH.md) for the measurements behind it.

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

## Building

```sh
cargo build --release --target aarch64-unknown-linux-musl
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
