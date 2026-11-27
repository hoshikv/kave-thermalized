# kave-thermalized (`hoshikv-thermal`)

Rust implementation of the `android.hardware.thermal.IThermal/default` AIDL HAL and embedded Xiaomi `mi_thermald` policy engine for **Redmi Turbo 3 / POCO F6 (`peridot`, SM8635)** running ColorOS.

Replaces both `android.hardware.thermal-service.qti` and stock `/vendor/bin/mi_thermald` with a single native binary (`/vendor/bin/hoshikv-thermal`).

---

## Features

- **Unified AIDL Thermal HAL + Policy Engine**: Runs `android.hardware.thermal.IThermal/default` and the `mi_thermald` policy worker pool in one process (no external `mi_thermald` daemon).
- **Stock Xiaomi Encrypted Policy Support**: Decrypts and parses stock `/odm/etc/thermal-*.conf`, `/vendor/etc/thermal-map.conf`, and `/vendor/etc/thermald-devices.conf` (AES-128-CBC and plaintext INI).
- **Full `mi_thermald` Worker Types**: Implements `monitor`, `ss` (stepped), `sic` (PID/stepped battery/skin), and `chrono` algorithms with multi-device arbitration (`select_higher` / `select_lower`).
- **Dynamic `sconfig` Profile Switching**: Watches `/sys/class/thermal/thermal_message/sconfig` every 500 ms and hot-reloads the matching profile from `thermal-map.conf` while preserving sensor history and resetting unused cooling limits.
- **ColorOS Power Mode Sync**: Watches ColorOS battery/performance settings directly via `mtime`-cached Android Binary XML (ABX) reads (zero subprocesses, zero Binder overhead) and syncs them to `sconfig`:
  - **Power Saving Mode** (`low_power=1`) → `sconfig = 5` (`thermal-phone.conf`)
  - **Balanced / Default** → `sconfig = 0` (`thermal-normal.conf`)
  - **High Performance / GT Mode** (`high_performance_mode_on=1` or `gt_mode_state_setting=1`) → `sconfig = 50` (`thermal-per-normal.conf`)
  - Manual `sconfig` overrides (e.g. `echo 20 > sconfig`) are preserved until the user switches modes in Settings.
- **Late-Registering Sensor Recovery**: Lazily re-probes thermal zones (`battery`, `/sys/class/power_supply/battery/temp`, etc.) and cooling devices every 5 s if they were not ready during early boot, and normalizes decidegree battery readings (`375` → `37500` m°C).
- **Dynamic Idle 60 Hz (No OPlus VRR)**: Dedicated `idle-fps` thread polls the touchscreen `/dev/input/eventX` via `poll()` (0% CPU at rest) and switches SurfaceFlinger directly over Binder (`ISurfaceComposer` transaction `1035`, `group=0` seamless): drops to **60 Hz** after 3 s without touch, wakes to **120 Hz** in `<1 ms` on touch, and auto-releases override when screen is off/AOD, Standard 60 Hz is selected, or High Performance / GT mode is active. Configurable via `HOSHIKV_IDLE_FPS=0` or `HOSHIKV_IDLE_FPS_MS`.
- **Stock `VIRTUAL-SENSOR0` Skin Model**: Computes virtual skin temperature using stock `peridot` weights (`cpu=-26`, `batt=301`, `chg=147`, `wifi=330`, `pa0=36`, `pa1=119`, `quiet=-2`, `weight_sum=1000`, `compensation=2015`).

---

## Architecture

One process (`/vendor/bin/hoshikv-thermal`, ~660 KB stripped) with three runtime roles:

1. **Main loop (`hoshikv-thermal`)** — Runs every 1000 ms, reads thermal zones + cooling device states (read-only), updates `SharedState`, and dispatches AIDL `IThermalChangedCallback` notifications when throttling severity changes.
2. **Binder pool (`binder-pool`)** — Registers and serves `android.hardware.thermal.IThermal/default` via `rsbinder`.
3. **Policy engine (`mi-thermald`)** — Loads the active vendor INI policy, spawns algorithm worker threads, initializes `sconfig` from `-1` to the current ColorOS mode (`0`/`5`/`50`), and polls `sconfig` + ColorOS settings every 500 ms.

Logs go to stdout and `/dev/kmsg` with the `hoshikv-thermal:` prefix.

---

## AIDL HAL Interface

### Exposed Temperatures

| Type | Names | Kernel Zones / Source |
|---|---|---|
| `CPU` | `CPU0`–`CPU7` | `cpu-0-0-0`, `cpu-0-1-0`, `cpu-0-2-0`, `cpu-1-0-0`, `cpu-1-1-0`, `cpu-1-2-0`, `cpu-1-3-0`, `cpu-2-0-0` |
| `GPU` | `GPU0`–`GPU3` | `gpuss-0`–`gpuss-3` |
| `NPU` | `nsp0`–`nsp3` | `nsphvx-0`, `nsphvx-1`, `nsphmx-0`, `nsphmx-1` |
| `BATTERY` | `battery` | `/sys/class/power_supply/battery/temp` (fallback: `battery` thermal zone) |
| `SKIN` | `skin`, `shell_skin` | Weighted `VIRTUAL-SENSOR0` model (fallback: `quiet_therm`) |
| `BCL` | `socd`, `vbat` | `socd`, `pm8550_vbat` / `vbat` |

### Hot Throttling Thresholds (°C)

| Group | LIGHT | MODERATE | SEVERE | CRITICAL | EMERGENCY | SHUTDOWN |
|---|---:|---:|---:|---:|---:|---:|
| CPU | 70 | 80 | 90 | 98 | 105 | 115 |
| GPU | 60 | 70 | 80 | 90 | 100 | 110 |
| NPU | 65 | 75 | 85 | 95 | 105 | 115 |
| Battery | 43 | 45 | 48 | 50 | 52 | 55 |
| Skin | 46 | 50 | 54 | 58 | 62 | 66 |

### Exposed Cooling Devices

`cpufreq-cpu0`, `cpufreq-cpu3`, `cpufreq-cpu7`, `gpu`, `battery`.

---

## Policy Loading & `sconfig` Profiles

### Initial Profile Resolution

1. `MI_THERMALD_CONFIG` (if explicitly set).
2. Charger boot mode (`ro.bootmode=charger` or `MI_THERMALD_BOOTMODE`): `thermal-chg-only.conf`.
3. Region map (`thermal-region-map.conf`) matched against `ro.boot.hwversion`.
4. `/data/vendor/thermal/thermal-global-mode` mapped through `thermal-map.conf`.
5. Fallback: `thermal-normal.conf` (searched in `/data/vendor/thermal/config/`, `MI_THERMALD_CONFIG_DIR`, `/odm/etc/`, `/vendor/etc/`).

### `peridot` `sconfig` Profile Map

Writing a scene ID to `/sys/class/thermal/thermal_message/sconfig` switches both `hoshikv-thermal`'s active policy and notifies `qti_battery_charger` (ADSP charger thermal scene):

| `sconfig` | Profile | Use Case |
|---:|---|---|
| `0` | `thermal-normal.conf` | Balanced / daily default |
| `5` | `thermal-phone.conf` | Power Saving (strictest thermal/power limits) |
| `7` | `thermal-class0.conf` | Light usage / social |
| `10` | `thermal-navigation.conf` | Maps / GPS |
| `11` | `thermal-video.conf` | Video playback |
| `14` | `thermal-videochat.conf` | Video call |
| `15` | `thermal-camera.conf` | Camera |
| `16` / `17` | `thermal-4k.conf` | 4K recording |
| `18` | `thermal-tgame.conf` | Light/medium gaming |
| `19` | `thermal-mgame.conf` | Heavy gaming |
| `20` | `thermal-yuanshen.conf` | Sustained heavy gaming (Genshin) |
| `26` | `thermal-highfps.conf` | High FPS gaming |
| `50` | `thermal-per-normal.conf` | ColorOS High Performance / GT mode |
| `500` | `thermal-hp-normal.conf` | High Performance normal |
| `501` | `thermal-hp-mgame.conf` | High Performance heavy gaming |
| `6` | `thermal-nolimits.conf` | Benchmark / no thermal limits |

### Environment Overrides (Testing / Debugging)

| Variable | Purpose |
|---|---|
| `MI_THERMALD_CONFIG_DIR` | Override config search directory |
| `MI_THERMALD_CONFIG` | Force a specific INI config file |
| `MI_THERMALD_MAP` | Override `thermal-map.conf` path |
| `MI_THERMALD_REGION_MAP` | Override `thermal-region-map.conf` path |
| `MI_THERMALD_DEVICES` | Override `thermald-devices.conf` path |
| `MI_THERMALD_SYSFS_ROOT` | Prefix for sysfs/settings paths (useful for host tests) |
| `MI_THERMALD_HWVERSION` | Override `ro.boot.hwversion` |
| `MI_THERMALD_BOOTMODE` | Override `ro.bootmode` |
| `MI_THERMALD_DRY_RUN` | Log writes without touching sysfs (`1`/`true`) |
| `MI_THERMALD_VERBOSE` | Enable verbose worker and switch logging (`1`/`true`) |

---

## Project Structure

```text
.
├── aidl/android/hardware/thermal/  # AOSP Thermal AIDL definitions
├── src/
│   ├── main.rs                     # Process entry, binder thread, 1s HAL tick
│   ├── hal.rs                      # IThermal AIDL service, thresholds, callbacks
│   ├── idle_fps.rs                 # Touch-driven 120 Hz <-> 60 Hz idle dropper via SurfaceFlinger Binder
│   ├── sensors.rs                  # HAL sensor probing, lazy re-probe, VIRTUAL-SENSOR0
│   ├── sysfs.rs                    # Cached sysfs & cooling-device helpers
│   ├── mi_thermald.rs              # Policy loader, sconfig watcher, ColorOS ABX mode sync
│   ├── algos.rs                    # monitor / ss / sic / chrono policy workers
│   ├── config.rs                   # INI, device, thermal-map, and region-map parsers
│   ├── crypto.rs                   # AES-128-CBC decryptor for vendor thermal configs
│   ├── devices.rs                  # Multi-algo cooling-device arbitration & sysfs writes
│   └── thermal_sensors.rs          # Engine sensor store & virtual sensor evaluation
├── build.rs                        # Generates Rust AIDL bindings via rsbinder-aidl
├── Cargo.toml
└── Cargo.lock
```

---

## Build & Test

`.cargo/config.toml` defaults to `aarch64-linux-android` using `/opt/android-ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android28-clang`.

```bash
# Host unit tests (19 tests)
cargo test --target x86_64-unknown-linux-gnu

# Android release binary -> target/aarch64-linux-android/release/hoshikv-thermal
cargo build --release --target aarch64-linux-android

# Optional verbose/debug logging build
cargo build --release --features debug-log --target aarch64-linux-android
```

---

## Deployment

Install the binary to `/vendor/bin/hoshikv-thermal` (`0755`, `u:object_r:mi_thermald_exec:s0`) and start it from `/vendor/etc/init/hw/init.mi_thermald.rc`:

```rc
on charger
    wait /sys/class/power_supply/usb/type
    start mi_thermald

on boot
    wait /sys/class/power_supply/usb/type
    start mi_thermald

service mi_thermald /system/vendor/bin/hoshikv-thermal
    class hal
    user root
    group system
    interface aidl android.hardware.thermal.IThermal/default
    seclabel u:r:mi_thermald:s0
```

Required SELinux rules in `odm_sepolicy.cil` for Enforcing mode (ColorOS settings ABX read + kmsg logging):

```cil
(allow mi_thermald system_data_file_34_0 (dir (search getattr)))
(allow mi_thermald system_data_file_34_0 (file (read open getattr)))
(allow mi_thermald kmsg_device_34_0 (chr_file (write append open)))
```

### Verification on Device

```bash
adb shell dmesg | grep hoshikv-thermal
adb shell cat /sys/class/thermal/thermal_message/sconfig
adb shell dumpsys thermalservice
```

---

## Licensing

The AIDL interface files under `aidl/android/hardware/thermal/` are derived from AOSP and retain their original Apache-2.0 headers.
