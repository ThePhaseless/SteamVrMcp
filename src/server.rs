use crate::{
    checks::{self, Readiness},
    headset::Eye,
    rig::{
        AnimateArgs, AnimateControllerArgs, ControllerPoseArgs, ControllerStateArgs, FingerCurlArgs, HandGestureArgs, HandModeArgs,
        HandSkeletonArgs, MoveArgs, MoveControllerArgs, PoseArgs, RigAction,
    },
    stack::Stack,
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

    #[tool(description = "Startup checks (ok, reason, fix) as of MCP start or the last vr_start, with the ALVR dashboard check re-run, and the current state of Steam, SteamVR, ALVR and the virtual headset.")]
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
