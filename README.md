# kave-thermalized

Rust thermal engine and `android.hardware.thermal.IThermal` AIDL HAL for the
Redmi Turbo 3 (peridot, SM8635).

The service registers:

```
android.hardware.thermal.IThermal/default
```

It combines a userspace thermal policy engine with a VINTF-stable AIDL HAL. The
engine reads Qualcomm thermal zones and cooling devices through sysfs, applies
frequency and feature limits in steps, and publishes temperatures, thresholds,
cooling-device states, and throttling callbacks to Android.

The repository is named `kave-thermalized`; the built binary remains
`hoshikv-thermal`.

## Features

### AIDL thermal HAL

- Implements `IThermal` as a Rust binder service through `rsbinder`.
- Registers `android.hardware.thermal.IThermal/default`.
- Supports temperature and cooling-device queries, with and without type
  filtering.
- Publishes static hot-threshold tables for every sensor.
- Supports registration and unregistration of thermal-change callbacks.
- Supports optional callback filtering by `TemperatureType`.
- Rejects duplicate callback registrations.
- Runs the binder thread pool separately from the control loop.
- Reports callback events when a sensor's throttling severity changes.

Published sensors:

| Type | Sensors |
|---|---|
| CPU | `CPU0` through `CPU7`, sourced from per-core thermal zones |
| GPU | `GPU0` through `GPU3` |
| NPU | `nsp0` through `nsp3` |
| Battery | `battery` from the battery power supply |
| Skin | `skin` and `shell_skin` from the weighted virtual temperature |
| BCL | `socd` percentage and `vbat` voltage |
| Cooling devices | `cpufreq-cpu0`, `cpufreq-cpu3`, `cpufreq-cpu7`, `gpu`, `battery` |

Each temperature is reported with one of the standard throttling severities:
`NONE`, `LIGHT`, `MODERATE`, `SEVERE`, `CRITICAL`, `EMERGENCY`, or `SHUTDOWN`.

### Thermal sensing

- Probes thermal zones by type instead of hard-coding zone numbers.
- Reads CPU, GPU, battery, charger, Wi-Fi, PA, quiet, connection, flash, modem,
  hotspot, and per-cluster junction sensors.
- Uses fallback values when optional zones are missing and logs unavailable
  nodes.
- Builds a weighted virtual temperature from CPU, battery, charger, Wi-Fi, PA0,
  PA1, quiet, connection, and hotspot readings.
- Uses the battery power-supply temperature when it reports a usable value.
- Reads screen state for the load tracker.
- Converts per-cluster junction readings into CPU temperatures with an 8 C
  offset and never reports below the main CPU thermal zone.

### Stepped thermal policy

The main engine runs once per second and applies stepped policies for:

- CPU cluster 0, cluster 1, and cluster 2 frequency caps.
- GPU maximum frequency.
- CDSP cooling state.
- LMH cooling state.
- CPU hotplug/pause states for CPUs 3, 4, and 7.
- Thermal message state.
- Torch level.
- Wi-Fi and hotspot limits.
- Maximum display brightness.
- Modem limit.
- Voice limit.
- Board temperature reporting.

Each policy has separate trigger, clear, and target arrays. Hysteresis is handled
by clearing at a lower temperature than the trigger.

CPU and GPU inputs can come from the bundled configuration. Default frequency
tables are tuned for peridot and can be replaced without rebuilding.

### Adaptive guard and load tracking

- Tracks CPU utilization from `/proc/stat`.
- Tracks average CPU frequency across the available policies.
- Tracks GPU busy time from KGSL nodes when available.
- Combines CPU load, CPU frequency, and GPU load into a smoothed load score.
- Classifies load as quiescent, background, active, heavy, or saturated.
- Adds dwell-time filtering to avoid rapid load-class changes.
- Evaluates CPU, board, and battery risk as nominal, elevated, high, critical, or
  thermal.
- Applies emergency frequency caps when CPU, board, or battery temperature
  reaches critical levels.
- Rate-limits repeated protection trips.
- Temporarily disables adaptive control after 12 thermal violations within one
  hour.
- Restores adaptive control after the violation count falls below four.
- Enters failsafe after ten consecutive bad sensor readings and releases all
  applied limits.
- Releases CPU, GPU, CDSP, LMH, hotplug, display, modem, and charging limits on
  shutdown or sensor failure.

### Charging thermal mitigation

- Reads battery temperature and USB online state.
- Tracks the battery-temperature rate of change with a smoothed estimate.
- Ramps the constant charge current down between 42 C and 50 C.
- Stops charge-current requests at 50 C.
- Uses the current `constant_charge_current` value as the maximum FCC at
  startup.
- Leaves charging untouched while USB is offline.

### Power and frequency tuning

- Applies per-policy maximum frequencies for policies 0, 3, 6, and 7.
- Snaps requested frequencies to values present in the kernel's available
  frequency table.
- Restores the kernel minimum frequency after boot-time modifications.
- Enables temperature-gated frequency caps with a configurable release margin.
- Supports an optional boot guard with separate boot-time frequency caps.
- Supports `schedutil`, `conservative`, `walt`, `powersave`, and `performance`
  governors.
- Applies WALT up/down rate limits when WALT is selected.
- Sets `swappiness`, dirty ratios, and disables CPU boost when requested.
- Applies Android properties on the first tuning pass.
- Reapplies important values every ten seconds instead of spinning the loop.

### Observability

- Logs to stdout and `/dev/kmsg` with the prefix `hoshikv-thermal:`.
- Prints a compact status line every two seconds.
- Writes a rolling status history to `/data/vendor/thermal/custom.dump`.
- Keeps the last 128 status lines in the dump.
- Includes sensor temperatures, policy levels, load class, guard state, charging
  temperature, and configured power limits in the status output.
- Logs every sysfs write result, including failures.
- Handles `SIGINT` and `SIGTERM` by releasing all managed controls.

## Configuration

The engine loads a JSON configuration from:

1. `/data/vendor/thermal/thermal_config.json`
2. `/vendor/etc/thermal_config.json`

If neither file is usable, it writes a built-in default configuration to
`/data/vendor/thermal/thermal_config.json` and uses that copy.

Only the sections currently consumed by the engine are documented here.
Legacy or placeholder keys in the bundled file are intentionally omitted.

### Active keys

| Section | Keys | Purpose |
|---|---|---|
| `weights` | `cpu`, `batt`, `chg`, `wifi`, `pa0`, `pa1`, `quiet`, `conn`, `hotspot` | Weights for virtual temperature |
| `cpu0`, `cpu3`, `cpu7` | `trig`, `clr`, `targets` | CPU cluster step policies |
| `gpu` | `trig`, `clr`, `freqs` | GPU step policy and frequency table |
| `power` | `max_freq_p0`, `max_freq_p3`, `max_freq_p6`, `max_freq_p7` | Per-policy frequency caps |
| `power` | `gate_temp` | Temperature that enables frequency caps |
| `power` | `governor` | CPU governor selection |
| `power` | `swappiness`, `dirty_ratio`, `dirty_bg` | VM tuning |
| `power` | `boot_guard_s`, `boot_max_p0`, `boot_max_p3`, `boot_max_p6`, `boot_max_p7` | Optional boot-time frequency cap |
| `power` | `props` | Android properties applied at startup |

`trig`, `clr`, and `targets` arrays represent temperature steps, release
thresholds, and output values. Trigger and clear arrays must use the same step
count. CPU targets are in kHz and GPU targets are in Hz.

## Project structure

```
.
├── aidl/android/hardware/thermal/
│   ├── CoolingDevice.aidl
│   ├── CoolingType.aidl
│   ├── IThermal.aidl
│   ├── IThermalChangedCallback.aidl
│   ├── Temperature.aidl
│   ├── TemperatureThreshold.aidl
│   ├── TemperatureType.aidl
│   └── ThrottlingSeverity.aidl
├── src/
│   ├── main.rs        # entrypoint, control loop, engine, charging, dump
│   ├── hal.rs         # IThermal implementation and callback registry
│   ├── sensors.rs     # thermal-zone probing and virtual temperature
│   ├── policy.rs      # stepped trigger/clear policy
│   ├── guard.rs       # emergency caps, rate limiting, failsafe inputs
│   ├── loadstat.rs    # CPU/GPU load tracking and classification
│   ├── tuner.rs       # frequency, governor, VM, and property tuning
│   ├── config.rs      # JSON loading, schema, and built-in seed
│   └── sysfs.rs       # sysfs node and cooling-device helpers
├── prebuilt/
│   ├── hoshikv-thermal-v2.0
│   ├── hoshikv-thermal-v2.1
│   ├── hoshikv-thermal-v2.2
│   ├── hoshikv-thermal-v2.3
│   └── hoshikv-thermal-v2.4
├── build.rs
├── Cargo.toml
├── Cargo.lock
├── thermal_config.json
├── .cargo/config.toml
└── README.md
```

## Build

Requirements:

- Rust toolchain with Cargo.
- Android NDK.
- `aarch64-linux-android` target support through the NDK linker.

The checked-in `.cargo/config.toml` points to:

```
/opt/android-ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android28-clang
```

Adjust that path if the NDK is installed elsewhere, then build:

```bash
cargo build --release
```

The default build target is `aarch64-linux-android`. The binary is written to:

```
target/aarch64-linux-android/release/hoshikv-thermal
```

The AIDL bindings are generated by `build.rs` using `rsbinder-aidl`. The
interface files are compiled with AIDL version 2 and synchronous methods.

## Deployment notes

- The binary is a 64-bit Android executable.
- It needs access to Qualcomm thermal zones, cooling devices, battery power
  supply nodes, KGSL devfreq nodes, and `/dev/kmsg`.
- Those accesses normally require vendor/root privileges and matching SELinux
  policy.
- The engine writes `/data/vendor/thermal/custom.dump` at runtime.
- Configuration can be shipped in `/vendor/etc/thermal_config.json` or placed
  at `/data/vendor/thermal/thermal_config.json`, which takes precedence.
- No init service definition or vendor makefile is included. Add the binary to
  the vendor image using the integration expected by the target ROM.
- The HAL does not replace the kernel thermal driver; it is a userspace policy
  layer over the existing thermal interfaces.

## Compatibility

The sysfs paths and frequency tables are tuned for peridot/SM8635. Other
Qualcomm devices can reuse the engine, but the sensor names, cooling-device
names, and frequency tables must match the target kernel.

## AIDL licensing

The AIDL interface definitions are derived from AOSP and retain their original
Apache-2.0 copyright headers.
