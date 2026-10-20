use crate::{checks::{self, Readiness}, stack::Stack};
use alvr_common::anyhow::{self, Result};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext},
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, SubscriptionFilter,
    },
    service::{NotificationContext, RequestContext, SubscriptionContext},
    tool, tool_router,
};
use serde::Serialize;
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

    #[tool(description = "Every startup check (ok, reason, fix) and the current state of Steam, SteamVR, ALVR and the virtual headset.")]
    async fn vr_status(&self) -> Result<CallToolResult, String> {
        self.run(|s| json(serde_json::json!({ "checks": s.recheck(), "state": s.status() }))).await.map_err(message)
    }

    #[tool(description = "Start Steam if needed, SteamVR with ALVR and the virtual headset, and return once it streams, or with the reason it stopped.")]
    async fn vr_start(&self) -> Result<CallToolResult, String> {
        self.run(|s| json(s.start()?)).await.map_err(message)
    }

    #[tool(description = "Shut SteamVR down through ALVR and disconnect the headset. Closes Steam only if vr_start started it.")]
    async fn vr_stop(&self) -> Result<CallToolResult, String> {
        self.run(|s| json(s.stop()?)).await.map_err(message)
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
