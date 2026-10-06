use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=aidl");

    let base = PathBuf::from("aidl/android/hardware/thermal");
    rsbinder_aidl::Builder::new()
        .source(base.join("CoolingDevice.aidl"))
        .source(base.join("CoolingType.aidl"))
        .source(base.join("IThermal.aidl"))
        .source(base.join("IThermalChangedCallback.aidl"))
        .source(base.join("Temperature.aidl"))
        .source(base.join("TemperatureThreshold.aidl"))
        .source(base.join("TemperatureType.aidl"))
        .source(base.join("ThrottlingSeverity.aidl"))
        .include_dir(PathBuf::from("aidl"))
        .version(2)
        .set_async_support(false)
        .output(PathBuf::from("thermal_aidl.rs"))
        .generate()
        .expect("AIDL codegen failed");
}
