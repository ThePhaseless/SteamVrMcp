use alvr_client_core::{ClientCapabilities, ClientCoreContext, ClientCoreEvent};
use alvr_common::{
    DeviceMotion, Fov, HEAD_ID, Pose, RelaxedAtomic, ViewParams,
    anyhow::{Context, Result},
    glam::{Quat, UVec2, Vec3},
    parking_lot::Mutex,
};
use alvr_packets::{FaceData, TrackingData};
use serde::Serialize;
use std::{
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, Serialize)]
pub struct HeadsetStatus {
    pub streaming: bool,
    pub fps: f32,
    pub view_resolution: [u32; 2],
    pub codec: Option<String>,
    pub frames_received: u64,
    pub hud_message: String,
}

#[derive(Default)]
struct Shared {
    status: HeadsetStatus,
    #[allow(dead_code)] // Task 4 decodes screenshots starting from it
    config_nal: Vec<u8>,
    /// Timestamp of the newest video frame, for ALVR's client statistics.
    last_frame: Duration,
}

/// The virtual headset: a stock `ClientCoreContext` driven like `alvr_client_mock`, without its GUI.
pub struct Headset {
    context: Arc<ClientCoreContext>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<RelaxedAtomic>,
    thread: Option<JoinHandle<()>>,
}

pub fn capabilities() -> ClientCapabilities {
    let resolution = UVec2::new(1088, 1216);
    ClientCapabilities {
        platform: alvr_system_info::platform(None, None),
        default_view_resolution: resolution,
        max_view_resolution: resolution,
        refresh_rates: vec![72.0, 90.0],
        foveated_encoding: false,
        encoder_high_profile: false,
        encoder_10_bits: false,
        encoder_av1: false,
        prefer_10bit: false,
        preferred_encoding_gamma: 1.0,
        prefer_hdr: false,
    }
}

/// The hostname `ClientCoreContext::new` stored; read it after `Headset::start`, which may reset it.
pub fn client_hostname() -> Result<String> {
    let path = dirs::config_dir().context("no config dir")?.join("ALVR Client/session.json");
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).context("reading ALVR client session")?)?;
    json["hostname"].as_str().map(str::to_owned).context("no hostname in ALVR client session")
}

fn view_params() -> [ViewParams; 2] {
    let eye = |x: f32, fov| ViewParams { pose: Pose { orientation: Quat::IDENTITY, position: Vec3::new(x, 0.0, 0.0) }, fov };
    [
        eye(-0.0315, Fov { left: -0.96, right: 0.87, up: 0.96, down: -0.96 }),
        eye(0.0315, Fov { left: -0.87, right: 0.96, up: 0.96, down: -0.96 }),
    ]
}

fn tracking_thread(context: Arc<ClientCoreContext>, streaming: Arc<RelaxedAtomic>, fps: f32) {
    let origin = Instant::now();
    let mut deadline = Instant::now();
    while streaming.value() {
        let pose = Pose { orientation: Quat::IDENTITY, position: Vec3::new(0.0, 1.6, 0.0) };
        context.send_tracking(TrackingData {
            poll_timestamp: origin.elapsed(),
            device_motions: vec![(*HEAD_ID, DeviceMotion { pose, linear_velocity: Vec3::ZERO, angular_velocity: Vec3::ZERO })],
            hand_skeletons: [None, None],
            face: FaceData::default(),
            body: None,
        });
        deadline += Duration::from_secs_f32(1.0 / fps / 3.0);
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
    }
}

/// Port of the mock's `client_thread` loop: events, then one emulated frame per frame period.
fn event_thread(context: Arc<ClientCoreContext>, shared: Arc<Mutex<Shared>>, stop: Arc<RelaxedAtomic>) {
    let streaming = Arc::new(RelaxedAtomic::new(false));
    let mut tracking = None;
    let mut reported_frame = Duration::ZERO;
    let mut deadline = Instant::now();
    while !stop.value() {
        while let Some(event) = context.poll_event() {
            match event {
                ClientCoreEvent::UpdateHudMessage(message) => shared.lock().status.hud_message = message,
                ClientCoreEvent::StreamingStarted(config) => {
                    let negotiated = &config.negotiated_config;
                    let fps = negotiated.refresh_rate_hint;
                    {
                        let status = &mut shared.lock().status;
                        status.streaming = true;
                        status.fps = fps;
                        status.view_resolution = negotiated.view_resolution.to_array();
                    }
                    context.send_view_params(view_params());
                    streaming.set(true);
                    let (context, streaming) = (Arc::clone(&context), Arc::clone(&streaming));
                    tracking = Some(thread::spawn(move || tracking_thread(context, streaming, fps)));
                }
                ClientCoreEvent::StreamingStopped => {
                    streaming.set(false);
                    if let Some(t) = tracking.take() {
                        t.join().ok();
                    }
                    let status = &mut shared.lock().status;
                    (status.streaming, status.fps, status.view_resolution, status.codec) = (false, 0.0, [0, 0], None);
                }
                ClientCoreEvent::DecoderConfig { codec, config_nal } => {
                    let mut shared = shared.lock();
                    shared.status.codec = Some(format!("{codec:?}"));
                    shared.config_nal = config_nal;
                }
                ClientCoreEvent::Haptics { .. } | ClientCoreEvent::RealTimeConfig(_) => (),
            }
        }

        // The mock reports a constant zero timestamp, which matches no frame and spams
        // "Statistics summary not ready!"; report the newest received frame instead.
        let frame = shared.lock().last_frame;
        if frame != reported_frame {
            reported_frame = frame;
            context.report_compositor_start(frame);
            context.report_submit(frame, Duration::ZERO);
        }

        let fps = shared.lock().status.fps;
        deadline += Duration::from_secs_f32(1.0 / if fps > 0.0 { fps } else { 72.0 });
        thread::sleep(deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(3)));
    }
    streaming.set(false);
    if let Some(t) = tracking {
        t.join().ok();
    }
}

impl Headset {
    pub fn start() -> Headset {
        let context = Arc::new(ClientCoreContext::new(capabilities(), vec![]));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(RelaxedAtomic::new(false));
        let frames = Arc::clone(&shared);
        // true for every frame: false makes the client request an IDR on every frame
        context.set_decoder_input_callback(Box::new(move |timestamp, _nal| {
            let mut shared = frames.lock();
            shared.status.frames_received += 1;
            shared.last_frame = timestamp;
            true
        }));
        context.resume();
        let thread = thread::spawn({
            let (context, shared, stop) = (Arc::clone(&context), Arc::clone(&shared), Arc::clone(&stop));
            move || event_thread(context, shared, stop)
        });
        Headset { context, shared, stop, thread: Some(thread) }
    }

    pub fn status(&self) -> HeadsetStatus {
        self.shared.lock().status.clone()
    }

    pub fn reconnect(&self) {
        self.context.pause();
        self.context.resume();
    }
}

impl Drop for Headset {
    fn drop(&mut self) {
        self.stop.set(true);
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
        // the context's own Drop joins its connection thread once this last Arc goes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alvr_common::glam::UVec2;

    #[test]
    fn capabilities_are_modest() {
        let c = capabilities();
        assert_eq!(c.default_view_resolution, UVec2::new(1088, 1216));
        assert_eq!(c.max_view_resolution, c.default_view_resolution);
        assert_eq!(c.default_view_resolution % 32, UVec2::ZERO);
        assert_eq!(c.refresh_rates, vec![72.0, 90.0]);
        assert!(!c.foveated_encoding);
    }
}
