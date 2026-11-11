//! Test broker for effects the world counts (`hsp-echo-broker --effects`).
//!
//! A create appends one line to `<dir>/ledger.jsonl` and syncs it BEFORE it holds and answers,
//! so a kill during the hold leaves an effect that happened with no answer. The broker is its
//! own process: the ledger survives a guest kill and a vamd restart, and the count is the
//! world's, not the host's.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use vam_spec::v1::hosted_broker_server::HostedBroker;
use vam_spec::v1::{
    HostExecSpec, InvokeRequest, InvokeResponse, ListToolsRequest, ListToolsResponse,
    OnCompleteRequest, OnCompleteResponse, OnPausedRequest, OnPausedResponse, OnRequestRequest,
    OnRequestResponse, OnResponseRequest, OnResponseResponse, RegisterRequest, RegisterResponse,
    SideEffectClass, ToolResult, ToolSpec,
};

use crate::hsp_echo::EchoBroker;

pub const LEDGER_FILE: &str = "ledger.jsonl";
pub const CREATE_TICKET: &str = "echo_create_ticket";
pub const CREATE_NOTE: &str = "echo_create_note";
pub const FIND_TICKET: &str = "echo_find_ticket";

/// The ledger line of one call.
fn line(tool: &str, title: &str, key: &str) -> String {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    serde_json::json!({"tool": tool, "title": title, "key": key, "at": at}).to_string()
}

/// Every parsable ledger line, oldest first.
pub fn read_ledger(dir: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join(LEDGER_FILE))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Append one line and sync it; returns the number of lines for `tool` now in the ledger.
pub fn append_ledger(dir: &Path, tool: &str, title: &str, key: &str) -> std::io::Result<usize> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(LEDGER_FILE);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{}", line(tool, title, key))?;
    f.sync_all()?;
    Ok(read_ledger(dir)
        .iter()
        .filter(|l| l["tool"].as_str() == Some(tool))
        .count())
}

/// `{"tool","title","count"}` per (tool, title), one JSON line each, sorted.
pub fn ledger_counts(dir: &Path) -> Vec<String> {
    let mut counts: BTreeMap<(String, String), u64> = BTreeMap::new();
    for l in read_ledger(dir) {
        let k = (
            l["tool"].as_str().unwrap_or_default().to_string(),
            l["title"].as_str().unwrap_or_default().to_string(),
        );
        *counts.entry(k).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|((tool, title), count)| {
            serde_json::json!({"tool": tool, "title": title, "count": count}).to_string()
        })
        .collect()
}

fn str_arg(args: &Option<prost_types::Struct>, key: &str) -> Option<String> {
    match args.as_ref()?.fields.get(key)?.kind.as_ref()? {
        prost_types::value::Kind::StringValue(s) => Some(s.clone()),
        _ => None,
    }
}

fn num_arg(args: &Option<prost_types::Struct>, key: &str) -> Option<f64> {
    match args.as_ref()?.fields.get(key)?.kind.as_ref()? {
        prost_types::value::Kind::NumberValue(n) => Some(*n),
        _ => None,
    }
}

fn sv(s: impl Into<String>) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(s.into())),
    }
}

fn spec(name: &str, description: &str, schema: &str, class: SideEffectClass) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        input_schema_json: schema.to_string(),
        execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(
            HostExecSpec {
                adapter_id: "broker-echo".to_string(),
                config: Default::default(),
            },
        )),
        side_effect: class as i32,
        ..Default::default()
    }
}

/// A broker whose creates are counted in its own ledger.
#[derive(Debug, Clone)]
pub struct EffectsTestBroker {
    dir: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl EffectsTestBroker {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            lock: Arc::new(Mutex::new(())),
        }
    }
}

/// Serve the effects broker over a Unix socket.
#[cfg(unix)]
pub async fn serve_unix_effects(
    listener: tokio::net::UnixListener,
    broker: EffectsTestBroker,
) -> Result<(), tonic::transport::Error> {
    tonic::transport::Server::builder()
        .add_service(vam_spec::v1::hosted_broker_server::HostedBrokerServer::new(
            broker,
        ))
        .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener))
        .await
}

#[tonic::async_trait]
impl HostedBroker for EffectsTestBroker {
    async fn register(
        &self,
        request: tonic::Request<RegisterRequest>,
    ) -> Result<tonic::Response<RegisterResponse>, tonic::Status> {
        EchoBroker.register(request).await
    }

    async fn list_tools(
        &self,
        _request: tonic::Request<ListToolsRequest>,
    ) -> Result<tonic::Response<ListToolsResponse>, tonic::Status> {
        let create = r#"{"type":"object","properties":{"title":{"type":"string"},"holdMs":{"type":"number"}},"required":["title"]}"#;
        Ok(tonic::Response::new(ListToolsResponse {
            tools: vec![
                spec(
                    CREATE_TICKET,
                    "Create a ticket with a title; answers its id",
                    create,
                    SideEffectClass::MutableExternal,
                ),
                spec(
                    CREATE_NOTE,
                    "Create a note with a title; answers its id",
                    create,
                    SideEffectClass::MutableExternal,
                ),
                spec(
                    FIND_TICKET,
                    "Find a ticket by title",
                    r#"{"type":"object","properties":{"title":{"type":"string"}},"required":["title"]}"#,
                    SideEffectClass::ReadOnly,
                ),
            ],
        }))
    }

    async fn invoke(
        &self,
        request: tonic::Request<InvokeRequest>,
    ) -> Result<tonic::Response<InvokeResponse>, tonic::Status> {
        let req = request.into_inner();
        let tool = req.tool_name.as_str();
        if ![CREATE_TICKET, CREATE_NOTE, FIND_TICKET].contains(&tool) {
            return Err(tonic::Status::not_found(format!(
                "tool {tool} is not served"
            )));
        }
        let title = str_arg(&req.args, "title").unwrap_or_default();
        let mut fields = BTreeMap::new();
        if tool == FIND_TICKET {
            let found = {
                let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
                let found = read_ledger(&self.dir)
                    .iter()
                    .filter(|l| {
                        l["tool"].as_str() == Some(CREATE_TICKET)
                            && l["title"].as_str() == Some(title.as_str())
                    })
                    .count();
                append_ledger(&self.dir, FIND_TICKET, &title, "")
                    .map_err(|e| tonic::Status::internal(format!("ledger: {e}")))?;
                found
            };
            fields.insert(
                "found".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::BoolValue(found > 0)),
                },
            );
            if found > 0 {
                fields.insert("id".to_string(), sv(format!("T-{found}")));
            }
        } else {
            // The effect is written down first; only then does the call hold and answer.
            let n = {
                let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
                append_ledger(&self.dir, tool, &title, "")
                    .map_err(|e| tonic::Status::internal(format!("ledger: {e}")))?
            };
            if let Some(ms) = num_arg(&req.args, "holdMs") {
                tokio::time::sleep(std::time::Duration::from_millis(ms.max(0.0) as u64)).await;
            }
            let prefix = if tool == CREATE_TICKET { "T" } else { "N" };
            fields.insert("id".to_string(), sv(format!("{prefix}-{n}")));
        }
        Ok(tonic::Response::new(InvokeResponse {
            result: Some(ToolResult {
                output: Some(prost_types::Struct { fields }),
                error: None,
            }),
            output_blocks: vec![],
            artifact: None,
            sources: Default::default(),
        }))
    }

    async fn on_request(
        &self,
        request: tonic::Request<OnRequestRequest>,
    ) -> Result<tonic::Response<OnRequestResponse>, tonic::Status> {
        EchoBroker.on_request(request).await
    }

    async fn on_complete(
        &self,
        request: tonic::Request<OnCompleteRequest>,
    ) -> Result<tonic::Response<OnCompleteResponse>, tonic::Status> {
        EchoBroker.on_complete(request).await
    }

    async fn on_response(
        &self,
        request: tonic::Request<OnResponseRequest>,
    ) -> Result<tonic::Response<OnResponseResponse>, tonic::Status> {
        EchoBroker.on_response(request).await
    }

    async fn on_paused(
        &self,
        request: tonic::Request<OnPausedRequest>,
    ) -> Result<tonic::Response<OnPausedResponse>, tonic::Status> {
        EchoBroker.on_paused(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(tool: &str, title: &str, hold: Option<f64>) -> tonic::Request<InvokeRequest> {
        let mut f = BTreeMap::new();
        f.insert("title".to_string(), sv(title));
        if let Some(h) = hold {
            f.insert(
                "holdMs".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::NumberValue(h)),
                },
            );
        }
        tonic::Request::new(InvokeRequest {
            tool_name: tool.to_string(),
            args: Some(prost_types::Struct { fields: f }),
            ..Default::default()
        })
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("effects-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[tokio::test]
    async fn a_create_is_in_the_ledger_before_the_hold_ends() {
        let dir = tmp("hold");
        let b = EffectsTestBroker::new(dir.clone());
        let call = tokio::spawn({
            let b = b.clone();
            async move { b.invoke(req(CREATE_TICKET, "Q4 plan", Some(600.0))).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!call.is_finished(), "still holding");
        assert_eq!(read_ledger(&dir).len(), 1, "the effect is already written");
        let out = call.await.unwrap().unwrap().into_inner();
        assert_eq!(str_arg(&out.result.unwrap().output, "id").unwrap(), "T-1");
    }

    #[tokio::test]
    async fn a_kill_during_the_hold_leaves_the_line() {
        let dir = tmp("kill");
        let b = EffectsTestBroker::new(dir.clone());
        let call = tokio::spawn({
            let b = b.clone();
            async move { b.invoke(req(CREATE_NOTE, "n", Some(60_000.0))).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        call.abort();
        let counts = ledger_counts(&dir);
        assert_eq!(counts.len(), 1);
        assert!(counts[0].contains("\"count\":1") && counts[0].contains(CREATE_NOTE));
    }

    #[tokio::test]
    async fn find_sees_a_created_ticket_and_counts_as_a_read() {
        let dir = tmp("find");
        let b = EffectsTestBroker::new(dir.clone());
        b.invoke(req(FIND_TICKET, "x", None)).await.unwrap();
        b.invoke(req(CREATE_TICKET, "x", None)).await.unwrap();
        let out = b
            .invoke(req(FIND_TICKET, "x", None))
            .await
            .unwrap()
            .into_inner();
        let o = out.result.unwrap().output.unwrap();
        assert!(matches!(
            o.fields["found"].kind,
            Some(prost_types::value::Kind::BoolValue(true))
        ));
        assert_eq!(ledger_counts(&dir).len(), 2);
    }
}
