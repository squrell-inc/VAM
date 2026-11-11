use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vam_core::CidAllocator;
use vam_core::MockVmm;
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{mint_api_key_secret, MEMBER_SCOPES};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;
use vam_storage::{
    ChunkStore, Drive9Config, ManifestFile, ManifestManager, MerkleManifest, MAX_DOWNLOAD_BYTES,
};

fn test_app(state: AppState) -> axum::Router {
    api::router(state)
}

static USER_COUNTER: AtomicU64 = AtomicU64::new(0);
static DRIVE9_ENV_LOCK: Mutex<()> = Mutex::new(());

struct Drive9TestRoot {
    _dir: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drive9TestRoot {
    fn new() -> Self {
        let lock = DRIVE9_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("VAM_DRIVE9_ROOT", dir.path());
        Self {
            _dir: dir,
            _lock: lock,
        }
    }

    fn path(&self) -> &Path {
        self._dir.path()
    }
}

fn member_auth_store() -> (Arc<SqliteWorkflowStore>, String, String) {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.initialize_catalog_schema().unwrap();
    let n = USER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let user_id = format!("wsfiles-{n}");
    store.create_user(&user_id, &user_id).unwrap();
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = MEMBER_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key_owned(
            &format!("key-{user_id}"),
            &user_id,
            &secret,
            &format!("User::\"{user_id}\""),
            &scopes,
            &user_id,
        )
        .unwrap();
    (store, secret, user_id)
}

fn app_state_http_only(store: Arc<SqliteWorkflowStore>) -> AppState {
    let vault = SecretsVault::for_tests(store.clone());
    let cid_allocator = Arc::new(CidAllocator::default());
    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));
    let (manager, _cmd_rx) = SessionManager::new(store.clone(), cid_allocator, vmm_factory);
    AppState {
        store,
        vault,
        manager,
        pdp: Arc::new(ControlPlanePdp::new().expect("pdp")),
        listen_addr: "127.0.0.1:8080".to_string(),
        draining: Arc::new(AtomicBool::new(false)),
        ready: Arc::new(AtomicBool::new(true)),
        e2e_mock_scripts: false,
    }
}

async fn response_body(response: axum::response::Response) -> String {
    let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The Space of `user` labelled `label`, made on first use: a workspace is named by its id.
fn space(store: &Arc<SqliteWorkflowStore>, user: &str, label: &str) -> String {
    if let Some(found) = store
        .list_spaces(user, None, None, 1000)
        .unwrap()
        .into_iter()
        .find(|s| s.label == label)
    {
        return found.space_id;
    }
    store
        .create_space(user, None, label, "run", None)
        .unwrap()
        .space_id
}

fn commit_head(
    drive9_root: &std::path::Path,
    workspace_id: &str,
    files: Vec<ManifestFile>,
) -> String {
    let layout = Drive9Config::new(drive9_root).layout();
    let chunk_store = ChunkStore::new(layout.clone());
    let manager = ManifestManager::new(layout, chunk_store);
    let mut manifest = MerkleManifest::new(workspace_id.to_string(), None);
    manifest.files = files;
    let hash = manager.commit_manifest(&manifest).unwrap();
    manager.write_head(workspace_id, &hash).unwrap();
    hash
}

#[tokio::test]
async fn workspace_files_list_and_download_happy_path() {
    let (store, secret, user_id) = member_auth_store();
    let drive9_root = Drive9TestRoot::new();

    let ws = space(&store, &user_id, "assistant");
    let payload = b"hello workspace file";
    let layout = Drive9Config::new(drive9_root.path()).layout();
    let chunk_store = ChunkStore::new(layout.clone());
    let hash = chunk_store.put_chunk(payload).unwrap();
    commit_head(
        drive9_root.path(),
        &ws,
        vec![ManifestFile {
            path: "workspace/notes/a.md".into(),
            size_bytes: payload.len() as u64,
            mode_permissions: 0o644,
            chunks: vec![hash],
        }],
    );

    let app = test_app(app_state_http_only(store.clone()));
    let list = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&format!(
                    "/v1/workspaces/{}/files",
                    space(&store, &user_id, "assistant")
                ))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let list_json: serde_json::Value = serde_json::from_str(&response_body(list).await).unwrap();
    assert_eq!(
        list_json["workspaceId"],
        space(&store, &user_id, "assistant").as_str()
    );
    assert_eq!(list_json["files"].as_array().unwrap().len(), 1);
    assert_eq!(list_json["files"][0]["path"], "workspace/notes/a.md");

    let download = app
        .oneshot(
            Request::builder()
                .uri(&format!(
                    "/v1/workspaces/{}/files/workspace/notes/a.md",
                    space(&store, &user_id, "assistant")
                ))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(
        download
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "application/octet-stream"
    );
    let bytes = hyper::body::to_bytes(download.into_body()).await.unwrap();
    assert_eq!(bytes.as_ref(), payload);
}

#[tokio::test]
async fn workspace_files_list_excludes_skills() {
    let (store, secret, user_id) = member_auth_store();
    let drive9_root = Drive9TestRoot::new();
    let ws = space(&store, &user_id, "ws-skills");
    commit_head(
        drive9_root.path(),
        &ws,
        vec![
            ManifestFile {
                path: "skills/foo/SKILL.md".into(),
                size_bytes: 1,
                mode_permissions: 0o644,
                chunks: vec![],
            },
            ManifestFile {
                path: "workspace/only.txt".into(),
                size_bytes: 1,
                mode_permissions: 0o644,
                chunks: vec![],
            },
        ],
    );

    let app = test_app(app_state_http_only(store.clone()));
    let response = app
        .oneshot(
            Request::builder()
                .uri(&format!(
                    "/v1/workspaces/{}/files",
                    space(&store, &user_id, "ws-skills")
                ))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&response_body(response).await).unwrap();
    let paths: Vec<_> = body["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["workspace/only.txt"]);
}

#[tokio::test]
async fn workspace_files_deny_listed_download_is_404() {
    let (store, secret, user_id) = member_auth_store();
    let drive9_root = Drive9TestRoot::new();
    let ws = space(&store, &user_id, "ws-deny");
    commit_head(
        drive9_root.path(),
        &ws,
        vec![ManifestFile {
            path: "workspace/.vam/SYSTEM.md".into(),
            size_bytes: 1,
            mode_permissions: 0o644,
            chunks: vec![],
        }],
    );

    let app = test_app(app_state_http_only(store.clone()));
    let response = app
        .oneshot(
            Request::builder()
                .uri(&format!(
                    "/v1/workspaces/{}/files/workspace/.vam/SYSTEM.md",
                    space(&store, &user_id, "ws-deny")
                ))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn workspace_files_are_read_by_space_id_only() {
    let (store, secret, user_id) = member_auth_store();
    let drive9_root = Drive9TestRoot::new();
    let agent_a = "urn:vam:agent:local.vam.dev/acme/agent-a";
    let agent_b = "urn:vam:agent:local.vam.dev/acme/agent-b";
    // Two Spaces of one owner with one label: the label tells them apart from nothing.
    let space_a = store
        .create_space(&user_id, Some(agent_a), "main", "run", None)
        .unwrap()
        .space_id;
    let space_b = store
        .create_space(&user_id, Some(agent_b), "main", "run", None)
        .unwrap()
        .space_id;
    assert_ne!(space_a, space_b);
    let empty = |path: &str| ManifestFile {
        path: path.into(),
        size_bytes: 0,
        mode_permissions: 0o644,
        chunks: vec![],
    };
    commit_head(
        drive9_root.path(),
        &space_a,
        vec![empty("workspace/agent-a.txt")],
    );
    commit_head(
        drive9_root.path(),
        &space_b,
        vec![empty("workspace/agent-b.txt")],
    );

    let app = test_app(app_state_http_only(store.clone()));
    async fn list_files(app: &axum::Router, secret: &str, id: &str) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/workspaces/{id}/files"))
                    .header("authorization", format!("Bearer {secret}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    let a_resp = list_files(&app, &secret, &space_a).await;
    assert_eq!(a_resp.status(), StatusCode::OK);
    let a_json: serde_json::Value = serde_json::from_str(&response_body(a_resp).await).unwrap();
    assert_eq!(a_json["workspaceId"], space_a);
    assert_eq!(a_json["files"][0]["path"], "workspace/agent-a.txt");

    let b_resp = list_files(&app, &secret, &space_b).await;
    assert_eq!(b_resp.status(), StatusCode::OK);
    let b_json: serde_json::Value = serde_json::from_str(&response_body(b_resp).await).unwrap();
    assert_eq!(b_json["files"][0]["path"], "workspace/agent-b.txt");

    // A label, a legacy `{owner}--{label}` id and an agent-scoped id name nothing.
    for name in [
        "main".to_string(),
        format!("{user_id}--main"),
        format!("{user_id}--agent-a-34303b60--main"),
    ] {
        let resp = list_files(&app, &secret, &name).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{name}");
    }
}

#[tokio::test]
async fn workspace_files_of_another_owner_are_not_found() {
    let (store, secret, user_id) = member_auth_store();
    let drive9_root = Drive9TestRoot::new();

    let alice_ws = space(&store, &user_id, "assistant");
    commit_head(
        drive9_root.path(),
        &alice_ws,
        vec![ManifestFile {
            path: "workspace/alice-only.txt".into(),
            size_bytes: 0,
            mode_permissions: 0o644,
            chunks: vec![],
        }],
    );
    // Bob's Space is a row of his own; its files exist on disk under its id.
    let bob_ws = store
        .create_space("bob", None, "assistant", "run", None)
        .unwrap()
        .space_id;
    commit_head(
        drive9_root.path(),
        &bob_ws,
        vec![ManifestFile {
            path: "workspace/bob-only.txt".into(),
            size_bytes: 0,
            mode_permissions: 0o644,
            chunks: vec![],
        }],
    );

    let app = test_app(app_state_http_only(store.clone()));
    let get = |id: String| {
        Request::builder()
            .uri(format!("/v1/workspaces/{id}/files"))
            .header("authorization", format!("Bearer {secret}"))
            .body(Body::empty())
            .unwrap()
    };
    let response = app.clone().oneshot(get(alice_ws)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&response_body(response).await).unwrap();
    let paths: Vec<_> = body["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["workspace/alice-only.txt"]);

    // Bob's id answers exactly as an id that does not exist.
    let foreign = app.clone().oneshot(get(bob_ws)).await.unwrap();
    let foreign_status = foreign.status();
    let foreign_body = response_body(foreign).await;
    let missing = app
        .oneshot(get(vam_orchestrator::spaces::mint_id("sp_")))
        .await
        .unwrap();
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(foreign_body, response_body(missing).await);
}

#[tokio::test]
async fn workspace_files_oversize_returns_413() {
    let (store, secret, user_id) = member_auth_store();
    let drive9_root = Drive9TestRoot::new();
    let ws = space(&store, &user_id, "ws-big");
    commit_head(
        drive9_root.path(),
        &ws,
        vec![ManifestFile {
            path: "workspace/huge.bin".into(),
            size_bytes: MAX_DOWNLOAD_BYTES + 1,
            mode_permissions: 0o644,
            chunks: vec![],
        }],
    );

    let app = test_app(app_state_http_only(store.clone()));
    let response = app
        .oneshot(
            Request::builder()
                .uri(&format!(
                    "/v1/workspaces/{}/files/workspace/huge.bin",
                    space(&store, &user_id, "ws-big")
                ))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

fn put_request(uri: &str, secret: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("authorization", format!("Bearer {secret}"))
        .header("content-type", "application/octet-stream")
        .body(Body::from(body))
        .unwrap()
}

fn put_json_request(uri: &str, secret: &str, json: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("authorization", format!("Bearer {secret}"))
        .header("content-type", "application/json")
        .body(Body::from(json.to_string()))
        .unwrap()
}

#[tokio::test]
async fn workspace_files_put_happy_path_round_trips() {
    let (store, secret, user_id) = member_auth_store();
    let _drive9_root = Drive9TestRoot::new();
    let app = test_app(app_state_http_only(store.clone()));
    let payload = b"uploaded plan".to_vec();

    let put = app
        .clone()
        .oneshot(put_request(
            &format!(
                "/v1/workspaces/{}/files/workspace/notes/a.md",
                space(&store, &user_id, "assistant")
            ),
            &secret,
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&response_body(put).await).unwrap();
    assert_eq!(
        json["workspaceId"],
        space(&store, &user_id, "assistant").as_str()
    );
    assert_eq!(json["path"], "workspace/notes/a.md");
    assert_eq!(json["sizeBytes"], payload.len());
    assert!(json["head"].as_str().unwrap().len() > 8);

    let download = app
        .oneshot(
            Request::builder()
                .uri(&format!(
                    "/v1/workspaces/{}/files/workspace/notes/a.md",
                    space(&store, &user_id, "assistant")
                ))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    let bytes = hyper::body::to_bytes(download.into_body()).await.unwrap();
    assert_eq!(bytes.as_ref(), payload);
}

#[tokio::test]
async fn workspace_files_put_into_another_owners_space_is_not_found() {
    let (store, secret, _user_id) = member_auth_store();
    let drive9_root = Drive9TestRoot::new();
    let bob_ws = store
        .create_space("bob", None, "assistant", "run", None)
        .unwrap()
        .space_id;
    commit_head(
        drive9_root.path(),
        &bob_ws,
        vec![ManifestFile {
            path: "workspace/bob-only.txt".into(),
            size_bytes: 0,
            mode_permissions: 0o644,
            chunks: vec![],
        }],
    );

    let app = test_app(app_state_http_only(store.clone()));
    let put = app
        .oneshot(put_request(
            &format!("/v1/workspaces/{bob_ws}/files/workspace/planted.md"),
            &secret,
            b"from-alice".to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::NOT_FOUND);

    let layout = Drive9Config::new(drive9_root.path()).layout();
    let bob_read = vam_storage::read_workspace_file_from_head(
        &layout,
        &bob_ws,
        "workspace/planted.md",
        MAX_DOWNLOAD_BYTES,
    );
    assert!(matches!(
        bob_read,
        Err(vam_storage::Drive9Error::NotFound(_))
    ));
}

#[tokio::test]
async fn workspace_files_put_traversal_is_400() {
    let (store, secret, user_id) = member_auth_store();
    let _drive9_root = Drive9TestRoot::new();
    let app = test_app(app_state_http_only(store.clone()));
    let response = app
        .oneshot(put_request(
            &format!(
                "/v1/workspaces/{}/files/workspace/%2e%2e/%2e%2e/etc/passwd",
                space(&store, &user_id, "assistant")
            ),
            &secret,
            b"nope".to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn workspace_files_put_dot_vam_is_400() {
    let (store, secret, user_id) = member_auth_store();
    let _drive9_root = Drive9TestRoot::new();
    let app = test_app(app_state_http_only(store.clone()));
    let response = app
        .oneshot(put_request(
            &format!(
                "/v1/workspaces/{}/files/workspace/.vam/host-artifacts/x",
                space(&store, &user_id, "assistant")
            ),
            &secret,
            b"nope".to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn workspace_files_put_busy_workspace_is_409() {
    let (store, secret, user_id) = member_auth_store();
    let _drive9_root = Drive9TestRoot::new();
    let ws = space(&store, &user_id, "assistant");
    store
        .create_session_with_owner("sess-busy", None, "vm-busy", &ws, "default", &user_id)
        .unwrap();

    let app = test_app(app_state_http_only(store.clone()));
    let response = app
        .oneshot(put_request(
            &format!(
                "/v1/workspaces/{}/files/workspace/notes/a.md",
                space(&store, &user_id, "assistant")
            ),
            &secret,
            b"nope".to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn workspace_files_put_oversize_returns_413() {
    let (store, secret, user_id) = member_auth_store();
    let _drive9_root = Drive9TestRoot::new();
    let app = test_app(app_state_http_only(store.clone()));
    let body = vec![0u8; (MAX_DOWNLOAD_BYTES as usize) + 1];
    let response = app
        .oneshot(put_request(
            &format!(
                "/v1/workspaces/{}/files/workspace/huge.bin",
                space(&store, &user_id, "assistant")
            ),
            &secret,
            body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn workspace_files_put_json_base64_png_round_trips() {
    let (store, secret, user_id) = member_auth_store();
    let _drive9_root = Drive9TestRoot::new();
    let app = test_app(app_state_http_only(store.clone()));
    let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQUR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let json = format!(r#"{{"name":"pixel.png","media_type":"image/png","base64":"{b64}"}}"#);

    let put = app
        .clone()
        .oneshot(put_json_request(
            &format!(
                "/v1/workspaces/{}/files/workspace/pixel.png",
                space(&store, &user_id, "assistant")
            ),
            &secret,
            &json,
        ))
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::OK);

    let download = app
        .oneshot(
            Request::builder()
                .uri(&format!(
                    "/v1/workspaces/{}/files/workspace/pixel.png",
                    space(&store, &user_id, "assistant")
                ))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    let bytes = hyper::body::to_bytes(download.into_body()).await.unwrap();
    assert!(bytes.starts_with(b"\x89PNG"));
}

#[tokio::test]
async fn workspace_files_put_json_invalid_base64_is_400() {
    let (store, secret, user_id) = member_auth_store();
    let _drive9_root = Drive9TestRoot::new();
    let app = test_app(app_state_http_only(store.clone()));
    let response = app
        .oneshot(put_json_request(
            &format!(
                "/v1/workspaces/{}/files/workspace/x.bin",
                space(&store, &user_id, "assistant")
            ),
            &secret,
            r#"{"base64":"!!!not-base64!!!"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
