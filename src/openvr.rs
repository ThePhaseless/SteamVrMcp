//! SteamVR's own view of tracked devices and hand skeletons, read through an OpenVR client in this process.
#![allow(non_upper_case_globals)] // matches on OpenVR's enum constant names, as bindgen keeps them

use alvr_common::{
    anyhow::{Context, Result, bail},
    glam::{Mat3, Quat, Vec3},
    warn,
};
use libloading::Library;
use serde::Serialize;
use std::{
    cell::Cell,
    collections::BTreeMap,
    ffi::{CStr, CString, c_char},
    fs,
    mem::{size_of, zeroed},
    os::unix::ffi::OsStrExt,
    path::Path,
    ptr,
};

#[allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code)]
mod sys {
    include!(concat!(env!("OUT_DIR"), "/openvr.rs"));
}
use sys::*;

/// Written to `state_dir/openvr/` and loaded with SetActionManifestPath.
/// Not runtime_dir: vrserver runs in the Steam Linux Runtime container, which shares the home
/// directory but not /run/user/<uid>/vr-test-mcp (probe: ActionManifestError_FileNotFound).
const ASSETS: [(&str, &str); 3] = [
    ("actions.json", include_str!("../assets/openvr/actions.json")),
    ("bindings_knuckles.json", include_str!("../assets/openvr/bindings_knuckles.json")),
    (
        "bindings_svl_hand_interaction_augmented.json",
        include_str!("../assets/openvr/bindings_svl_hand_interaction_augmented.json"),
    ),
];
const HANDS: [&str; 2] = ["left", "right"];
/// OpenVR's hand skeleton (openvr.h HandSkeletonBone).
const BONES: usize = 31;

#[derive(Debug, Serialize)]
pub struct TrackedState {
    pub devices: Vec<Device>,
    /// "left" | "right"
    pub skeletons: BTreeMap<String, SkeletonState>,
}

#[derive(Debug, Serialize)]
pub struct Device {
    pub index: u32,
    /// "hmd" | "controller" | "tracker" | "reference" | "other"
    pub class: String,
    /// "left" | "right" | ""
    pub role: String,
    pub serial: String,
    pub model: String,
    pub connected: bool,
    pub pose_valid: bool,
    pub position: [f32; 3],
    /// x, y, z, w
    pub quat: [f32; 4],
    pub linear_velocity: [f32; 3],
    pub angular_velocity: [f32; 3],
    /// Only for the hmd: the last worn (true) or removed (false) event SteamVR sent, None before any.
    pub head_proximity: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct SkeletonState {
    pub active: bool,
    /// thumb, index, middle, ring, pinky; as SteamVR animates them
    pub curls: [f32; 5],
    /// between thumb-index, index-middle, middle-ring, ring-pinky
    pub splay: [f32; 4],
    /// as the driver reports them
    pub device_curls: [f32; 5],
    pub device_splay: [f32; 4],
    /// 31 OpenVR bones in model space without controller: position x, y, z, then quat x, y, z, w
    pub bones: Option<Vec<[f32; 7]>>,
}

#[derive(Debug, Serialize)]
pub struct SceneApp {
    pub pid: u32,
    /// "none" | "starting" | "quitting" | "running" | "waiting"
    pub state: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct FrameStats {
    pub presents: u32,
    pub dropped: u32,
    pub reprojected: u32,
}

/// One compositor frame from `GetFrameTimings`.
#[derive(Clone, Copy, Default)]
pub struct FrameTiming {
    pub system_s: f64,
    pub interval_ms: f32,
    pub gpu_ms: f32,
    pub dropped: u32,
    pub mispresented: u32,
    pub reprojected: bool,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Spread {
    pub p50: f32,
    pub p95: f32,
    pub p99: f32,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct CompositorSummary {
    pub frames: usize,
    pub fps: f32,
    pub interval_ms: Spread,
    pub gpu_ms: Spread,
    pub dropped: u32,
    pub mispresented: u32,
    pub reprojected: usize,
}

/// How many frames of SteamVR's history to ask for.
pub const TIMINGS_MAX: u32 = 1024;

/// Nearest-rank percentiles; sorts `values`, which must not be empty.
pub fn spread(values: &mut [f32]) -> Spread {
    values.sort_by(f32::total_cmp);
    let rank = |p: usize| values[(p * values.len()).div_ceil(100) - 1];
    Spread { p50: rank(50), p95: rank(95), p99: rank(99) }
}

/// The frames within `window_s` of the newest, or None when there are none.
pub fn summarize(timings: &[FrameTiming], window_s: f64) -> Option<CompositorSummary> {
    let newest = timings.iter().map(|t| t.system_s).max_by(f64::total_cmp)?;
    let kept: Vec<&FrameTiming> = timings.iter().filter(|t| t.system_s >= newest - window_s).collect();
    let mean = kept.iter().map(|t| t.interval_ms).sum::<f32>() / kept.len() as f32;
    Some(CompositorSummary {
        frames: kept.len(),
        fps: 1000.0 / mean,
        interval_ms: spread(&mut kept.iter().map(|t| t.interval_ms).collect::<Vec<_>>()),
        gpu_ms: spread(&mut kept.iter().map(|t| t.gpu_ms).collect::<Vec<_>>()),
        dropped: kept.iter().map(|t| t.dropped).sum(),
        mispresented: kept.iter().map(|t| t.mispresented).sum(),
        reprojected: kept.iter().filter(|t| t.reprojected).count(),
    })
}

/// Position and x, y, z, w rotation of an OpenVR row-major 3x4 pose.
fn pose(m: &[[f32; 4]; 3]) -> ([f32; 3], [f32; 4]) {
    let col = |c: usize| Vec3::new(m[0][c], m[1][c], m[2][c]);
    let rotation = Quat::from_mat3(&Mat3::from_cols(col(0), col(1), col(2)));
    (col(3).to_array(), rotation.to_array())
}

/// Calls `f` in an OpenVR function table.
macro_rules! call {
    ($table:expr, $f:ident $(, $arg:expr)*) => {
        unsafe { ((*$table).$f.expect(stringify!($f)))($($arg),*) }
    };
}

fn input_ok(e: EVRInputError, what: &str) -> Result<()> {
    if e != 0 {
        bail!("{what}: EVRInputError {e}");
    }
    Ok(())
}

/// A background OpenVR application: it never starts SteamVR and never shows as the scene app.
pub struct OpenVr {
    lib: Library,
    system: *const VR_IVRSystem_FnTable,
    input: *const VR_IVRInput_FnTable,
    apps: *const VR_IVRApplications_FnTable,
    compositor: *const VR_IVRCompositor_FnTable,
    set: VRActionSetHandle_t,
    skeletons: [VRActionHandle_t; 2],
    // shortcut: whether SteamVR delivers these events to a Background app is unverified live, check once and fall back to None
    head_proximity: Cell<Option<bool>>,
}

// SAFETY: the function tables are process-global in vrclient and take no thread-local state
unsafe impl Send for OpenVr {}

type InitFn = unsafe extern "C" fn(*mut EVRInitError, EVRApplicationType) -> isize;
type GetInterfaceFn = unsafe extern "C" fn(*const c_char, *mut EVRInitError) -> isize;
type ErrorTextFn = unsafe extern "C" fn(EVRInitError) -> *const c_char;
type ShutdownFn = unsafe extern "C" fn();

impl OpenVr {
    /// Connects to a running vrserver with the client library of the SteamVR install, and loads the
    /// skeleton action manifest from `state_dir/openvr/`.
    // shortcut: assumes vrserver's container sees state_dir (true under $HOME), copy into $HOME when XDG_STATE_HOME is elsewhere
    pub fn connect(steamvr_dir: &Path, state_dir: &Path) -> Result<OpenVr> {
        let path = steamvr_dir.join("bin/linux64/libopenvr_api.so");
        let lib = unsafe { Library::new(&path) }.with_context(|| path.display().to_string())?;
        let mut err = 0;
        unsafe {
            let init = *lib.get::<InitFn>(b"VR_InitInternal")?;
            let text = *lib.get::<ErrorTextFn>(b"VR_GetVRInitErrorAsEnglishDescription")?;
            init(&mut err, EVRApplicationType_VRApplication_Background);
            if err != EVRInitError_VRInitError_None {
                bail!("VR_InitInternal: {} ({err})", CStr::from_ptr(text(err)).to_string_lossy());
            }
        }
        let mut vr = OpenVr {
            lib,
            system: ptr::null(),
            input: ptr::null(),
            apps: ptr::null(),
            compositor: ptr::null(),
            set: 0,
            skeletons: [0; 2],
            head_proximity: Cell::new(None),
        };
        vr.attach(state_dir)?; // on error, dropping vr shuts the client down
        Ok(vr)
    }

    fn attach(&mut self, state_dir: &Path) -> Result<()> {
        let get = unsafe { *self.lib.get::<GetInterfaceFn>(b"VR_GetGenericInterface")? };
        let table = |version: &[u8]| -> Result<isize> {
            let name = CString::new([b"FnTable:", version.strip_suffix(b"\0").unwrap_or(version)].concat())?;
            let mut err = 0;
            let table = unsafe { get(name.as_ptr(), &mut err) };
            if err != EVRInitError_VRInitError_None || table == 0 {
                bail!("VR_GetGenericInterface({}): EVRInitError {err}", name.to_string_lossy());
            }
            Ok(table)
        };
        self.system = table(IVRSystem_Version)? as _;
        self.input = table(IVRInput_Version)? as _;
        self.apps = table(IVRApplications_Version)? as _;
        self.compositor = table(IVRCompositor_Version)? as _;

        let dir = state_dir.join("openvr");
        fs::create_dir_all(&dir).with_context(|| dir.display().to_string())?;
        for (name, text) in ASSETS {
            fs::write(dir.join(name), text).with_context(|| dir.join(name).display().to_string())?;
        }
        let manifest = CString::new(dir.join(ASSETS[0].0).as_os_str().as_bytes())?;
        input_ok(call!(self.input, SetActionManifestPath, manifest.as_ptr() as *mut _), "SetActionManifestPath")?;
        let set = c"/actions/main";
        input_ok(call!(self.input, GetActionSetHandle, set.as_ptr() as *mut _, &mut self.set), "GetActionSetHandle")?;
        for (hand, handle) in HANDS.iter().zip(&mut self.skeletons) {
            let name = CString::new(format!("/actions/main/in/skeleton_{hand}"))?;
            input_ok(call!(self.input, GetActionHandle, name.as_ptr() as *mut _, handle), "GetActionHandle")?;
        }
        Ok(())
    }

    pub fn tracked_state(&self, include_bones: bool) -> Result<TrackedState> {
        let mut set = VRActiveActionSet_t {
            ulActionSet: self.set,
            ulRestrictedToDevice: 0,
            ulSecondaryActionSet: 0,
            unPadding: 0,
            nPriority: 0,
        };
        input_ok(call!(self.input, UpdateActionState, &mut set, size_of::<VRActiveActionSet_t>() as u32, 1), "UpdateActionState")?;
        let mut poses = [unsafe { zeroed::<TrackedDevicePose_t>() }; k_unMaxTrackedDeviceCount as usize];
        let standing = ETrackingUniverseOrigin_TrackingUniverseStanding;
        call!(self.system, GetDeviceToAbsoluteTrackingPose, standing, 0.0, poses.as_mut_ptr(), poses.len() as u32);
        let devices = (0..).zip(&poses).filter_map(|(index, p)| self.device(index, p)).collect();

        let mut skeletons = BTreeMap::new();
        for (hand, &action) in HANDS.iter().zip(&self.skeletons) {
            let mut data = unsafe { zeroed::<InputSkeletalActionData_t>() };
            let size = size_of::<InputSkeletalActionData_t>() as u32;
            // a skeleton SteamVR withholds is reported inactive, so poses still come through
            if let Err(e) = input_ok(call!(self.input, GetSkeletalActionData, action, &mut data, size), hand) {
                warn!("skeleton {e:#}");
            }
            let summary = |kind| {
                let mut s = unsafe { zeroed::<VRSkeletalSummaryData_t>() };
                // an inactive action answers an error and leaves zeros, which is what we report
                call!(self.input, GetSkeletalSummaryData, action, kind, &mut s);
                s
            };
            let (animated, device) = (summary(EVRSummaryType_VRSummaryType_FromAnimation), summary(EVRSummaryType_VRSummaryType_FromDevice));
            let bones = include_bones.then(|| {
                let mut bones = [unsafe { zeroed::<VRBoneTransform_t>() }; BONES];
                let (space, range) = (
                    EVRSkeletalTransformSpace_VRSkeletalTransformSpace_Model,
                    EVRSkeletalMotionRange_VRSkeletalMotionRange_WithoutController,
                );
                call!(self.input, GetSkeletalBoneData, action, space, range, bones.as_mut_ptr(), BONES as u32);
                bones
                    .iter()
                    .map(|b| {
                        let (p, q) = (b.position.v, b.orientation);
                        [p[0], p[1], p[2], q.x, q.y, q.z, q.w]
                    })
                    .collect()
            });
            skeletons.insert(
                hand.to_string(),
                SkeletonState {
                    active: data.bActive,
                    curls: animated.flFingerCurl,
                    splay: animated.flFingerSplay,
                    device_curls: device.flFingerCurl,
                    device_splay: device.flFingerSplay,
                    bones,
                },
            );
        }
        Ok(TrackedState { devices, skeletons })
    }

    /// The device at `index`, or None for an empty slot.
    fn device(&self, index: u32, p: &TrackedDevicePose_t) -> Option<Device> {
        let class = match call!(self.system, GetTrackedDeviceClass, index) {
            ETrackedDeviceClass_TrackedDeviceClass_Invalid => return None,
            ETrackedDeviceClass_TrackedDeviceClass_HMD => "hmd",
            ETrackedDeviceClass_TrackedDeviceClass_Controller => "controller",
            ETrackedDeviceClass_TrackedDeviceClass_GenericTracker => "tracker",
            ETrackedDeviceClass_TrackedDeviceClass_TrackingReference => "reference",
            _ => "other",
        };
        let role = match call!(self.system, GetControllerRoleForTrackedDeviceIndex, index) {
            ETrackedControllerRole_TrackedControllerRole_LeftHand => "left",
            ETrackedControllerRole_TrackedControllerRole_RightHand => "right",
            _ => "",
        };
        let text = |prop| {
            let mut buf = [0 as c_char; 256];
            let mut err = 0;
            call!(self.system, GetStringTrackedDeviceProperty, index, prop, buf.as_mut_ptr(), buf.len() as u32, &mut err);
            unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
        };
        let (position, quat) = pose(&p.mDeviceToAbsoluteTracking.m);
        Some(Device {
            index,
            class: class.into(),
            role: role.into(),
            serial: text(ETrackedDeviceProperty_Prop_SerialNumber_String),
            model: text(ETrackedDeviceProperty_Prop_ModelNumber_String),
            connected: p.bDeviceIsConnected,
            pose_valid: p.bPoseIsValid,
            position,
            quat,
            linear_velocity: p.vVelocity.v,
            angular_velocity: p.vAngularVelocity.v,
            head_proximity: if class == "hmd" { self.head_proximity.get() } else { None },
        })
    }

    /// Drains the event queue. On VREvent_Quit it acknowledges, so SteamVR does not kill this
    /// process after its quit timeout (openvr.h AcknowledgeQuit_Exiting), and returns true: the
    /// caller then drops the reader.
    pub fn poll_quit(&self) -> bool {
        let mut event = unsafe { zeroed::<VREvent_t>() };
        let mut quit = false;
        while call!(self.system, PollNextEvent, &mut event, size_of::<VREvent_t>() as u32) {
            quit |= event.eventType == EVREventType_VREvent_Quit;
            self.head_proximity.set(proximity(self.head_proximity.get(), event.eventType, event.trackedDeviceIndex));
        }
        if quit {
            call!(self.system, AcknowledgeQuit_Exiting);
        }
        quit
    }

    pub fn scene_app(&self) -> SceneApp {
        let state = match call!(self.apps, GetSceneApplicationState) {
            EVRSceneApplicationState_Starting => "starting",
            EVRSceneApplicationState_Quitting => "quitting",
            EVRSceneApplicationState_Running => "running",
            EVRSceneApplicationState_Waiting => "waiting",
            _ => "none",
        };
        SceneApp { pid: call!(self.apps, GetCurrentSceneProcessId), state: state.into() }
    }

    /// Frame counters of the current scene app since it started.
    pub fn frame_stats(&self) -> Result<FrameStats> {
        let mut s = unsafe { zeroed::<Compositor_CumulativeStats>() };
        call!(self.compositor, GetCumulativeStats, &mut s, size_of::<Compositor_CumulativeStats>() as u32);
        if s.m_nPid == 0 {
            bail!("no frame stats: no scene app, or the compositor withholds them from a background app");
        }
        Ok(FrameStats { presents: s.m_nNumFramePresents, dropped: s.m_nNumDroppedFrames, reprojected: s.m_nNumReprojectedFrames })
    }

    /// The compositor's recent frames, oldest first.
    pub fn frame_timings(&self) -> Result<Vec<FrameTiming>> {
        let mut frames = vec![unsafe { zeroed::<Compositor_FrameTiming>() }; TIMINGS_MAX as usize];
        frames[0].m_nSize = size_of::<Compositor_FrameTiming>() as u32;
        let got = call!(self.compositor, GetFrameTimings, frames.as_mut_ptr(), TIMINGS_MAX) as usize;
        if got == 0 {
            bail!("no frame timings: no scene app, or the compositor withholds them from a background app");
        }
        Ok(frames[..got.min(frames.len())]
            .iter()
            .map(|f| FrameTiming {
                system_s: f.m_flSystemTimeInSeconds,
                interval_ms: f.m_flClientFrameIntervalMs,
                gpu_ms: f.m_flTotalRenderGpuMs,
                dropped: f.m_nNumDroppedFrames,
                mispresented: f.m_nNumMisPresented,
                reprojected: f.m_nReprojectionFlags != 0,
            })
            .collect())
    }
}

/// The hmd's worn state after an event; SteamVR derives these from the driver's /proximity input.
fn proximity(current: Option<bool>, event_type: u32, device: u32) -> Option<bool> {
    match event_type {
        EVREventType_VREvent_TrackedDeviceUserInteractionStarted if device == 0 => Some(true),
        EVREventType_VREvent_TrackedDeviceUserInteractionEnded if device == 0 => Some(false),
        _ => current,
    }
}

impl Drop for OpenVr {
    fn drop(&mut self) {
        if let Ok(shutdown) = unsafe { self.lib.get::<ShutdownFn>(b"VR_ShutdownInternal") } {
            unsafe { shutdown() };
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use alvr_common::glam::Quat;
    use std::f32::consts::FRAC_PI_2;

    #[test]
    fn spread_uses_nearest_rank() {
        let mut v: Vec<f32> = (1..=100).map(|i| i as f32).collect();
        let s = spread(&mut v);
        assert_eq!((s.p50, s.p95, s.p99), (50.0, 95.0, 99.0));
        let s = spread(&mut [7.0]);
        assert_eq!((s.p50, s.p95, s.p99), (7.0, 7.0, 7.0));
    }

    #[test]
    fn summarize_keeps_window_and_counts() {
        let timings: Vec<FrameTiming> = (0..900)
            .map(|i| FrameTiming {
                system_s: i as f64 / 90.0,
                interval_ms: 11.1,
                gpu_ms: 5.0,
                dropped: u32::from(i % 10 == 0),
                reprojected: i % 10 == 0,
                ..Default::default()
            })
            .collect();
        let s = summarize(&timings, 1.05).unwrap();
        assert_eq!((s.frames, s.dropped, s.reprojected), (95, 9, 9));
        assert!((s.fps - 90.09).abs() < 0.01, "{}", s.fps);
        assert_eq!((s.gpu_ms.p50, s.gpu_ms.p95, s.gpu_ms.p99), (5.0, 5.0, 5.0));
    }

    #[test]
    fn summarize_empty_is_none() {
        assert!(summarize(&[], 5.0).is_none());
    }

    #[test]
    fn proximity_follows_hmd_events_only() {
        let (started, ended) = (EVREventType_VREvent_TrackedDeviceUserInteractionStarted, EVREventType_VREvent_TrackedDeviceUserInteractionEnded);
        assert_eq!(proximity(None, started, 0), Some(true));
        assert_eq!(proximity(Some(true), ended, 0), Some(false));
        assert_eq!(proximity(Some(true), ended, 3), Some(true), "not the hmd");
        assert_eq!(proximity(None, EVREventType_VREvent_Quit, 0), None, "unrelated event");
    }

    #[test]
    fn matrix34_to_pose() {
        let m = [[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 2.0], [0.0, 0.0, 1.0, 3.0]];
        assert_eq!(pose(&m), ([1.0, 2.0, 3.0], [0.0, 0.0, 0.0, 1.0]));
        // 90° yaw: x -> -z, z -> x
        let m = [[0.0, 0.0, 1.0, 0.0], [0.0, 1.0, 0.0, 0.0], [-1.0, 0.0, 0.0, 0.0]];
        let (_, q) = pose(&m);
        assert!(Quat::from_array(q).abs_diff_eq(Quat::from_rotation_y(FRAC_PI_2), 1e-6), "{q:?}");
    }

    #[test]
    fn assets_parse() {
        let [actions, knuckles, hand]: [serde_json::Value; 3] = ASSETS.map(|(_, text)| serde_json::from_str(text).unwrap());
        for side in ["left", "right"] {
            let name = format!("/actions/main/in/skeleton_{side}");
            let action = actions["actions"].as_array().unwrap().iter().find(|a| a["name"] == name.as_str()).unwrap();
            assert_eq!((&action["type"], &action["skeleton"]), (&"skeleton".into(), &format!("/skeleton/hand/{side}").into()));
            for bindings in [&knuckles, &hand] {
                let skeleton = bindings["bindings"]["/actions/main"]["skeleton"].as_array().unwrap();
                let path = format!("/user/hand/{side}/input/skeleton/{side}");
                assert!(skeleton.iter().any(|b| b["output"] == name.as_str() && b["path"] == path.as_str()), "{bindings}");
            }
        }
        let types: Vec<_> = actions["default_bindings"].as_array().unwrap().iter().map(|b| b["controller_type"].clone()).collect();
        assert_eq!(types, ["knuckles", "svl_hand_interaction_augmented"]);
    }
}
