//! The `HostedBroker` implementation.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use vam_spec::v1::hosted_broker_server::HostedBroker;
use vam_spec::v1::{
    ErrorStatus, HookDisposition, HostArtifact, InvokeRequest, InvokeResponse, ListToolsRequest,
    ListToolsResponse, OnCompleteRequest, OnCompleteResponse, OnPausedRequest, OnPausedResponse,
    OnRequestRequest, OnRequestResponse, OnResponseRequest, OnResponseResponse, RegisterRequest,
    RegisterResponse, ResultSource, ResultSourceKind, ToolResult, VamErrorCode,
};

use crate::error::{FetchError, ProviderError};
use crate::fetch::artifact::{self, PageMeta};
use crate::fetch::content::{clean_line, MAX_TITLE_CHARS};
use crate::fetch::preview::{self, MAX_PREVIEW_BYTES};
use crate::fetch::{FetchOutcome, Page, PageFetcher, TOTAL_TIMEOUT};
use crate::provider::{redact, Hit, ResolvedSecrets, SearchProvider};
use crate::tools::{self, WEB_FETCH, WEB_SEARCH};

/// Invoke deadline when the host sends none.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
/// Held back from the host's deadline so the answer still arrives in time.
const DEADLINE_MARGIN: Duration = Duration::from_millis(500);

/// Web broker: `web_search` through a provider, `web_fetch` through a page
/// fetcher.
#[derive(Clone)]
pub struct WebBroker {
    provider: Arc<dyn SearchProvider>,
    fetcher: Arc<dyn PageFetcher>,
}

impl WebBroker {
    pub fn new(provider: Arc<dyn SearchProvider>, fetcher: Arc<dyn PageFetcher>) -> Self {
        Self { provider, fetcher }
    }

    fn deadline(request: &InvokeRequest) -> Duration {
        let ms = request.context.as_ref().map(|c| c.timeout_ms).unwrap_or(0);
        let total = if ms > 0 {
            Duration::from_millis(ms as u64)
        } else {
            DEFAULT_TIMEOUT
        };
        total
            .saturating_sub(DEADLINE_MARGIN)
            .max(Duration::from_millis(500))
    }

    async fn search(&self, request: InvokeRequest) -> InvokeResponse {
        let secrets = ResolvedSecrets::new(request.resolved_secrets.clone());
        let deadline = Self::deadline(&request);
        let args = request
            .args
            .as_ref()
            .map(tools::struct_to_json)
            .unwrap_or_else(|| json!({}));
        let outcome = async {
            let query = tools::parse_search_args(&args)?;
            self.provider.search(&query, &secrets, deadline).await
        }
        .await;
        match outcome {
            Ok(hits) => search_success(self.provider.name(), hits, &secrets),
            Err(err) => error_response(&err, &secrets),
        }
    }
}

impl WebBroker {
    async fn fetch(&self, request: InvokeRequest) -> InvokeResponse {
        let secrets = ResolvedSecrets::new(request.resolved_secrets.clone());
        let deadline = Self::deadline(&request).min(TOTAL_TIMEOUT);
        let args = request
            .args
            .as_ref()
            .map(tools::struct_to_json)
            .unwrap_or_else(|| json!({}));
        let outcome = async {
            let fetch = tools::parse_fetch_args(&args)?;
            let outcome = self.fetcher.fetch(&fetch.url, deadline).await?;
            Ok::<_, FetchError>((outcome, fetch.focus))
        }
        .await;
        match outcome {
            Ok((FetchOutcome::Page(page), focus)) => page_response(*page, focus.as_deref()),
            Ok((
                FetchOutcome::Redirected {
                    requested_url,
                    final_url,
                    redirect_to,
                },
                _,
            )) => redirected_response(&requested_url, &final_url, &redirect_to),
            Err(err) => typed_error(
                err.vam_code(),
                redact(&err.model_message(), &secrets),
                err.retryable(),
            ),
        }
    }
}

/// A page: the full text goes to a workspace file, the model reads a preview.
/// `sources` carries the requested URL (so a page fetched from a search hit
/// keeps that hit's id) and no content.
fn page_response(page: Page, focus: Option<&str>) -> InvokeResponse {
    let meta = PageMeta {
        requested_url: &page.requested_url,
        final_url: page.final_url.as_str(),
        title: &page.title,
        retrieved_at: &page.retrieved_at,
        content_type: &page.content_type,
    };
    let file = artifact::build(&meta, &page.final_url, &page.markdown);
    let preview = preview::select(&file.kept_text, focus, MAX_PREVIEW_BYTES);
    let title = clean_line(&page.title, MAX_TITLE_CHARS);
    let output = json!({
        "status": "ok",
        "url": page.requested_url,
        "final_url": page.final_url.as_str(),
        "title": title,
        "retrieved_at": page.retrieved_at,
        "content_type": page.content_type,
        "raw_bytes": page.raw_bytes,
        "saved_bytes": file.bytes.len(),
        "truncated": file.truncated,
        "focus_matched": preview.focus_matched,
        "file": file.relpath,
        "preview": preview.text,
        "note": "The full page is saved as a workspace file; read it for detail. The preview shows the top of the page, or the sections that match focus.",
    });
    let source_title = if title.is_empty() {
        page.final_url.host_str().unwrap_or("").to_string()
    } else {
        title
    };
    InvokeResponse {
        result: Some(ToolResult {
            output: Some(tools::json_to_struct(&output)),
            error: None,
        }),
        output_blocks: vec![],
        artifact: Some(HostArtifact {
            suggested_relpath: file.relpath,
            bytes: file.bytes,
        }),
        sources: vec![ResultSource {
            url: page.requested_url,
            title: source_title,
            published: String::new(),
            kind: ResultSourceKind::Page as i32,
        }],
    }
}

/// A cross-host redirect: a structured result, nothing saved, no source.
fn redirected_response(requested: &str, final_url: &str, redirect_to: &str) -> InvokeResponse {
    let output = json!({
        "status": "redirected",
        "url": requested,
        "final_url": final_url,
        "redirect_to": redirect_to,
        "message": "This address redirects to a different site. Nothing was fetched or saved. To read it, call web_fetch again with url set to redirect_to.",
    });
    InvokeResponse {
        result: Some(ToolResult {
            output: Some(tools::json_to_struct(&output)),
            error: None,
        }),
        output_blocks: vec![],
        artifact: None,
        sources: vec![],
    }
}

fn search_success(provider: &str, hits: Vec<Hit>, secrets: &ResolvedSecrets) -> InvokeResponse {
    // Snippets go only into the text the model reads; `sources` carries no snippet.
    let results: Vec<_> = hits
        .iter()
        .map(|h| {
            json!({
                "title": h.title,
                "url": h.url,
                "published": h.published,
                "snippet": h.snippet,
            })
        })
        .collect();
    let output = json!({
        "provider": provider,
        "result_count": results.len(),
        "results": results,
    });
    let sources = hits
        .into_iter()
        .map(|h| ResultSource {
            url: h.url,
            title: h.title,
            published: h.published.unwrap_or_default(),
            kind: ResultSourceKind::SearchResult as i32,
        })
        .collect();
    let _ = secrets;
    InvokeResponse {
        result: Some(ToolResult {
            output: Some(tools::json_to_struct(&output)),
            error: None,
        }),
        output_blocks: vec![],
        artifact: None,
        sources,
    }
}

fn error_response(err: &ProviderError, secrets: &ResolvedSecrets) -> InvokeResponse {
    let code = match err {
        ProviderError::InvalidQuery(_) => VamErrorCode::InvalidArgument,
        ProviderError::MissingSecret => VamErrorCode::FailedPrecondition,
        ProviderError::ProviderAuth => VamErrorCode::Unauthenticated,
        ProviderError::RateLimited { .. } => VamErrorCode::ResourceExhausted,
        ProviderError::Timeout => VamErrorCode::DeadlineExceeded,
        ProviderError::Upstream { .. } | ProviderError::BadResponse => VamErrorCode::Unavailable,
        ProviderError::Fixture(_) => VamErrorCode::Internal,
    };
    typed_error(code, redact(&err.model_message(), secrets), err.retryable())
}

fn typed_error(code: VamErrorCode, message: String, retryable: bool) -> InvokeResponse {
    InvokeResponse {
        result: Some(ToolResult {
            output: None,
            error: Some(ErrorStatus {
                code: code as i32,
                message,
                retryable,
                details: vec![],
            }),
        }),
        output_blocks: vec![],
        artifact: None,
        sources: vec![],
    }
}

#[tonic::async_trait]
impl HostedBroker for WebBroker {
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
        let request = request.into_inner();
        let response = match request.tool_name.as_str() {
            WEB_SEARCH => self.search(request).await,
            WEB_FETCH => self.fetch(request).await,
            other => typed_error(
                VamErrorCode::NotFound,
                format!("unknown_tool: this broker has no tool named '{other}'"),
                false,
            ),
        };
        Ok(tonic::Response::new(response))
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
    use std::collections::HashMap;

    use tokio::net::TcpListener;
    use vam_spec::v1::hosted_broker_client::HostedBrokerClient;

    use super::*;
    use crate::fetch::FixtureFetcher;
    use crate::provider::mock::MockProvider;
    use crate::serve::serve_tcp_listener;

    async fn start(dir: &std::path::Path) -> HostedBrokerClient<tonic::transport::Channel> {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let broker = WebBroker::new(
            Arc::new(MockProvider::new(dir.to_path_buf())),
            Arc::new(FixtureFetcher::new(dir.to_path_buf())),
        );
        tokio::spawn(async move {
            let _ = serve_tcp_listener(listener, broker).await;
        });
        HostedBrokerClient::connect(format!("http://{addr}"))
            .await
            .expect("connect")
    }

    fn invoke(tool: &str, args: serde_json::Value) -> InvokeRequest {
        InvokeRequest {
            tool_name: tool.to_string(),
            args: Some(tools::json_to_struct(&args)),
            resolved_secrets: HashMap::new(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn list_tools_and_search_round_trip() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(
            dir.path().join("default.json"),
            r#"{"results":[{"title":"A","url":"https://a.example/","snippet":"snippet text","published":"2026-02-03"}]}"#,
        )
        .expect("write");
        let mut client = start(dir.path()).await;

        let listed = client
            .list_tools(ListToolsRequest::default())
            .await
            .expect("list")
            .into_inner();
        let names: Vec<_> = listed.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["web_search", "web_fetch"]);

        let response = client
            .invoke(invoke("web_search", json!({"query": "anything"})))
            .await
            .expect("invoke")
            .into_inner();
        assert!(response
            .result
            .as_ref()
            .and_then(|r| r.error.as_ref())
            .is_none());
        assert_eq!(response.sources.len(), 1);
        assert_eq!(response.sources[0].url, "https://a.example/");
        assert_eq!(response.sources[0].published, "2026-02-03");
        assert_eq!(
            response.sources[0].kind,
            ResultSourceKind::SearchResult as i32
        );
        let output = tools::struct_to_json(
            response
                .result
                .as_ref()
                .and_then(|r| r.output.as_ref())
                .expect("output"),
        );
        assert_eq!(output["results"][0]["snippet"], "snippet text");
        assert_eq!(output["result_count"], 1.0);
    }

    #[tokio::test]
    async fn fetch_page_round_trip_with_artifact_preview_and_source() {
        let dir = tempfile::tempdir().expect("tmp");
        let url = "https://docs.example/guide";
        let pages = dir.path().join("pages");
        std::fs::create_dir_all(&pages).expect("mkdir");
        std::fs::write(
            pages.join(format!("{}.html", FixtureFetcher::fixture_key(url))),
            "<html><head><title>The Guide</title></head><body><h1>Guide</h1><p>Intro.</p>\
             <h2>Pricing</h2><p>Plans cost money.</p><h2>Setup</h2><p>Install it.</p></body></html>",
        )
        .expect("write");
        let mut client = start(dir.path()).await;

        let response = client
            .invoke(invoke(
                "web_fetch",
                json!({"url": url, "focus": "pricing plans"}),
            ))
            .await
            .expect("invoke")
            .into_inner();
        assert!(response
            .result
            .as_ref()
            .and_then(|r| r.error.as_ref())
            .is_none());
        let output = tools::struct_to_json(
            response
                .result
                .as_ref()
                .and_then(|r| r.output.as_ref())
                .expect("output"),
        );
        assert_eq!(output["status"], "ok");
        assert_eq!(output["title"], "The Guide");
        assert_eq!(output["focus_matched"], true);
        let preview = output["preview"].as_str().expect("preview");
        assert!(preview.contains("Plans cost money."));
        assert!(!preview.contains("Install it."));
        // Output text carries no snippet-like page body beyond the preview.
        assert!(output["preview"].as_str().expect("p").len() <= 4096);

        let artifact = response.artifact.expect("artifact");
        assert!(artifact.suggested_relpath.starts_with("web-docs.example-"));
        assert!(artifact.suggested_relpath.ends_with(".md"));
        assert_eq!(output["file"], artifact.suggested_relpath.as_str());
        assert!(artifact.bytes.len() <= crate::fetch::artifact::MAX_ARTIFACT_BYTES);
        let file = String::from_utf8(artifact.bytes).expect("utf8");
        assert!(file.starts_with("Source: https://docs.example/guide\n"));
        assert!(file.contains("Retrieved: "));
        assert!(file.contains("Install it."));

        assert_eq!(response.sources.len(), 1);
        assert_eq!(response.sources[0].url, url);
        assert_eq!(response.sources[0].title, "The Guide");
        assert_eq!(response.sources[0].kind, ResultSourceKind::Page as i32);
    }

    #[tokio::test]
    async fn cross_host_redirect_is_a_structured_result_without_artifact_or_source() {
        let dir = tempfile::tempdir().expect("tmp");
        let pages = dir.path().join("pages");
        std::fs::create_dir_all(&pages).expect("mkdir");
        std::fs::write(
            pages.join(format!(
                "{}.json",
                FixtureFetcher::fixture_key("https://old.example/")
            )),
            r#"{"redirect_to":"https://new.example/home"}"#,
        )
        .expect("write");
        let mut client = start(dir.path()).await;
        let response = client
            .invoke(invoke("web_fetch", json!({"url": "https://old.example/"})))
            .await
            .expect("invoke")
            .into_inner();
        assert!(response
            .result
            .as_ref()
            .and_then(|r| r.error.as_ref())
            .is_none());
        assert!(response.artifact.is_none());
        assert!(response.sources.is_empty());
        let output = tools::struct_to_json(
            response
                .result
                .as_ref()
                .and_then(|r| r.output.as_ref())
                .expect("output"),
        );
        assert_eq!(output["status"], "redirected");
        assert_eq!(output["redirect_to"], "https://new.example/home");
        assert!(output["message"].as_str().expect("m").contains("web_fetch"));
    }

    #[tokio::test]
    async fn fetch_errors_and_bad_args_are_typed() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut client = start(dir.path()).await;

        for (args, code) in [
            (json!({"url": "ftp://a.example/"}), "invalid_url:"),
            (json!({"url": "http://a.example:8080/"}), "invalid_url:"),
            (json!({"url": "https://u:p@a.example/"}), "invalid_url:"),
            (
                json!({"url": format!("https://a.example/{}", "a".repeat(2100))}),
                "url_too_long:",
            ),
            (json!({}), "invalid_url:"),
            (
                json!({"url": "https://a.example/", "extra": 1}),
                "invalid_url:",
            ),
            (
                json!({"url": "https://nothing.example/"}),
                "not_accessible:",
            ),
        ] {
            let response = client
                .invoke(invoke("web_fetch", args.clone()))
                .await
                .expect("invoke")
                .into_inner();
            let error = response.result.and_then(|r| r.error).expect("error");
            assert!(error.message.starts_with(code), "{args}: {}", error.message);
            assert!(response.artifact.is_none());
            assert!(response.sources.is_empty());
        }

        let bad = client
            .invoke(invoke("web_search", json!({"query": ""})))
            .await
            .expect("invoke")
            .into_inner();
        let error = bad.result.and_then(|r| r.error).expect("error");
        assert!(error.message.starts_with("invalid_query:"));

        let unknown = client
            .invoke(invoke("nope", json!({})))
            .await
            .expect("invoke")
            .into_inner();
        assert!(unknown
            .result
            .and_then(|r| r.error)
            .expect("error")
            .message
            .starts_with("unknown_tool:"));
    }
}
