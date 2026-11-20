# kave-thermalized

`hoshikv-thermal` is the `android.hardware.thermal.IThermal/default` AIDL HAL
for the Redmi Turbo 3 (peridot, SM8650). It embeds the vendor `mi_thermald`
policy engine in the HAL process. The old Kave stepped engine, guard, load
tracker, tuner, and JSON policy file have been removed.

This is not a kernel thermal driver. The kernel exposes thermal zones and
cooling devices; `hoshikv-thermal` publishes them through AIDL and the embedded
engine applies the vendor INI policy.

## Runtime model

- One process owns both the binder service and the policy engine.
- The binder thread registers `android.hardware.thermal.IThermal/default`.
- The HAL publishes temperatures, thresholds, cooling-device states, and
  throttling callbacks every second.
- HAL sensor reads and cooling-device reads are read-only.
- All policy writes come from the embedded `mi_thermald` engine.
- The engine watches `/sys/class/thermal/thermal_message/sconfig`; a change
  rebuilds the worker set while preserving sensor history and device targets.
- The engine runs in a thread named `mi-thermald`. There is no separate
  `mi_thermald` daemon spawned by this binary.

`mi-thermald` logs to stdout and `/dev/kmsg` with the `hoshikv-thermal:` prefix.

## Configuration

The engine reads the vendor INI tree. The initial profile is selected in this
order:

1. `MI_THERMALD_CONFIG`, when set.
2. Charger boot mode: `/odm/etc/thermal-chg-only.conf`, or the indexed
   `/vendor/etc/thermal-chg-only[-N].conf` when a vendor normal profile exists.
3. The region map (`MI_THERMALD_REGION_MAP`, `/odm/etc/thermal-region-map.conf`,
   or `/vendor/etc/thermal-region-map.conf`) matched against
   `MI_THERMALD_HWVERSION` or `ro.boot.hwversion`.
4. The global mode in `/data/vendor/thermal/thermal-global-mode`, mapped
   through `thermal-map.conf`.
5. `thermal-normal.conf` in the selected config directory.

A profile name is resolved in this order:

1. `/data/vendor/thermal/config/<name>`
2. the selected config directory
3. `/odm/etc/<name>`
4. `/vendor/etc/<name>`

`thermal-map.conf` is searched at the selected config directory,
`/odm/etc/thermal-map.conf`, and `/vendor/etc/thermal-map.conf`. On peridot,
the active policy is `/odm/etc/thermal-normal.conf`; the ODM map file is absent
and the vendor map is used. `/vendor/etc/thermal-normal.conf` is also absent.
`/data/vendor/thermal/config/` is optional and was empty on the reference
device.

Encrypted profiles use AES-128-CBC with the vendor key and IV, no padding, and
NUL termination. The loader also accepts plaintext INI files. The encrypted
tree is intentionally not decoded into the repository.

The engine can be redirected for testing:

| Variable | Meaning |
|---|---|
| `MI_THERMALD_CONFIG_DIR` | Config directory override |
| `MI_THERMALD_CONFIG` | Single config file override |
| `MI_THERMALD_MAP` | `thermal-map.conf` override |
| `MI_THERMALD_REGION_MAP` | `thermal-region-map.conf` override |
| `MI_THERMALD_DEVICES` | `thermald-devices.conf` override |
| `MI_THERMALD_SYSFS_ROOT` | Sysfs root prefix |
| `MI_THERMALD_HWVERSION` | Region-map hardware version override |
| `MI_THERMALD_BOOTMODE` | Boot-mode override |
| `MI_THERMALD_DRY_RUN` | Log intended writes without applying them |
| `MI_THERMALD_VERBOSE` | Verbose config and worker logging |

Typical policy sections are `[MONITOR-*]`, `[SS-CPU*]`, `[SIC-BAT]`, and
virtual/simulated sensor sections. `mi_thermald` supports the `monitor`, `ss`,
`sic`, and `chrono` worker types. A `device` value may contain several device
names separated by `+`; the device store applies the highest or lowest request
according to the vendor `select_higher` setting.

A reference dry run on the target loaded 77 sensors, 26 devices, and 20
algorithms. The active set included `MONITOR-SENSOR0`, `SS-CPU0`, `SS-CPU3`,
`SS-CPU7`, `MONITOR-BAT`, `SIC-BAT`, `MONITOR-TEMP_STATE`, `MONITOR-GPU`,
`SS-LMH_CPU7`, and `MONITOR-NPU`.

## HAL interface

The HAL publishes these temperature names:

| Type | Names |
|---|---|
| CPU | `CPU0`–`CPU7` |
| GPU | `GPU0`–`GPU3` |
| NPU | `nsp0`–`nsp3` |
| Battery | `battery` |
| Skin | `skin`, `shell_skin` |
| BCL | `socd`, `vbat` |

CPU zones are `cpu-0-0-0`, `cpu-0-1-0`, `cpu-0-2-0`, `cpu-1-0-0`,
`cpu-1-1-0`, `cpu-1-2-0`, `cpu-1-3-0`, and `cpu-2-0-0`; GPU zones are
`gpuss-0` through `gpuss-3`; NPU zones are `nsphvx-0`, `nsphvx-1`, `nsphmx-0`,
and `nsphmx-1`.
Missing zones fall back to 25 °C; the battery value prefers
`/sys/class/power_supply/battery/temp`.

Hot thresholds in °C:

| Group | Thresholds |
|---|---|
| CPU | 70, 80, 90, 98, 105, 115 |
| GPU | 60, 70, 80, 90, 100, 110 |
| NPU | 65, 75, 85, 95, 105, 115 |
| Battery | 43, 45, 48, 50, 52, 55 |
| Skin | 46, 50, 54, 58, 62, 66 |

`skin` and `shell_skin` are weighted virtual sensors. The default weights are
CPU 200, battery 200, charger 100, Wi-Fi 60, PA0 40, PA1 80, quiet 150,
connection 100, and hotspot 60. The virtual value uses the available zones and
falls back to `quiet_therm`.

Cooling devices exposed by the HAL are `cpufreq-cpu0`, `cpufreq-cpu3`,
`cpufreq-cpu7`, `gpu`, and `battery`.

Callbacks are registered with `registerThermalChangedCallback`; type filtering
uses the AIDL `TemperatureType`. The HAL does not write cooling state.

## oplus thermal stack

These components are present on peridot and are relevant when diagnosing
thermal behavior. They are separate from the embedded `mi_thermald` policy.

| Component | Role |
|---|---|
| `ostatsd` | OStats telemetry; not a thermal controller |
| HANS | Scene and freezer management; not an INI policy engine |
| horae | Thermal status and process-binding service |
| `OplusThermalService` | Framework consumer of `sys_thermal_config.xml` |
| oplus Battery | Parser for `sys_thermal_control_config*.xml` app/gear data |

### `ostatsd`

Service name is `ostatsd`, with interface `com.oplus.ostats.api.IOStats`; the
binary is `/system_ext/bin/ostatsd`. Related processes are `ostats_pullerd` and
`ostats_tpd`. Its init script creates `/data/misc/ostats`, `packet_files`,
`dcs_config`, `database`, and `database/thermal_analysis`. It collects thermal
statistics, app memory metrics, DCS data, and `MidasConfig`/`OStatsConfig`; it
does not select a thermal profile or write cooling devices.

### HANS

Service `/system_ext/bin/hans` is disabled initially and starts when
`sys.hans.enable=true`. It sets up `/dev/freezer` and exposes
`oplus.hans.IHansComunication`. Useful properties are
`persist.vendor.enable.hans` and `persist.sys.hans.appcard.enable`.

`HansSceneManager` provides `getSceneIdByScene()` and
`notifyAppGoForeground(int, String)`. Its app-target constants include
`APP_TARGET_INVALID=0`, `APP_TARGET_THIRD=1`, `APP_TARGET_WHITELIST=2`,
`APP_TARGET_EX_WHITELIST=4`, `APP_TARGET_MAX=8`, and
`AIRPLANE_SCENE_TARGET=7`. Scene IDs include `SCENE_ID_AIRPLANE`,
`SCENE_ID_CAMERA_MIN_SYSTEM`, `SCENE_ID_CPU_HIGH_LOAD`, `SCENE_ID_DEEP_IDLE`,
`SCENE_ID_DEEP_SLEEP`, `SCENE_ID_GAME_MIN_SYSTEM`, `SCENE_ID_IDLE`,
`SCENE_ID_LIVE_STREAM_MIN_SYSTEM`, `SCENE_ID_NIGHT_DEEP_DOZE`, and
`SCENE_ID_STRICT_MODE`.

HANS scene notifications do not currently change a `mi_thermald` profile.

### horae

Service `horae` exposes `com.oplus.horae.IHoraeService` and registers
`com.oplus.thermalcontrol.IThermalStatusListener`. Observed transactions cover
register (1), unregister (2), status (4), current temperatures (17), specified
temperatures (20), all shell temperatures (21), ambient temperature (25), and
process binding information (101). It reports thermal state and bindings; this
repository does not parse a horae policy file.

### `sys_thermal_config.xml`

This file is a monitoring list, not a package-to-`sconfig` map. The reference
copy sets `more_heat=490`, `heat=440`, `less_heat=410`, `preheat=400`,
`thermal_heat_path=/sys/class/thermal/thermal_zone84/temp`,
`thermal_battery_temp=1`, and `thermal_monitor_app_limit_time=1200000`.
`thermal_monitor_application` includes entries such as
`com.tencent.tmgp.pubgmhd`, `com.tencent.tmgp.sgame`,
`com.ss.android.ugc.aweme`, `com.smile.gifmaker`, and
`com.tencent.tmgp.pubgm`.

`OplusThermalService` looks it up through
`OplusCfgFilePolicy.getCfgTopPriorityFile("sys_thermal_config.xml",
"etc/ThermalServiceConfig", 3)` and reads the `sys_thermal_config` provider.
No `ThermalServiceConfig` directory was found on the reference device, so the
exact active file path is not confirmed.

### `sys_thermal_control_config*.xml`

These are oplus Battery app/gear configurations. `ThermalConfigUtil`
(`lb.h`) parses them in `loadApListFromXml`. The XML contains:

- `<specific>` groups whose tag names are hyphen-joined package names, such as
  `com.android.settings-com.android.mms-com.android.contacts`.
- `<specificScene>` groups whose tag names add `_scene_N` suffixes, for example
  `com.tencent.mm_scene_103-com.tencent.mobileqq_scene_103-...`.
- `<scene>` entries such as `scene_16`, `scene_101` through `scene_105`,
  `scene_150`, and `scene_151`.

Each group contains `tempGear` entries with `<gear_config>` attributes for CPU,
GPU, brightness, charging, FPS, refresh rate, restrictions, camera, flashlight,
Wi-Fi hotspot, video, and boost behavior. The parser here is oplus Battery, not
`mi_thermald`.

The device carries variants under
`/my_product/cust/{EUEX,ROW,US}/etc/temperature_profile/`. The IN directory was
empty in the reference check. `ThermalConfigUtil` resolves the profile through
`/etc/temperature_profile` and `OplusCfgFilePolicy`; the active regional choice
is not verified by this project.

## Applist and `sconfig`

`mi_thermald` currently has no applist reader. It selects profiles from
`sconfig`, which is owned by `system_server` and exposed at
`/sys/class/thermal/thermal_message/sconfig`. The engine polls that file every
500 ms and resolves the numeric value through `thermal-map.conf`.

The oplus app/gear XML files described above do not feed the embedded engine.
Editing them will not change the current `mi_thermald` profile. A future applist
implementation would need a separate file and a package-to-`sconfig` contract;
the schema below is only a proposal:

```yaml
applist:
  enabled: true
  default: -1
  poll_ms: 1000
  apps:
    com.tencent.tmgp.sgame: 18
    com.tencent.tmgp.pubgmhd: 18
```

No applist file exists in this repository or in the reference `mi_thermald`
tree. Do not treat the proposed schema as implemented behavior.

## Project structure

```
.
├── aidl/android/hardware/thermal/  # generated AIDL definitions
├── src/
│   ├── main.rs                    # binder, HAL tick, engine lifecycle
│   ├── hal.rs                     # IThermal service and callbacks
│   ├── sensors.rs                 # HAL sensor probing and virtual skin
│   ├── sysfs.rs                   # read-only sysfs/cooling helpers
│   ├── mi_thermald.rs             # config selection and policy lifecycle
│   ├── algos.rs                   # monitor/ss/sic/chrono workers
│   ├── config.rs                  # INI, device, map, region parsers
│   ├── crypto.rs                  # AES-128-CBC decryption
│   ├── devices.rs                 # cooling-device arbitration
│   └── thermal_sensors.rs         # engine sensor store
├── prebuilt/                      # earlier release binaries
├── build.rs
├── Cargo.toml
└── Cargo.lock
```

`thermal_sensors.rs` is the engine's sensor store. `sensors.rs` is the HAL's
read-only sensor feed.

## Build

The checked-in `.cargo/config.toml` defaults to Android, so host tests need an
explicit target:

```bash
cargo fmt --check
cargo clippy --target x86_64-unknown-linux-gnu --all-targets -- -D warnings
cargo test --target x86_64-unknown-linux-gnu
cargo build --release --target aarch64-linux-android
```

The Android linker path is:

```
/opt/android-ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android28-clang
```

Adjust `.cargo/config.toml` if the NDK lives elsewhere. `build.rs` generates
`thermal_aidl.rs` with `rsbinder-aidl`.

The release binary is:

```
target/aarch64-linux-android/release/hoshikv-thermal
```

`prebuilt/` contains binaries from earlier releases. The source build is
authoritative.

## Deployment

The binary must run with root or vendor privileges so it can register the AIDL
service and write thermal sysfs. The vendor or ODM thermal configuration tree
must be present.

The reference peridot deployment uses `/vendor/bin/hoshikv-thermal` and an init
service that starts it as the thermal HAL. The test module modifies
`/vendor/etc/init/hw/init.mi_thermald.rc` to start
`/system/vendor/bin/hoshikv-thermal`; the stock `/vendor/bin/mi_thermald` stays
untouched. The repository does not ship that init script or the module.

If the HAL starts but the engine cannot find a config, the HAL still exposes
sensor data, but no vendor policy is applied. Check the `hoshikv-thermal:`
kernel log line and the selected config path before changing thermal XML files.

Useful checks on device:

```bash
adb shell dmesg | grep hoshikv-thermal
adb shell service list | grep thermal
adb shell dumpsys thermalservice
```

Do not overwrite a stock vendor binary or deploy to `/vendor` without a
recovery path.

## Known limits

- HANS transaction semantics and scene-to-thermal effects were not probed.
- The exact active `sys_thermal_config.xml` path is not confirmed; the local
  copy came from the device firmware.
- Regional selection of `sys_thermal_control_config.xml` is not verified.
- The applist/schema above is a proposal, not an implementation.
- No camera-specific per-app trigger is implemented.

## AIDL licensing

The AIDL interface definitions are derived from AOSP and retain their original
Apache-2.0 copyright headers.
