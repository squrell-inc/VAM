//! The world-piece format (`e2e/world/<piece>.yaml`).

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PieceKind {
    Org,
    Agent,
    Playbook,
    /// An org secret with a runner-made random value, never printed.
    Secret,
    /// An org-scope OAuth MCP connection made through the public API: `body` is the
    /// `PUT /v1/mcp/connections` request (`connectionId`, `url`, `authMode: oauth`, `oauth`), then
    /// runner-made random access and refresh tokens are upserted. The values are never printed and
    /// the connection is removed at cleanup. `secret-absent` can name the piece (the access token).
    #[serde(rename = "mcp-connection")]
    McpConnection,
    /// A skill package built from `body.spec` (the manifest), published and released.
    Skill,
    /// A test broker process: started on a short socket path and registered with vamd
    /// (`PUT /v1/hosted/brokers`). `body` is the request, with `{{socket}}` for the path.
    Broker,
    /// A workspace that an earlier vamd wrote, copied into the node's Drive9 store before the
    /// scenario starts (`fixture` names the directory under `e2e/world/fixtures`).
    #[serde(rename = "workspace-seed")]
    WorkspaceSeed,
    /// A runner-hosted loopback provider (`provider`: `microsoft-login`, `microsoft-graph` or `linear-server`).
    /// The runner makes its tokens in memory and never prints them.
    #[serde(rename = "mock-provider")]
    MockProvider,
    /// A marketplace repository the runner makes from a fixture directory: commits it, tags it
    /// and serves the result as a bare repo (`file://`), standing in for `url`.
    #[serde(rename = "git-marketplace")]
    GitMarketplace,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorldPiece {
    /// Equals the file stem.
    pub piece: String,
    pub kind: PieceKind,
    #[serde(default)]
    pub requires: Vec<String>,
    /// `org` only: the role the signed-in profile has.
    pub role: Option<String>,
    /// `org` of a member role only: the login name to create the user under, instead of the
    /// run-unique default. `{tag}` is replaced by a short token unique to the piece, scenario and
    /// run (eight lowercase hex digits), so two scenarios on one node never clash.
    pub username: Option<String>,
    /// `agent` and `playbook`: release it after the put.
    #[serde(default)]
    pub release: bool,
    /// `agent` and `playbook`: the PUT body. The URN carries the `{{name}}` placeholder, which
    /// the engine replaces with `<piece>-<scenario>-<runTag>`.
    pub body: Option<serde_json::Value>,
    /// `secret` only: make it an egress secret (kind `egress`) instead of the default
    /// inference one. The proxy swaps the placeholder for the value on requests to the
    /// destinations.
    pub egress: Option<EgressSpec>,
    /// `secret` only: text appended to the piece's run-unique name to make the vault binding id
    /// (the HTTP host adapter reads names ending in `auth-header` or `base-url`). `{{name:<piece>}}`
    /// in a skill is still the name without it.
    pub binding_suffix: Option<String>,
    /// `secret` only: `egress` stores the value as an egress secret (default: inference).
    pub secret_kind: Option<String>,
    /// `secret` only: the hosts an egress secret is pinned to.
    #[serde(default)]
    pub allowed_destinations: Vec<String>,
    /// `agent` only: the urn is fixed (no `{{name}}`), because a seeded workspace is named after
    /// the agent that wrote it. The urn must be unique in the world.
    #[serde(default)]
    pub fixed_urn: bool,
    /// `workspace-seed` only: the fixture directory under `e2e/world/fixtures`.
    pub fixture: Option<String>,
    /// `workspace-seed` only: the id of the seeded workspace, as the node that wrote it named it.
    pub workspace_id: Option<String>,
    /// `workspace-seed` only: the seeded thread's name, as the node that wrote it named it.
    pub thread: Option<String>,
    /// `broker` only: the env var that names the binary (default `VAM_E2E_TEST_BROKER`).
    pub bin: Option<String>,
    /// `broker` only: the process arguments, with `{{socket}}` and `{{dir}}` (default
    /// `--triggers --socket {{socket}}`).
    pub args: Option<Vec<String>>,
    /// `broker` only: after registering, start the process again with `--gateway-socket <path>`
    /// (vamd reports the path only once the broker is registered).
    #[serde(default)]
    pub restart_with_gateway: bool,
    /// `mock-provider` only: `microsoft-login`, `microsoft-graph` or `linear-server`.
    pub provider: Option<String>,
    /// `skill` only: extra files packed into the package, `{ "path in the package": "path under
    /// e2e/world/fixtures" }` (a script a `localExec` tool runs).
    #[serde(default)]
    pub files: std::collections::BTreeMap<String, String>,
    /// `skill` only: arrange the piece's name and URN but do not publish it; a `publish-skill`
    /// step does, so a refusal is a step result (product) and not a harness failure.
    #[serde(default)]
    pub deferred: bool,
    /// `git-marketplace` only: the https URL the node is told about (the seam maps it to the repo).
    pub url: Option<String>,
    /// `git-marketplace` only: tags created on the first commit.
    #[serde(default)]
    pub tags: Vec<String>,
    /// `git-marketplace` only: the node lists it from the start (id `squrell`); nobody adds it.
    #[serde(default)]
    pub preconfigured: bool,
    /// `git-marketplace` only: the `publisherDomain` the admin confirms when adding it.
    pub publisher_domain: Option<String>,
    /// Set by the loader: the absolute fixture directory of a `workspace-seed` or `git-marketplace` piece.
    #[serde(skip)]
    pub fixture_dir: Option<std::path::PathBuf>,
}

/// How an egress `secret` piece is bound. The binding id is fixed, not run-unique, because the
/// guest's placeholder (`vamsec_` plus the id with `/` and `-` as `_`) must be written in a mock
/// script, which cannot name a run-unique id. A runner starts a fresh state directory, so the id
/// does not collide.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EgressSpec {
    pub binding_id: String,
    pub allowed_destinations: Vec<String>,
}

impl WorldPiece {
    /// The URN in `body.spec.resourceId.urn`, when present.
    pub fn urn(&self) -> Option<&str> {
        self.body
            .as_ref()?
            .pointer("/spec/resourceId/urn")?
            .as_str()
    }
}
