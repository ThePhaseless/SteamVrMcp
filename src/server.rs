use crate::{
    checks::{self, Readiness},
    headset::Eye,
    rig::{
        AnimateArgs, AnimateControllerArgs, AxisArgs, ButtonArgs, ControllerPoseArgs, ControllerStateArgs, FingerCurlArgs, GripArgs,
        HandGestureArgs, HandModeArgs, HandSkeletonArgs, MoveArgs, MoveControllerArgs, PoseArgs, RigAction, TouchArgs, TrackpadArgs,
        TriggerArgs,
    },
    stack::{LogQuery, SettingValue, Stack},
};
use alvr_common::anyhow::{self, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, SubscriptionFilter,
    },
    service::{NotificationContext, RequestContext, SubscriptionContext},
    tool, tool_router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Client names of hosts that ignore `notifications/tools/list_changed`. They get every tool
/// listed, and a gated one answers `not ready: <reason>`.
pub const HOSTS_WITHOUT_LIST_CHANGED: &[&str] = &[];

pub struct VrServer {
    stack: Arc<Stack>,
    router: ToolRouter<VrServer>,
}

/// The client name: from `initialize` on a legacy connection, from each request's `_meta` on a 2026-07-28 one.
fn client_name(context: &RequestContext<RoleServer>) -> String {
    context.client_info().map(|c| c.name).unwrap_or_default()
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct ScreenshotArgs {
    /// Which eye to show; "both" is the side-by-side frame ALVR streams.
    #[serde(default)]
    eye: Eye,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct TrackedStateArgs {
    /// Also return the 31 OpenVR hand bones per hand (model space, without controller).
    #[serde(default)]
    include_bones: bool,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct HapticsArgs {
    /// Only events with t_s after this; pass the last t_s seen to get new ones.
    since_s: Option<f64>,
    /// Empty the log after reading.
    #[serde(default)]
    clear: bool,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct SettingsArgs {
    values: Vec<SettingValue>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct LaunchArgs {
    /// Decimal: a Steam app id (e.g. "620980"), a non-Steam shortcut's 32-bit app id or its 64-bit game id.
    app_id: String,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum Flavour {
    /// A bare hand drives the Valve Index controller device.
    Index,
    /// A bare hand is a separate full-skeleton hand-tracker device, as VRChat expects.
    SkeletalTracker,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct FlavourArgs {
    flavour: Flavour,
}

/// A tool result with `v` as pretty JSON text.
fn json(v: impl Serialize) -> Result<CallToolResult> {
    Ok(CallToolResult::success(vec![ContentBlock::text(serde_json::to_string_pretty(&v)?)]))
}

/// Error text for a tool result the caller reads.
fn message(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[tool_router(vis = "pub(crate)")]
impl VrServer {
    pub fn new(stack: Arc<Stack>) -> VrServer {
        VrServer { stack, router: Self::tool_router() }
    }

    /// Runs blocking Stack work off the async runtime.
    async fn run(&self, f: impl FnOnce(&Stack) -> Result<CallToolResult> + Send + 'static) -> Result<CallToolResult> {
        let stack = Arc::clone(&self.stack);
        tokio::task::spawn_blocking(move || f(&stack)).await?
    }

    #[tool(description = "Startup checks (ok, reason, fix) as of MCP start or the last vr_start, with the ALVR dashboard check re-run, and the current state of Steam, SteamVR, ALVR, the virtual headset (streaming, fps, resolution, codec) and, while the OpenVR reader is connected, the scene app.")]
    async fn vr_status(&self) -> Result<CallToolResult, String> {
        self.run(|s| json(serde_json::json!({ "checks": s.checks(), "state": s.status() }))).await.map_err(message)
    }

    #[tool(description = "Start Steam if needed, SteamVR with ALVR and the virtual headset, and return once it streams, or with the reason it stopped.")]
    async fn vr_start(&self) -> Result<CallToolResult, String> {
        self.run(|s| json(s.start()?)).await.map_err(message)
    }

    #[tool(description = "Shut SteamVR down through ALVR and disconnect the headset. Closes Steam only if vr_start started it.")]
    async fn vr_stop(&self) -> Result<CallToolResult, String> {
        self.run(|s| json(s.stop()?)).await.map_err(message)
    }

    #[tool(description = "A PNG screenshot of what the headset streams, from a fresh keyframe: both eyes side by side, or one eye.")]
    async fn screenshot(&self, Parameters(args): Parameters<ScreenshotArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| {
            let shot = s.screenshot(args.eye)?;
            Ok(CallToolResult::success(vec![
                ContentBlock::image(STANDARD.encode(&shot.png), "image/png"),
                ContentBlock::text(serde_json::to_string(&shot)?),
            ]))
        })
        .await
        .map_err(message)
    }

    #[tool(description = "What SteamVR itself tracks: each device's class, role, serial, model, connection, pose (SteamVR standing space: meters, +Y up, -Z forward; quat x, y, z, w) and velocities; per hand the skeleton action's state, finger curls and splay as SteamVR animates them and as the driver reports them, and optionally the bones; plus the scene app's pid and state.")]
    async fn get_tracked_state(&self, Parameters(args): Parameters<TrackedStateArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| {
            let state = s.tracked_state(args.include_bones)?;
            json(serde_json::json!({ "devices": state.devices, "skeletons": state.skeletons, "scene_app": s.scene_app() }))
        })
        .await
        .map_err(message)
    }

    /// Applies `a`; an animation returns once it is done.
    async fn rig(&self, a: RigAction) -> Result<CallToolResult, String> {
        let animated = matches!(a, RigAction::AnimateHead(_) | RigAction::AnimateController(_));
        let done_after = self.stack.apply(a).map_err(message)?; // a brief rig lock, no blocking call
        if !animated {
            return json(serde_json::json!({ "ok": true })).map_err(message);
        }
        tokio::time::sleep(done_after).await; // no Stack lock is held here
        json(serde_json::json!({ "done_after_s": done_after.as_secs_f32() })).map_err(message)
    }

    #[tool(description = "Set the head pose in SteamVR standing space (meters, +Y up, -Z forward): position plus yaw/pitch/roll in degrees, or a quaternion x, y, z, w. Stops an animation.")]
    async fn set_head_pose(&self, Parameters(args): Parameters<PoseArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetHeadPose(args)).await
    }

    #[tool(description = "Move the head relative to where it is: offset in meters along the playspace axes or, with frame \"local\", the head's own axes; yaw/pitch/roll in degrees about the head's own axes.")]
    async fn move_head(&self, Parameters(args): Parameters<MoveArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::MoveHead(args)).await
    }

    #[tool(description = "Move the head smoothly from its current pose through the waypoints, evenly spaced over duration_s, sending velocities as a real headset does. Returns when done.")]
    async fn animate_head(&self, Parameters(args): Parameters<AnimateArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::AnimateHead(args)).await
    }

    #[tool(description = "Set a Valve Index controller's pose, in the playspace or relative to the head's pose now (relative_to \"head\"). Same pose fields as set_head_pose.")]
    async fn set_controller_pose(&self, Parameters(args): Parameters<ControllerPoseArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetControllerPose(args)).await
    }

    #[tool(description = "Move a controller relative to where it is, as move_head does for the head.")]
    async fn move_controller(&self, Parameters(args): Parameters<MoveControllerArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::MoveController(args)).await
    }

    #[tool(description = "Animate a controller through waypoints, as animate_head does; waypoints are in the playspace or relative to the head's pose at the call. Returns when done.")]
    async fn animate_controller(&self, Parameters(args): Parameters<AnimateControllerArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::AnimateController(args)).await
    }

    #[tool(description = "Connect or disconnect a controller. ALVR 21 has one flag for both connected and tracked (Controller.cpp:187-205), so either false makes the controller disconnected and untracked. Input tools refuse for it.")]
    async fn set_controller_state(&self, Parameters(args): Parameters<ControllerStateArgs>) -> Result<CallToolResult, String> {
        let active = args.active();
        self.rig(RigAction::SetControllerState(args)).await?;
        json(serde_json::json!({
            "ok": true,
            "connected": active,
            "tracked": active,
            "note": "ALVR 21 sets connected and tracked from one flag per hand (Controller.cpp:187-205)",
        }))
        .map_err(message)
    }

    #[tool(description = "Curl the thumb, index, middle, ring and pinky fingers (0 straight to 1 curled) and set the splay (0 together to 1 spread); an unset value keeps its last value. The hand model turns them into a 26-joint skeleton sent with the controller pose. SteamVR gets index to pinky also as /input/finger/* and the thumb through the skeleton only.")]
    async fn set_finger_curl(&self, Parameters(args): Parameters<FingerCurlArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetFingerCurl(args)).await
    }

    #[tool(description = "Per hand: hold a Valve Index controller (\"controller\") or be a bare tracked hand (\"hand\"), which sends only a skeleton, open unless set. With ALVR's default Index flavour a bare hand drives the Index controller device, and ALVR's gesture mapping turns pinches into buttons. Immediate.")]
    async fn set_hand_mode(&self, Parameters(args): Parameters<HandModeArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetHandMode(args)).await
    }

    #[tool(description = "Send raw hand joints: 26 entries in OpenXR XR_EXT_hand_tracking order (palm, wrist, thumb metacarpal..tip, index..little metacarpal..tip), each px, py, pz in meters in the playspace and qx, qy, qz, qw (-Z along the bone toward the tip, +Y the back of the hand). Holds until the next curl or gesture call.")]
    async fn set_hand_skeleton(&self, Parameters(args): Parameters<HandSkeletonArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetHandSkeleton(args)).await
    }

    #[tool(description = "Pose a hand through the hand model: give either gesture (open, fist, point, pinch, thumbs_up) or custom curls (thumb, index, middle, ring, pinky, splay, each 0 to 1, unset 0), not both. A bare hand's pinch meets ALVR's pinch distance, so it acts as the trigger.")]
    async fn set_hand_gesture(&self, Parameters(args): Parameters<HandGestureArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetHandGesture(args)).await
    }

    #[tool(description = "Press a Valve Index button and hold it until release_button: a, b, system, thumbstick (click and touch), trigger (value 1, click, touch), grip (squeeze value 1, force 0) or trackpad (force 1, touch). Refuses for an untracked controller or a bare hand.")]
    async fn press_button(&self, Parameters(args): Parameters<ButtonArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::PressButton(args)).await
    }

    #[tool(description = "Release a button: everything press_button set for it goes back to false or 0.")]
    async fn release_button(&self, Parameters(args): Parameters<ButtonArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::ReleaseButton(args)).await
    }

    #[tool(description = "Touch or untouch a button's capacitive sensor without pressing it: a, b, system, trigger, thumbstick or trackpad. The grip has no touch in ALVR's Index profile.")]
    async fn touch_button(&self, Parameters(args): Parameters<TouchArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::TouchButton(args)).await
    }

    #[tool(description = "Set the trigger value, 0 to 1; click follows at >= 0.95 and touch at > 0.")]
    async fn set_trigger(&self, Parameters(args): Parameters<TriggerArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetTrigger(args)).await
    }

    #[tool(description = "Set the grip (squeeze) value and force, each 0 to 1; force defaults to 0.")]
    async fn set_grip(&self, Parameters(args): Parameters<GripArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetGrip(args)).await
    }

    #[tool(description = "Set the thumbstick: x and y from -1 to 1 (+x right, +y up), click (default false), touch (default: deflected or clicked). Holds until changed; set 0, 0 to center it.")]
    async fn set_thumbstick(&self, Parameters(args): Parameters<AxisArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetThumbstick(args)).await
    }

    #[tool(description = "Set the trackpad: x and y from -1 to 1 (+x right, +y up), force 0 to 1 (default 0), touch (default true). Holds until changed; send touch false to lift the finger.")]
    async fn set_trackpad(&self, Parameters(args): Parameters<TrackpadArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetTrackpad(args)).await
    }

    #[tool(description = "Haptic events the app sent, oldest first: t_s (seconds on the MCP's clock), hand (left, right or other), duration_s, frequency (Hz) and amplitude (0 to 1). Keeps the newest 1024.")]
    async fn get_haptics(&self, Parameters(args): Parameters<HapticsArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.haptics(args.since_s.unwrap_or(f64::NEG_INFINITY), args.clear))).await.map_err(message)
    }

    #[tool(description = "Set ALVR session values by path (ALVR's syntax from the session JSON root, e.g. session_settings.video.preferred_fps) and check ALVR stored them. Unless ALVR's settings schema flags every path real-time, SteamVR then restarts and the call returns once the headset streams again (restarted, restarts, streaming). A bad path or value fails before any restart.")]
    async fn set_alvr_settings(&self, Parameters(args): Parameters<SettingsArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.set_alvr_settings(args.values)?)).await.map_err(message)
    }

    #[tool(description = "How a bare tracked hand appears in SteamVR: \"index\" (ALVR's default; it drives the Index controller and pinches act as buttons) or \"skeletal_tracker\" (a separate full-skeleton hand tracker per hand). Sets ALVR's steamvr_input_2_0 through set_alvr_settings, so SteamVR restarts.")]
    async fn set_hand_tracking_flavour(&self, Parameters(args): Parameters<FlavourArgs>) -> Result<CallToolResult, String> {
        let skeletal = matches!(args.flavour, Flavour::SkeletalTracker);
        self.run(move |s| json(s.set_hand_tracking_flavour(skeletal)?)).await.map_err(message)
    }

    #[tool(description = "ALVR log events (kept since vr_start, newest 1000) and the tails of SteamVR's vrserver, vrcompositor and vrmonitor logs, merged in time order: t (local time of day), source, level, text. Filter by source (alvr, vrserver, vrcompositor, vrmonitor or all), text (contains, ignoring case) and min_level (error, warning, info, debug; applies to ALVR events only, SteamVR lines are filtered by text only). Returns the newest limit lines (1 to 1000, default 100).")]
    async fn get_logs(&self, Parameters(args): Parameters<LogQuery>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.logs(args)?)).await.map_err(message)
    }

    #[tool(description = "Launch a Steam app or non-Steam shortcut through Steam after checking it exists, and return once SteamVR reports its scene running: the app (app_id, game_id, kind, name, compat_tool such as a Proton version) and scene_pid. Otherwise fails with the reason: a Steam dialog waiting on the desktop, or a timeout with the last vrserver log lines.")]
    async fn launch_app(&self, Parameters(args): Parameters<LaunchArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.launch_app(&args.app_id)?)).await.map_err(message)
    }

    #[tool(description = "Stop the scene app SteamVR reports: SIGTERM to its pid, SIGKILL after 15 s. Refuses when the scene is SteamVR Home. Returns stopped_pid. A Proton game's other wine processes may outlive it.")]
    async fn stop_app(&self) -> Result<CallToolResult, String> {
        self.run(|s| json(s.stop_app()?)).await.map_err(message)
    }

    #[tool(description = "Put back the SteamVR and ALVR setup from before the first vr_start. Refuses while SteamVR runs.")]
    async fn vr_restore(&self) -> Result<CallToolResult, String> {
        self.run(|s| json(s.restore().map(|()| serde_json::json!({ "restored": true }))?)).await.map_err(message)
    }
}

impl VrServer {
    /// Readiness read now, after re-running the startup checks if `recheck`.
    async fn readiness(&self, recheck: bool) -> Result<Readiness, ErrorData> {
        let stack = Arc::clone(&self.stack);
        tokio::task::spawn_blocking(move || {
            if recheck {
                stack.recheck();
            }
            stack.readiness()
        })
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }

    /// Makes the watcher thread send `notifications/tools/list_changed` through `notify`.
    fn forward_changes<F: Future<Output = ()> + Send + 'static>(&self, notify: impl Fn() -> F + Send + Sync + 'static) {
        let runtime = tokio::runtime::Handle::current();
        self.stack.set_on_change(Box::new(move || {
            runtime.spawn(notify());
        }));
    }
}

impl ServerHandler for VrServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build())
            .with_server_info(Implementation::new("vr-test-mcp", env!("CARGO_PKG_VERSION")))
    }

    /// Legacy lifecycle (`initialize`): notifications go straight to the peer.
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let client = context.peer.peer_info().map(|i| i.client_info.name.clone()).unwrap_or_default();
        eprintln!("vr-test-mcp: client {client:?} initialized");
        let peer = context.peer;
        self.forward_changes(move || {
            let peer = peer.clone();
            async move { peer.notify_tool_list_changed().await.unwrap_or_else(|e| eprintln!("vr-test-mcp: {e}")) }
        });
    }

    fn accepted_subscription_filter(&self, requested: &SubscriptionFilter) -> Option<SubscriptionFilter> {
        Some(requested.supported_by(&self.get_info().capabilities))
    }

    /// 2026-07-28 lifecycle: notifications go through the client's `subscriptions/listen` request.
    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        eprintln!("vr-test-mcp: client {:?} listens", client_name(context.request_context()));
        let sink = context.sink().clone();
        self.forward_changes(move || {
            let sink = sink.clone();
            async move { sink.notify_tool_list_changed().await.unwrap_or_else(|e| eprintln!("vr-test-mcp: {e}")) }
        });
        context.cancelled().await;
        Ok(())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let r = self.readiness(false).await?;
        let list_all = HOSTS_WITHOUT_LIST_CHANGED.contains(&client_name(&context).as_str());
        let tools = self.router.list_all().into_iter().filter(|t| list_all || checks::tool_ready(&t.name, &r).is_ok()).collect();
        Ok(ListToolsResult { tools, ..Default::default() })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // static checks run again at each vr_start, so a fixed problem does not need an MCP restart
        let r = self.readiness(request.name == "vr_start").await?;
        if let Err(reason) = checks::tool_ready(&request.name, &r) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!("not ready: {reason}"))]).into());
        }
        self.router.call(ToolCallContext::new(self, request, context)).await
    }
}

pub async fn serve(stack: Arc<Stack>) -> Result<()> {
    VrServer::new(stack).serve(rmcp::transport::stdio()).await?.waiting().await?;
    Ok(())
}
