// The VR_* entry points sit under `#if 0` in openvr_capi.h, so src/openvr.rs declares them itself.
fn main() {
    let header = "vendor/openvr/openvr_capi.h";
    println!("cargo:rerun-if-changed={header}");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("openvr.rs");
    bindgen::Builder::default()
        .header(header)
        .allowlist_type("VR_IVR(System|Input|Applications|Compositor)_FnTable")
        .allowlist_type("TrackedDevicePose_t|InputSkeletalActionData_t|VRSkeletalSummaryData_t|VRBoneTransform_t")
        .allowlist_type("VRActiveActionSet_t|Compositor_CumulativeStats|Compositor_FrameTiming")
        .allowlist_type("ETrackedDeviceClass|ETrackedControllerRole|ETrackingUniverseOrigin|ETrackedDeviceProperty|EVRSkeletalTransformSpace")
        .allowlist_type("EVRSkeletalMotionRange|EVRSummaryType|EVRSceneApplicationState|EVRApplicationType|EVRInitError")
        .allowlist_var("IVR(System|Input|Applications|Compositor)_Version|k_unMaxTrackedDeviceCount")
        .prepend_enum_name(false)
        .generate()
        .expect("bindgen on openvr_capi.h")
        .write_to_file(out)
        .unwrap();
}
