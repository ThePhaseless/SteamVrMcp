use alvr_client_core::{ClientCapabilities, ClientCoreContext, ClientCoreEvent};
use alvr_common::{
    Fov, Pose, RelaxedAtomic, ViewParams,
    anyhow::{Context, Result, bail},
    glam::{Quat, UVec2, Vec3},
    parking_lot::Mutex,
};
use crate::{alvr_api::Api, rig::Rig};
use openh264::{decoder::Decoder, formats::YUVSource};
use serde::{Deserialize, Serialize};
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

/// Which part of the side-by-side frame a screenshot shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum Eye {
    #[default]
    Both,
    Left,
    Right,
}

#[derive(Debug, Serialize)]
pub struct Screenshot {
    #[serde(skip)]
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub frame_timestamp_ms: u64,
}

#[derive(Default)]
struct Shared {
    status: HeadsetStatus,
    config_nal: Vec<u8>,
    /// Set by a screenshot in progress: the first IDR frame to arrive lands in `idr`.
    armed: bool,
    idr: Option<(Duration, Vec<u8>)>,
    /// Timestamp of the newest video frame, for ALVR's client statistics.
    last_frame: Duration,
}

impl Shared {
    /// The decoder callback. Accepts a frame only once DecoderConfig has arrived, as a real client
    /// does: the server sends DecoderConfig only in reply to RequestIdr (server_core
    /// connection.rs:1287-1294), and the client sends one for every frame not accepted.
    fn on_frame(&mut self, timestamp: Duration, nal: &[u8]) -> bool {
        self.status.frames_received += 1;
        self.last_frame = timestamp;
        if self.armed && self.idr.is_none() && has_idr(nal) {
            self.idr = Some((timestamp, nal.to_vec()));
        }
        self.status.codec.is_some()
    }

    fn on_decoder_config(&mut self, codec: String, config_nal: Vec<u8>) {
        self.status.codec = Some(codec);
        self.config_nal = config_nal;
    }

    fn on_streaming_stopped(&mut self) {
        let status = &mut self.status;
        (status.streaming, status.fps, status.view_resolution, status.codec) = (false, 0.0, [0, 0], None);
    }
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

/// Whether an Annex-B stream holds an IDR slice (NAL type 5).
pub fn has_idr(annexb: &[u8]) -> bool {
    annexb.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 5)
}

/// Decodes one IDR frame, after the codec's config NALs, into `(width, height, RGB8)`.
// shortcut: decodes one IDR per call; keep a running decoder if callers need many frames per second
pub fn decode_idr(config_nal: &[u8], frame: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = Decoder::new()?;
    let yuv = decoder.decode(&[config_nal, frame].concat())?.context("the decoder produced no picture")?;
    let (w, h) = yuv.dimensions();
    let mut rgb = vec![0; w * h * 3];
    yuv.write_rgb8(&mut rgb);
    Ok((w as u32, h as u32, rgb))
}

/// The part of a side-by-side RGB8 frame `eye` shows: ALVR packs the left eye on the left half.
fn crop(eye: Eye, width: u32, height: u32, rgb: &[u8]) -> (u32, Vec<u8>) {
    let (width, half) = (width as usize, width as usize / 2);
    let (from, to) = match eye {
        Eye::Both => return (width as u32, rgb.to_vec()),
        Eye::Left => (0, half),
        Eye::Right => (half, width),
    };
    let rows = (0..height as usize).flat_map(|y| &rgb[(y * width + from) * 3..(y * width + to) * 3]);
    ((to - from) as u32, rows.copied().collect())
}

fn view_params() -> [ViewParams; 2] {
    let eye = |x: f32, fov| ViewParams { pose: Pose { orientation: Quat::IDENTITY, position: Vec3::new(x, 0.0, 0.0) }, fov };
    [
        eye(-0.0315, Fov { left: -0.96, right: 0.87, up: 0.96, down: -0.96 }),
        eye(0.0315, Fov { left: -0.87, right: 0.96, up: 0.96, down: -0.96 }),
    ]
}

fn tracking_thread(context: Arc<ClientCoreContext>, rig: Arc<Mutex<Rig>>, streaming: Arc<RelaxedAtomic>, fps: f32) {
    let origin = Instant::now();
    let mut deadline = Instant::now();
    while streaming.value() {
        let data = rig.lock().tracking(Instant::now(), origin.elapsed());
        context.send_tracking(data);
        deadline += Duration::from_secs_f32(1.0 / fps / 3.0);
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
    }
}

/// Port of the mock's `client_thread` loop: events, then one emulated frame per frame period.
fn event_thread(context: Arc<ClientCoreContext>, shared: Arc<Mutex<Shared>>, rig: Arc<Mutex<Rig>>, stop: Arc<RelaxedAtomic>) {
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
                    let (context, rig, streaming) = (Arc::clone(&context), Arc::clone(&rig), Arc::clone(&streaming));
                    tracking = Some(thread::spawn(move || tracking_thread(context, rig, streaming, fps)));
                }
                ClientCoreEvent::StreamingStopped => {
                    streaming.set(false);
                    if let Some(t) = tracking.take() {
                        t.join().ok();
                    }
                    shared.lock().on_streaming_stopped();
                }
                ClientCoreEvent::DecoderConfig { codec, config_nal } => {
                    shared.lock().on_decoder_config(format!("{codec:?}"), config_nal);
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
    /// Starts the client; its tracking thread sends what `rig` holds.
    pub fn start(rig: Arc<Mutex<Rig>>) -> Headset {
        let context = Arc::new(ClientCoreContext::new(capabilities(), vec![]));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(RelaxedAtomic::new(false));
        let frames = Arc::clone(&shared);
        context.set_decoder_input_callback(Box::new(move |timestamp, nal| frames.lock().on_frame(timestamp, nal)));
        context.resume();
        let thread = thread::spawn({
            let (context, shared, stop) = (Arc::clone(&context), Arc::clone(&shared), Arc::clone(&stop));
            move || event_thread(context, shared, rig, stop)
        });
        Headset { context, shared, stop, thread: Some(thread) }
    }

    pub fn status(&self) -> HeadsetStatus {
        self.shared.lock().status.clone()
    }

    /// A handle that takes screenshots without the headset itself, so callers need not hold a lock around the wait.
    pub fn grabber(&self) -> Grabber {
        Grabber(Arc::clone(&self.shared))
    }

    pub fn reconnect(&self) {
        self.pause();
        self.resume();
    }

    /// Disconnects and stays disconnected until `resume`.
    pub fn pause(&self) {
        self.context.pause();
    }

    pub fn resume(&self) {
        self.context.resume();
    }
}

pub struct Grabber(Arc<Mutex<Shared>>);

impl Grabber {
    /// Asks ALVR for an IDR frame, waits up to 2 s for it, and returns `eye` of it as a PNG.
    // shortcut: one screenshot at a time (a second call's disarm cuts the first short), enough for one MCP client
    pub fn screenshot(&self, api: &Api, eye: Eye) -> Result<Screenshot> {
        let codec = self.0.lock().status.codec.clone().context("the headset has not received ALVR's decoder config yet")?;
        if codec != "H264" {
            bail!("screenshots need H264; ALVR codec is {codec}");
        }
        {
            let mut shared = self.0.lock();
            (shared.armed, shared.idr) = (true, None);
        }
        let posted = api.post("/api/insert-idr");
        let deadline = Instant::now() + Duration::from_secs(2);
        let frame = loop {
            let mut shared = self.0.lock();
            if posted.is_err() || shared.idr.is_some() || Instant::now() >= deadline {
                shared.armed = false;
                break shared.idr.take().map(|idr| (idr, shared.config_nal.clone()));
            }
            drop(shared);
            thread::sleep(Duration::from_millis(10));
        };
        posted?;
        let ((timestamp, frame), config_nal) = frame.context("no IDR frame arrived within 2 s")?;
        let (width, height, rgb) = decode_idr(&config_nal, &frame)?;
        let (width, rgb) = crop(eye, width, height, &rgb);
        let mut png = vec![];
        let mut encoder = png::Encoder::new(&mut png, width, height);
        (encoder.set_color(png::ColorType::Rgb), encoder.set_depth(png::BitDepth::Eight));
        encoder.write_header()?.write_image_data(&rgb)?;
        Ok(Screenshot { png, width, height, frame_timestamp_ms: timestamp.as_millis() as u64 })
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

    #[test]
    fn frames_are_accepted_only_after_decoder_config() {
        let mut s = Shared::default();
        // not accepted: the client then sends RequestIdr, the only trigger for DecoderConfig
        assert!(!s.on_frame(Duration::from_millis(1), &[]));
        s.on_decoder_config("H264".into(), vec![0, 0, 1]);
        assert!(s.on_frame(Duration::from_millis(2), &[]));
        s.on_streaming_stopped();
        assert!(!s.on_frame(Duration::from_millis(3), &[]), "a new stream needs a new DecoderConfig");
        assert_eq!((s.status.frames_received, s.last_frame), (3, Duration::from_millis(3)));
        assert_eq!(s.status.codec, None);
    }

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/red_blue.h264");

    #[test]
    fn has_idr_finds_type_5() {
        assert!(has_idr(FIXTURE));
        assert!(!has_idr(&[0, 0, 0, 1, 0x41, 0x9a]));
    }

    #[test]
    fn decodes_fixture_into_red_left_blue_right() {
        let (w, h, rgb) = decode_idr(&[], FIXTURE).unwrap();
        assert_eq!((w, h), (128, 64));
        let px = |x: usize, y: usize| &rgb[(y * w as usize + x) * 3..][..3];
        let (red, blue) = (px(10, 10), px(100, 10));
        assert!(red[0] > 200 && red[1] < 60 && red[2] < 60, "{red:?}");
        assert!(blue[2] > 200 && blue[0] < 60 && blue[1] < 60, "{blue:?}");
    }

    #[test]
    fn left_eye_crop_is_red_half() {
        let (w, h, rgb) = decode_idr(&[], FIXTURE).unwrap();
        let (cw, left) = crop(Eye::Left, w, h, &rgb);
        assert_eq!((cw, left.len()), (64, 64 * 64 * 3));
        assert!(left.chunks(3).all(|p| p[0] > 200 && p[2] < 60));
        let (cw, right) = crop(Eye::Right, w, h, &rgb);
        assert_eq!(cw, 64);
        assert!(right.chunks(3).all(|p| p[2] > 200 && p[0] < 60));
        assert_eq!(crop(Eye::Both, w, h, &rgb), (128, rgb));
    }

    #[test]
    fn armed_slot_keeps_the_first_idr_only() {
        let mut s = Shared::default();
        s.on_frame(Duration::from_millis(1), FIXTURE);
        assert_eq!(s.idr, None, "not armed");
        s.armed = true;
        s.on_frame(Duration::from_millis(2), &[0, 0, 0, 1, 0x41, 0x9a]);
        assert_eq!(s.idr, None, "not an IDR");
        s.on_frame(Duration::from_millis(3), FIXTURE);
        s.on_frame(Duration::from_millis(4), FIXTURE);
        assert_eq!(s.idr, Some((Duration::from_millis(3), FIXTURE.to_vec())));
    }
}
