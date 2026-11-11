//! The `HostedBroker`: accepts `Register`, lists the six tools and dispatches `Invoke`
//! (`INVALID_ARGUMENT` without a caller identity).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use vam_spec::v1::hosted_broker_server::HostedBroker;
use vam_spec::v1::{
    ErrorStatus, HookDisposition, InvokeRequest, InvokeResponse, ListToolsRequest,
    ListToolsResponse, OnCompleteRequest, OnCompleteResponse, OnPausedRequest, OnPausedResponse,
    OnRequestRequest, OnRequestResponse, OnResponseRequest, OnResponseResponse, RegisterRequest,
    RegisterResponse, ToolResult, VamErrorCode,
};

use crate::clock::{Clock, SystemClock};
use crate::gateway::GatewayClient;
use crate::manage;
use crate::ops::{self, Caller, Ctx, ToolErr};
use crate::store::Store;
use crate::tools;

/// Why an Invoke without `CallerContext.user_id` and `agent_urn` is refused (vamd checks first).
pub const NO_IDENTITY: &str = "INVALID_ARGUMENT: the call carries no user and agent identity";

/// The broker: dispatches the tools to [`crate::ops`].
#[derive(Clone)]
pub struct ScheduleBroker {
    store: Arc<Store>,
    gateway: GatewayClient,
    clock: Arc<dyn Clock>,
    min_interval: Duration,
    wake: Arc<Notify>,
}

impl ScheduleBroker {
    pub fn new(store: Arc<Store>, gateway: GatewayClient) -> Self {
        Self {
            store,
            gateway,
            clock: Arc::new(SystemClock),
            min_interval: Duration::from_secs(15 * 60),
            wake: Arc::new(Notify::new()),
        }
    }

    /// The clock the tools read (tests move it).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The least time between two fires of a cron schedule.
    pub fn with_min_interval(mut self, min_interval: Duration) -> Self {
        self.min_interval = min_interval;
        self
    }

    /// The handle to notify after a create or update so the fire loop re-reads its next wake
    /// (the one from `Scheduler::wake_handle`).
    pub fn with_wake(mut self, wake: Arc<Notify>) -> Self {
        self.wake = wake;
        self
    }

    async fn run_tool(&self, req: &InvokeRequest) -> Result<serde_json::Value, ToolErr> {
        let cc = req.caller_context.clone().unwrap_or_default();
        let caller = Caller {
            user_id: cc.user_id,
            agent_urn: cc.agent_urn,
        };
        let args = match &req.args {
            Some(s) => ops::struct_to_json(s),
            None => serde_json::json!({}),
        };
        let Some(args) = args.as_object() else {
            return Err(ToolErr::invalid("arguments must be an object"));
        };
        let ctx = Ctx {
            store: &self.store,
            clock: self.clock.as_ref(),
            min_interval: self.min_interval,
            wake: &self.wake,
        };
        let grant = req.run_grant.as_ref();
        match req.tool_name.as_str() {
            tools::SCHEDULE_CREATE => ops::create(&ctx, &caller, args, grant),
            tools::SCHEDULE_LIST => ops::list(&ctx, &caller, args),
            tools::SCHEDULE_GET => ops::get(&ctx, &caller, args),
            tools::SCHEDULE_UPDATE => ops::update(&ctx, &caller, args, grant),
            tools::SCHEDULE_DELETE => manage::delete(&ctx, &self.gateway, &caller, args).await,
            tools::SCHEDULE_RUN_NOW => {
                manage::run_now(&ctx, &self.gateway, &caller, args, &req.run_id).await
            }
            other => Err(ToolErr::invalid(format!("unknown tool `{other}`"))),
        }
    }
}

fn refusal(code: VamErrorCode, message: &str) -> InvokeResponse {
    InvokeResponse {
        result: Some(ToolResult {
            output: None,
            error: Some(ErrorStatus {
                code: code as i32,
                message: message.to_string(),
                retryable: false,
                details: vec![],
            }),
        }),
        output_blocks: vec![],
        artifact: None,
        sources: vec![],
    }
}

#[tonic::async_trait]
impl HostedBroker for ScheduleBroker {
    async fn register(
        &self,
        _request: tonic::Request<RegisterRequest>,
    ) -> Result<tonic::Response<RegisterResponse>, tonic::Status> {
        Ok(tonic::Response::new(RegisterResponse {
            error: None,
            session_policy_version: String::new(),
        }))
    }

    async fn list_tools(
        &self,
        _request: tonic::Request<ListToolsRequest>,
    ) -> Result<tonic::Response<ListToolsResponse>, tonic::Status> {
        Ok(tonic::Response::new(ListToolsResponse {
            tools: tools::tool_specs(),
        }))
    }

    async fn invoke(
        &self,
        request: tonic::Request<InvokeRequest>,
    ) -> Result<tonic::Response<InvokeResponse>, tonic::Status> {
        let req = request.get_ref();
        let has_identity = req
            .caller_context
            .as_ref()
            .is_some_and(|c| !c.user_id.is_empty() && !c.agent_urn.is_empty());
        if !has_identity {
            return Ok(tonic::Response::new(refusal(
                VamErrorCode::InvalidArgument,
                NO_IDENTITY,
            )));
        }
        // Never log the arguments, the message or the grant: ids only (ops logs those).
        Ok(tonic::Response::new(match self.run_tool(req).await {
            Ok(v) => InvokeResponse {
                result: Some(ToolResult {
                    output: Some(ops::json_to_struct(&v)),
                    error: None,
                }),
                output_blocks: vec![],
                artifact: None,
                sources: vec![],
            },
            Err(e) => refusal(e.code, &e.message),
        }))
    }

    async fn on_request(
        &self,
        _request: tonic::Request<OnRequestRequest>,
    ) -> Result<tonic::Response<OnRequestResponse>, tonic::Status> {
        Ok(tonic::Response::new(OnRequestResponse {
            error: None,
            injected_context: vec![],
            disposition: HookDisposition::Ok as i32,
        }))
    }

    async fn on_complete(
        &self,
        _request: tonic::Request<OnCompleteRequest>,
    ) -> Result<tonic::Response<OnCompleteResponse>, tonic::Status> {
        Ok(tonic::Response::new(OnCompleteResponse {
            error: None,
            retention_labels: vec![],
            disposition: HookDisposition::Ok as i32,
        }))
    }

    async fn on_response(
        &self,
        _request: tonic::Request<OnResponseRequest>,
    ) -> Result<tonic::Response<OnResponseResponse>, tonic::Status> {
        Ok(tonic::Response::new(OnResponseResponse {
            error: None,
            response_blocks: vec![],
            disposition: HookDisposition::Ok as i32,
        }))
    }

    async fn on_paused(
        &self,
        _request: tonic::Request<OnPausedRequest>,
    ) -> Result<tonic::Response<OnPausedResponse>, tonic::Status> {
        Ok(tonic::Response::new(OnPausedResponse {
            error: None,
            disposition: HookDisposition::Ok as i32,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::CallerContext;

    fn broker() -> ScheduleBroker {
        ScheduleBroker::new(
            Arc::new(Store::open_in_memory().expect("store")),
            GatewayClient::new(None),
        )
    }

    async fn error_of(req: InvokeRequest) -> ErrorStatus {
        broker()
            .invoke(tonic::Request::new(req))
            .await
            .expect("answer")
            .into_inner()
            .result
            .and_then(|r| r.error)
            .expect("error")
    }

    fn with_identity(user: &str, agent: &str) -> InvokeRequest {
        InvokeRequest {
            tool_name: "schedule_list".into(),
            caller_context: Some(CallerContext {
                user_id: user.into(),
                agent_urn: agent.into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn an_unknown_tool_is_invalid_argument() {
        let mut req = with_identity("u1", "urn:vam:agent:a");
        req.tool_name = "schedule_nope".into();
        let err = error_of(req).await;
        assert_eq!(err.code, VamErrorCode::InvalidArgument as i32);
    }

    #[tokio::test]
    async fn invoke_runs_list_create_and_get_end_to_end() {
        use prost_types::Timestamp;
        let b = broker();
        let call = |tool: &str, args: serde_json::Value, grant: Option<vam_spec::v1::RunGrant>| {
            InvokeRequest {
                tool_name: tool.into(),
                args: Some(ops::json_to_struct(&args)),
                run_grant: grant,
                ..with_identity("u1", "urn:vam:agent:a")
            }
        };
        let out = |r: tonic::Response<InvokeResponse>| {
            ops::struct_to_json(&r.into_inner().result.unwrap().output.expect("output"))
        };
        let empty = out(b
            .invoke(tonic::Request::new(call(
                "schedule_list",
                serde_json::json!({}),
                None,
            )))
            .await
            .unwrap());
        assert_eq!(empty.to_string(), "{\"schedules\":[]}");
        let exp = chrono::Utc::now().timestamp() + 86_400;
        let grant = vam_spec::v1::RunGrant {
            token: "vam_rg_rg_B1.sek".into(),
            grant_id: "rg_B1".into(),
            expires_at: Some(Timestamp {
                seconds: exp,
                nanos: 0,
            }),
            ..Default::default()
        };
        let args = serde_json::json!({"title": "t", "when": {"once": {"in": "PT1H"}}, "run": {"message": "m"}});
        let made = out(b
            .invoke(tonic::Request::new(call(
                "schedule_create",
                args,
                Some(grant),
            )))
            .await
            .unwrap());
        let id = made["scheduleId"].as_str().expect("id").to_string();
        assert!(id.starts_with("sch_") && !id.contains("B1"));
        let got = out(b
            .invoke(tonic::Request::new(call(
                "schedule_get",
                serde_json::json!({"scheduleId": id}),
                None,
            )))
            .await
            .unwrap());
        assert_eq!(got["message"], "m");
    }

    #[tokio::test]
    async fn invoke_without_user_or_agent_is_invalid_argument() {
        for req in [
            InvokeRequest::default(),
            with_identity("", "urn:vam:agent:a"),
            with_identity("u1", ""),
        ] {
            let err = error_of(req).await;
            assert_eq!(err.code, VamErrorCode::InvalidArgument as i32);
            assert!(err.message.starts_with("INVALID_ARGUMENT"));
        }
    }

    #[tokio::test]
    async fn list_tools_answers_the_six() {
        let out = broker()
            .list_tools(tonic::Request::new(ListToolsRequest::default()))
            .await
            .expect("answer")
            .into_inner();
        assert_eq!(out.tools.len(), 6);
    }
}
