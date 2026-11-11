//! Fail-closed admission gate for HSP broker registration and invocation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::net::IpAddr;
use std::str::FromStr;

use prost_types::value::Kind;
use prost_types::{Struct, Value};
use url::Url;
use vam_spec::hosted_adapter_id;
use vam_spec::v1::{
    CapabilityManifest, HookPhase, InvokeRequest, ListToolsRequest, RegisterRequest, ToolSpec,
    TrustTier,
};
use vam_spec::validation::Validate;

use crate::error::OrchestratorError;
use crate::hsp::{HspClient, HspEndpoint};
use crate::hsp_store::{HspBrokerApproval, HspBrokerToolSnapshot};
use crate::ssrf_guard::{is_restricted_ip, resolve_and_classify_host};
use crate::web_admission::{AdmissionEnv, Refusal};
use crate::workflow::SqliteWorkflowStore;

/// Manifest label that marks a web broker (operator-set, part of the approval).
pub const WEB_CAPABILITY_LABEL: &str = "vam.capability";
pub const WEB_CAPABILITY_VALUE: &str = "web";

/// The approved manifest labels this broker a web broker.
pub fn manifest_is_web(manifest: &CapabilityManifest) -> bool {
    manifest
        .labels
        .get(WEB_CAPABILITY_LABEL)
        .map(String::as_str)
        == Some(WEB_CAPABILITY_VALUE)
}

/// Logical names of bindings that no tool export lists in `required_secrets`.
/// Such a binding is never sent to any tool (least privilege), which is
/// almost always a manifest written before exports declared their secrets.
pub fn unlisted_secret_bindings(manifest: &CapabilityManifest) -> Vec<String> {
    manifest
        .secret_bindings
        .iter()
        .map(|binding| binding.logical_name.clone())
        .filter(|name| {
            !manifest
                .tools
                .iter()
                .any(|tool| tool.required_secrets.contains(name))
        })
        .collect()
}

/// Why a preflight refused the call.
#[derive(Debug)]
pub enum PreflightError {
    /// Admission, trust tier or the allowlist.
    Denied(OrchestratorError),
    /// A typed web refusal the model can act on.
    Refused(Refusal),
}

impl From<OrchestratorError> for PreflightError {
    fn from(err: OrchestratorError) -> Self {
        Self::Denied(err)
    }
}

impl From<Refusal> for PreflightError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl PreflightError {
    /// Flatten to the error type of the older entry points.
    pub fn into_orchestrator(self) -> OrchestratorError {
        match self {
            Self::Denied(err) => err,
            Self::Refused(r) => OrchestratorError::Hsp(format!("{}: {}", r.code, r.advice)),
        }
    }
}

/// Host-side registry that enforces operator-approved capability manifests.
pub struct HspRegistry<'a> {
    store: &'a SqliteWorkflowStore,
}

impl<'a> HspRegistry<'a> {
    pub fn new(store: &'a SqliteWorkflowStore) -> Self {
        Self { store }
    }

    /// Fail-closed gate before dialing a broker for `Register`.
    pub fn admit_register(&self, request: &RegisterRequest) -> Result<(), OrchestratorError> {
        request
            .validate()
            .map_err(|error| OrchestratorError::InvalidContract(error.to_string()))?;

        let approval = self.require_enabled_approval(&request.broker_id)?;
        let presented = request.manifest.as_ref().ok_or_else(|| {
            OrchestratorError::InvalidContract("RegisterRequest has no manifest".to_string())
        })?;

        if presented.broker_id != approval.manifest.broker_id {
            return Err(deny_drifted());
        }

        assert_unbound_secrets(presented)?;
        manifest_is_subset(presented, &approval.manifest)
    }

    /// Fail-closed gate before dialing a broker for `Invoke`.
    pub fn admit_invoke(
        &self,
        broker_id: &str,
        request: &InvokeRequest,
    ) -> Result<(), OrchestratorError> {
        let approval = self.require_enabled_approval(broker_id)?;

        let approved_tools: HashSet<&str> = approval
            .manifest
            .tools
            .iter()
            .map(|tool| tool.tool_name.as_str())
            .collect();
        if !approved_tools.contains(request.tool_name.as_str()) {
            return Err(deny_undeclared_tool());
        }

        // Least privilege: an invoke carries only the secrets its export
        // lists, and each of them must be an approved binding.
        let approved_secrets: HashSet<&str> = approval
            .manifest
            .secret_bindings
            .iter()
            .map(|binding| binding.logical_name.as_str())
            .collect();
        let exported: HashSet<&str> = approval
            .manifest
            .tools
            .iter()
            .filter(|tool| tool.tool_name == request.tool_name)
            .flat_map(|tool| tool.required_secrets.iter().map(String::as_str))
            .collect();
        for key in request.resolved_secrets.keys() {
            if !approved_secrets.contains(key.as_str()) || !exported.contains(key.as_str()) {
                return Err(deny_extra_secret());
            }
        }

        crate::caller_context::assert_identity_satisfied(
            approval.manifest.identity_requirement.as_ref(),
            request.caller_context.as_ref(),
        )?;

        Ok(())
    }

    /// Admit then forward `Register` over transport and refresh the tool catalog snapshot.
    pub async fn register_admitted(
        &self,
        client: &mut HspClient,
        request: RegisterRequest,
    ) -> Result<vam_spec::v1::RegisterResponse, OrchestratorError> {
        self.admit_register(&request)?;
        let approval = self.require_enabled_approval(&request.broker_id)?;
        assert_trust_transport(approval.manifest.trust_tier, client.endpoint())?;
        let broker_id = request.broker_id.clone();
        let response = client.register(request).await?;
        if response.error.is_none() {
            self.store
                .set_hsp_broker_endpoint(&broker_id, client.endpoint())?;
            self.list_tools_admitted(client, &broker_id).await?;
        }
        Ok(response)
    }

    /// Dial `ListTools`, intersect with approved exports, rewrite adapter ids, and persist.
    pub async fn list_tools_admitted(
        &self,
        client: &mut HspClient,
        broker_id: &str,
    ) -> Result<Vec<HspBrokerToolSnapshot>, OrchestratorError> {
        let approval = self.require_enabled_approval(broker_id)?;
        let approved_exports: HashMap<&str, &vam_spec::v1::ToolExport> = approval
            .manifest
            .tools
            .iter()
            .map(|tool| (tool.tool_name.as_str(), tool))
            .collect();

        let response = client
            .list_tools(ListToolsRequest {
                broker_id: broker_id.to_string(),
            })
            .await?;

        for spec in &response.tools {
            if !approved_exports.contains_key(spec.name.as_str()) {
                return Err(deny_undeclared_tool());
            }
        }

        let snapshots = response
            .tools
            .into_iter()
            .map(|spec| snapshot_from_list_tools(broker_id, spec, &approved_exports))
            .collect::<Result<Vec<_>, OrchestratorError>>()?;

        self.store.replace_hsp_broker_tools(broker_id, &snapshots)?;
        Ok(snapshots)
    }

    /// Preflight admit + trust-tier + destination before connecting or invoking.
    ///
    /// The destination check depends on the tool (AR-1.4):
    /// - `PUBLIC_WEB`: the full open-web admission ([`crate::web_admission`]);
    ///   there is no way to skip it. Other string arguments follow the query
    ///   rule.
    /// - `ALLOWLIST` on a web-labelled broker: arguments are search text, not
    ///   destinations. The query rule applies instead of the allowlist.
    /// - `ALLOWLIST` elsewhere: URLs in args must match `egress_domains`.
    ///
    /// A web-labelled broker is refused (`web_disabled`) unless the session's
    /// agent has `permissions.web.enabled`.
    pub async fn preflight_invoke(
        &self,
        broker_id: &str,
        endpoint: &HspEndpoint,
        request: &InvokeRequest,
        env: &AdmissionEnv,
    ) -> Result<(), PreflightError> {
        self.admit_invoke(broker_id, request)?;
        let approval = self.require_enabled_approval(broker_id)?;
        assert_trust_transport(approval.manifest.trust_tier, endpoint)?;
        let manifest = &approval.manifest;
        let is_web = manifest_is_web(manifest);
        if is_web
            && !crate::web_admission::session_web_enabled(
                self.store,
                &env.session_id,
                Some(env.agent_urn.as_str()),
            )
        {
            return Err(crate::web_admission::web_disabled().into());
        }
        let public_web = manifest
            .tools
            .iter()
            .find(|tool| tool.tool_name == request.tool_name)
            .is_some_and(|tool| {
                tool.destination_policy == vam_spec::v1::ToolDestination::PublicWeb as i32
            });
        if public_web {
            crate::web_admission::check_query_args(
                request.args.as_ref(),
                Some(crate::web_admission::DESTINATION_ARG),
            )?;
            crate::web_admission::admit_public_web(
                self.store,
                env,
                &request.run_id,
                request.args.as_ref(),
            )
            .await?;
        } else if is_web {
            crate::web_admission::check_query_args(request.args.as_ref(), None)?;
        } else {
            assert_invoke_egress_for_tool(manifest, &request.tool_name, request.args.as_ref())?;
        }
        Ok(())
    }

    /// Admit then forward `Invoke` over transport.
    ///
    /// Carries no session, so a web-labelled broker is refused here; the live
    /// path is `invoke_hosted_tool`, which passes the session.
    pub async fn invoke_admitted(
        &self,
        broker_id: &str,
        client: &mut HspClient,
        request: InvokeRequest,
    ) -> Result<vam_spec::v1::InvokeResponse, OrchestratorError> {
        self.preflight_invoke(
            broker_id,
            client.endpoint(),
            &request,
            &AdmissionEnv::none(),
        )
        .await
        .map_err(PreflightError::into_orchestrator)?;
        client.invoke(request).await
    }

    /// Fail-closed gate before dialing a broker for a lifecycle hook RPC.
    ///
    /// Requires an enabled approval, a stored endpoint, trust-tier transport match,
    /// and manifest `hooks[]` participation for `phase`. Does not schedule waves —
    /// that is the dispatcher.
    pub fn admit_hook(
        &self,
        broker_id: &str,
        phase: HookPhase,
        caller: Option<&vam_spec::v1::CallerContext>,
    ) -> Result<(), OrchestratorError> {
        if phase == HookPhase::Unspecified {
            return Err(deny_undeclared_hook());
        }
        let approval = self.require_enabled_approval(broker_id)?;
        let endpoint = self
            .store
            .get_hsp_broker_endpoint(broker_id)?
            .ok_or_else(|| OrchestratorError::Hsp("hsp denied: no endpoint".into()))?;
        assert_trust_transport(approval.manifest.trust_tier, &endpoint)?;
        crate::caller_context::assert_identity_satisfied(
            approval.manifest.identity_requirement.as_ref(),
            caller,
        )?;
        let phase_i32 = phase as i32;
        let participates = approval
            .manifest
            .hooks
            .iter()
            .any(|hook| hook.phase == phase_i32);
        if !participates {
            return Err(deny_undeclared_hook());
        }
        Ok(())
    }

    fn require_enabled_approval(
        &self,
        broker_id: &str,
    ) -> Result<HspBrokerApproval, OrchestratorError> {
        match self.store.get_hsp_broker_approval(broker_id)? {
            Some(approval) if approval.enabled => Ok(approval),
            _ => Err(deny_unapproved_broker()),
        }
    }
}

/// Enforce TrustTier ↔ HSP transport pairing (fail-closed; no vsock connector yet).
pub fn assert_trust_transport(tier: i32, endpoint: &HspEndpoint) -> Result<(), OrchestratorError> {
    match TrustTier::try_from(tier) {
        Ok(TrustTier::Unspecified) => Err(OrchestratorError::Hsp(
            "hsp denied: unspecified trust tier".into(),
        )),
        Ok(TrustTier::Core) => Err(OrchestratorError::Hsp(
            "hsp denied: core tier is in-process".into(),
        )),
        Ok(TrustTier::FirstParty) | Ok(TrustTier::Operator) => match endpoint {
            HspEndpoint::Unix(_) | HspEndpoint::Tcp(_) => Ok(()),
        },
        Ok(TrustTier::ThirdParty) => {
            Err(OrchestratorError::Hsp("hsp denied: vsock required".into()))
        }
        Err(_) => Err(OrchestratorError::Hsp(
            "hsp denied: unknown trust tier".into(),
        )),
    }
}

fn assert_unbound_secrets(manifest: &CapabilityManifest) -> Result<(), OrchestratorError> {
    for binding in &manifest.secret_bindings {
        if binding.required && binding.vault_key.trim().is_empty() {
            return Err(OrchestratorError::Hsp("hsp denied: unbound secret".into()));
        }
    }
    Ok(())
}

/// Egress scan for one tool call. The top-level `run` object of a call to a tool in the
/// approved manifest's `run_request_permission.grant_tools` is a grant request, not egress:
/// `grants::check_call` decodes and validates it before any grant is minted, the broker never
/// fetches it, and the future run applies its own egress. Every other key and tool is scanned.
fn assert_invoke_egress_for_tool(
    manifest: &CapabilityManifest,
    tool_name: &str,
    args: Option<&Struct>,
) -> Result<(), OrchestratorError> {
    let is_grant_tool = manifest
        .run_request_permission
        .as_ref()
        .is_some_and(|p| p.grant_tools.iter().any(|t| t == tool_name));
    match args {
        Some(s) if is_grant_tool && s.fields.contains_key("run") => {
            let mut scanned = s.clone();
            scanned.fields.remove("run");
            assert_invoke_egress(manifest, Some(&scanned))
        }
        _ => assert_invoke_egress(manifest, args),
    }
}

/// Scan invoke args for http(s) URLs and enforce egress allowlist + SSRF.
pub fn assert_invoke_egress(
    manifest: &CapabilityManifest,
    args: Option<&Struct>,
) -> Result<(), OrchestratorError> {
    let urls = collect_http_urls(args);
    if urls.is_empty() {
        return Ok(());
    }
    if manifest.egress_domains.is_empty() {
        return Err(OrchestratorError::Hsp(
            "hsp denied: egress not allowlisted".into(),
        ));
    }
    for url_str in urls {
        let url = Url::parse(&url_str)
            .map_err(|_| OrchestratorError::Hsp(format!("hsp denied: extra domain: {url_str}")))?;
        let host = url.host_str().ok_or_else(|| {
            OrchestratorError::Hsp(format!("hsp denied: extra domain: {url_str}"))
        })?;
        if !egress_host_allowed(host, &manifest.egress_domains) {
            return Err(OrchestratorError::Hsp(format!(
                "hsp denied: extra domain: {host}"
            )));
        }
        assert_host_not_ssrf_restricted(host)?;
    }
    Ok(())
}

fn egress_host_allowed(host: &str, domains: &[String]) -> bool {
    let host = host.to_lowercase();
    domains.iter().any(|domain| {
        let domain = domain.to_lowercase();
        host == domain || host.ends_with(&format!(".{domain}"))
    })
}

fn assert_host_not_ssrf_restricted(host: &str) -> Result<(), OrchestratorError> {
    if let Ok(ip) = IpAddr::from_str(host) {
        if is_restricted_ip(ip) {
            return Err(OrchestratorError::Hsp(format!(
                "hsp denied: ssrf restricted: {host}"
            )));
        }
        return Ok(());
    }
    match resolve_and_classify_host(host, 443) {
        Ok(_) => Ok(()),
        Err(err)
            if err.contains("restricted") || err.contains("SSRF") || err.contains("Restricted") =>
        {
            Err(OrchestratorError::Hsp(format!(
                "hsp denied: ssrf restricted: {host}"
            )))
        }
        // Hermetic: allowlisted hostname with DNS unavailable still passes allowlist.
        Err(_) => Ok(()),
    }
}

fn collect_http_urls(args: Option<&Struct>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(s) = args {
        collect_http_urls_from_struct(s, &mut out);
    }
    out
}

fn collect_http_urls_from_struct(s: &Struct, out: &mut Vec<String>) {
    for value in s.fields.values() {
        collect_http_urls_from_value(value, out);
    }
}

fn collect_http_urls_from_value(value: &Value, out: &mut Vec<String>) {
    match value.kind.as_ref() {
        Some(Kind::StringValue(text)) => {
            for candidate in extract_http_url_candidates(text) {
                out.push(candidate);
            }
        }
        Some(Kind::StructValue(nested)) => collect_http_urls_from_struct(nested, out),
        Some(Kind::ListValue(list)) => {
            for item in &list.values {
                collect_http_urls_from_value(item, out);
            }
        }
        _ => {}
    }
}

fn extract_http_url_candidates(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for prefix in ["https://", "http://"] {
        let mut rest = text;
        while let Some(idx) = rest.find(prefix) {
            let start = &rest[idx..];
            let end = start
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '>')
                .unwrap_or(start.len());
            let candidate = start[..end].trim_end_matches(['.', ',', ';', ')', ']']);
            if Url::parse(candidate).is_ok() {
                found.push(candidate.to_string());
            }
            rest = &start[prefix.len()..];
        }
    }
    found
}

/// A presented `notification_channel` must equal the approved channel and surface and may not
/// raise the action rate. Absent approved means nothing may be presented.
fn notification_channel_is_subset(
    presented: Option<&vam_spec::v1::NotificationChannel>,
    approved: Option<&vam_spec::v1::NotificationChannel>,
) -> bool {
    const DEFAULT_ACTIONS_PER_MINUTE: i32 = 60;
    let (presented, approved) = match (presented, approved) {
        (None, _) => return true,
        (Some(_), None) => return false,
        (Some(p), Some(a)) => (p, a),
    };
    let rate = |c: &vam_spec::v1::NotificationChannel| {
        if c.max_actions_per_minute == 0 {
            DEFAULT_ACTIONS_PER_MINUTE
        } else {
            c.max_actions_per_minute
        }
    };
    presented.channel == approved.channel
        && presented.surface == approved.surface
        && rate(presented) <= rate(approved)
}

/// A presented `run_request_permission` may only narrow the approved one.
/// Absent approved means nothing may be presented. Limits compare after the
/// node defaults replace 0.
fn run_request_permission_is_subset(
    presented: Option<&vam_spec::v1::RunRequestPermission>,
    approved: Option<&vam_spec::v1::RunRequestPermission>,
) -> bool {
    let (presented, approved) = match (presented, approved) {
        (None, _) => return true,
        (Some(_), None) => return false,
        (Some(p), Some(a)) => (p, a),
    };
    if presented.surface != approved.surface {
        return false;
    }
    if !presented
        .grant_tools
        .iter()
        .all(|tool| approved.grant_tools.contains(tool))
    {
        return false;
    }
    // Dropping a loop guard would widen what a triggered run may do.
    if !approved
        .loop_guarded_tools
        .iter()
        .all(|tool| presented.loop_guarded_tools.contains(tool))
    {
        return false;
    }
    let (p_min, p_day, p_active) = vam_spec::validation::effective_run_request_limits(presented);
    let (a_min, a_day, a_active) = vam_spec::validation::effective_run_request_limits(approved);
    p_min <= a_min && p_day <= a_day && p_active <= a_active
}

fn manifest_is_subset(
    presented: &CapabilityManifest,
    approved: &CapabilityManifest,
) -> Result<(), OrchestratorError> {
    if presented.trust_tier != approved.trust_tier {
        return Err(deny_drifted());
    }

    if relate_approved_set(
        &presented.tools,
        &approved.tools,
        |tool| tool.tool_name.clone(),
        // Narrowing to ALLOWLIST is fine; widening to PUBLIC_WEB is drift.
        |presented, approved| {
            presented.destination_policy == vam_spec::v1::ToolDestination::Allowlist as i32
                || presented.destination_policy == approved.destination_policy
        },
    )
    .is_drifted()
    {
        return Err(deny_drifted());
    }

    if relate_approved_set(
        &presented.secret_bindings,
        &approved.secret_bindings,
        |binding| binding.logical_name.clone(),
        |_, _| true,
    )
    .is_drifted()
    {
        return Err(deny_drifted());
    }

    if relate_approved_set(
        &presented.egress_domains,
        &approved.egress_domains,
        |domain| domain.clone(),
        |_, _| true,
    )
    .is_drifted()
    {
        return Err(deny_extra_domain());
    }

    if relate_approved_set(
        &presented.hooks,
        &approved.hooks,
        |hook| hook.phase.to_string(),
        |_, _| true,
    )
    .is_drifted()
    {
        return Err(deny_drifted());
    }

    if !run_request_permission_is_subset(
        presented.run_request_permission.as_ref(),
        approved.run_request_permission.as_ref(),
    ) {
        return Err(deny_drifted());
    }

    if !notification_channel_is_subset(
        presented.notification_channel.as_ref(),
        approved.notification_channel.as_ref(),
    ) {
        return Err(deny_drifted());
    }

    if !crate::caller_context::identity_requirement_is_subset(
        presented.identity_requirement.as_ref(),
        approved.identity_requirement.as_ref(),
    ) {
        return Err(deny_drifted());
    }

    Ok(())
}

/// How a presented list relates to the list that was approved.
///
/// This is the one subset check for both HSP admission and MCP refresh.
/// `same` decides whether a shared key still matches the pin. HSP passes
/// always-true (names only). MCP passes description and schema equality, so
/// a changed description is drift rather than a successful subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovedSetRelation {
    Identical,
    StrictSubset {
        removed: Vec<String>,
    },
    Drifted {
        added: Vec<String>,
        changed: Vec<String>,
    },
}

impl ApprovedSetRelation {
    pub fn is_drifted(&self) -> bool {
        matches!(self, Self::Drifted { .. })
    }
}

pub fn relate_approved_set<T>(
    presented: &[T],
    approved: &[T],
    key_of: impl Fn(&T) -> String,
    same: impl Fn(&T, &T) -> bool,
) -> ApprovedSetRelation {
    let mut approved_by_key: BTreeMap<String, &T> = BTreeMap::new();
    for item in approved {
        approved_by_key.entry(key_of(item)).or_insert(item);
    }

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut added = BTreeSet::new();
    let mut changed = BTreeSet::new();
    for item in presented {
        let key = key_of(item);
        match approved_by_key.get(&key) {
            None => {
                added.insert(key);
            }
            Some(pinned) => {
                if !same(item, pinned) {
                    changed.insert(key.clone());
                }
                seen.insert(key);
            }
        }
    }

    if !added.is_empty() || !changed.is_empty() {
        return ApprovedSetRelation::Drifted {
            added: added.into_iter().collect(),
            changed: changed.into_iter().collect(),
        };
    }

    let removed: Vec<String> = approved_by_key
        .keys()
        .filter(|key| !seen.contains(*key))
        .map(|key| (*key).clone())
        .collect();
    if removed.is_empty() {
        ApprovedSetRelation::Identical
    } else {
        ApprovedSetRelation::StrictSubset { removed }
    }
}

fn deny_unapproved_broker() -> OrchestratorError {
    OrchestratorError::Hsp("hsp denied: unapproved broker".into())
}

fn deny_undeclared_tool() -> OrchestratorError {
    OrchestratorError::Hsp("hsp denied: undeclared tool".into())
}

fn deny_extra_secret() -> OrchestratorError {
    OrchestratorError::Hsp("hsp denied: extra secret".into())
}

fn deny_extra_domain() -> OrchestratorError {
    OrchestratorError::Hsp("hsp denied: extra domain".into())
}

fn deny_drifted() -> OrchestratorError {
    OrchestratorError::Hsp("hsp denied: drifted manifest".into())
}

fn deny_undeclared_hook() -> OrchestratorError {
    OrchestratorError::Hsp("hsp denied: undeclared hook".into())
}

fn snapshot_from_list_tools(
    broker_id: &str,
    mut spec: ToolSpec,
    approved_exports: &HashMap<&str, &vam_spec::v1::ToolExport>,
) -> Result<HspBrokerToolSnapshot, OrchestratorError> {
    let export = approved_exports
        .get(spec.name.as_str())
        .copied()
        .ok_or_else(deny_undeclared_tool)?;
    let adapter_id = hosted_adapter_id(broker_id, &spec.name);
    if let Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(host)) =
        spec.execution_provider.as_mut()
    {
        host.adapter_id = adapter_id.clone();
    }
    Ok(HspBrokerToolSnapshot {
        tool_name: spec.name,
        adapter_id,
        side_effect: export.side_effect,
        description: spec.description,
        input_schema_json: if spec.input_schema_json.is_empty() {
            "{}".to_string()
        } else {
            spec.input_schema_json
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsp::HspEndpoint;
    use vam_spec::v1::{HookParticipation, IdentityRequirement, ManifestSecretBinding, ToolExport};

    fn approved_manifest() -> CapabilityManifest {
        CapabilityManifest {
            broker_id: "echo".to_string(),
            trust_tier: 2,
            secret_bindings: vec![ManifestSecretBinding {
                logical_name: "token".to_string(),
                vault_key: "vault/echo/token".to_string(),
                required: false,
            }],
            egress_domains: vec!["api.example.com".to_string()],
            hooks: vec![HookParticipation {
                phase: 1,
                required: false,
            }],
            tools: vec![
                ToolExport {
                    tool_name: "echo.ping".to_string(),
                    side_effect: 0,
                    required_secrets: vec![],
                    destination_policy: Default::default(),
                    effect: None,
                },
                ToolExport {
                    tool_name: "echo.status".to_string(),
                    side_effect: 0,
                    required_secrets: vec![],
                    destination_policy: Default::default(),
                    effect: None,
                },
            ],
            depends_on: vec![],
            labels: Default::default(),
            identity_requirement: None,
            run_request_permission: None,
            notification_channel: None,
        }
    }

    fn register_with(manifest: CapabilityManifest) -> RegisterRequest {
        RegisterRequest {
            broker_id: manifest.broker_id.clone(),
            broker_version: "1.0.0".to_string(),
            manifest: Some(manifest),
        }
    }

    #[test]
    fn assert_trust_transport_matrix() {
        let unix = HspEndpoint::Unix("/tmp/echo.sock".into());
        assert!(assert_trust_transport(2, &unix).is_ok());
        assert!(assert_trust_transport(3, &unix).is_ok());
        assert!(matches!(
            assert_trust_transport(0, &unix),
            Err(OrchestratorError::Hsp(ref msg)) if msg.contains("unspecified trust tier")
        ));
        assert!(matches!(
            assert_trust_transport(1, &unix),
            Err(OrchestratorError::Hsp(ref msg)) if msg.contains("core tier is in-process")
        ));
        assert!(matches!(
            assert_trust_transport(4, &unix),
            Err(OrchestratorError::Hsp(ref msg)) if msg.contains("vsock required")
        ));
        assert!(matches!(
            assert_trust_transport(99, &unix),
            Err(OrchestratorError::Hsp(ref msg)) if msg.contains("unknown trust tier")
        ));
    }

    #[test]
    fn admit_register_rejects_unapproved_broker() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let registry = HspRegistry::new(&store);
        let request = register_with(approved_manifest());
        let err = registry.admit_register(&request).unwrap_err();
        assert!(
            matches!(err, OrchestratorError::Hsp(ref msg) if msg.contains("unapproved broker"))
        );
    }

    #[test]
    fn admit_register_accepts_matching_subset() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let approved = approved_manifest();
        store.upsert_hsp_broker_approval(&approved).unwrap();

        let presented = CapabilityManifest {
            tools: vec![ToolExport {
                tool_name: "echo.ping".to_string(),
                side_effect: 0,
                required_secrets: vec![],
                destination_policy: Default::default(),
                effect: None,
            }],
            ..approved
        };
        let registry = HspRegistry::new(&store);
        registry.admit_register(&register_with(presented)).unwrap();
    }

    #[test]
    fn admit_register_rejects_extra_tool() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let approved = approved_manifest();
        store.upsert_hsp_broker_approval(&approved).unwrap();

        let mut presented = approved.clone();
        presented.tools.push(ToolExport {
            tool_name: "echo.extra".to_string(),
            side_effect: 0,
            required_secrets: vec![],
            destination_policy: Default::default(),
            effect: None,
        });
        let registry = HspRegistry::new(&store);
        let err = registry
            .admit_register(&register_with(presented))
            .unwrap_err();
        assert!(matches!(err, OrchestratorError::Hsp(ref msg) if msg.contains("drifted manifest")));
    }

    #[test]
    fn admit_register_rejects_widening_to_public_web() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let mut approved = approved_manifest();
        // PUBLIC_WEB is only valid on READ_ONLY exports.
        approved.tools[0].side_effect = vam_spec::v1::SideEffectClass::ReadOnly as i32;
        store.upsert_hsp_broker_approval(&approved).unwrap();

        let mut presented = approved.clone();
        presented.tools[0].destination_policy = vam_spec::v1::ToolDestination::PublicWeb as i32;
        let registry = HspRegistry::new(&store);
        let err = registry
            .admit_register(&register_with(presented))
            .unwrap_err();
        assert!(matches!(err, OrchestratorError::Hsp(ref msg) if msg.contains("drifted manifest")));
    }

    #[test]
    fn admit_register_accepts_approved_and_narrowed_public_web() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let mut approved = approved_manifest();
        approved.tools[0].side_effect = vam_spec::v1::SideEffectClass::ReadOnly as i32;
        approved.tools[0].destination_policy = vam_spec::v1::ToolDestination::PublicWeb as i32;
        store.upsert_hsp_broker_approval(&approved).unwrap();
        let registry = HspRegistry::new(&store);

        // Same policy as approved.
        registry
            .admit_register(&register_with(approved.clone()))
            .unwrap();

        // Narrowed back to the allowlist.
        let mut narrowed = approved;
        narrowed.tools[0].destination_policy = vam_spec::v1::ToolDestination::Allowlist as i32;
        registry.admit_register(&register_with(narrowed)).unwrap();
    }

    #[test]
    fn admit_invoke_rejects_extra_secret() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .upsert_hsp_broker_approval(&approved_manifest())
            .unwrap();
        let registry = HspRegistry::new(&store);
        let request = InvokeRequest {
            session_id: "sess".to_string(),
            run_id: "run".to_string(),
            tool_name: "echo.ping".to_string(),
            args: None,
            resolved_secrets: [("unknown".to_string(), "value".to_string())].into(),
            context: None,
            trace_context: None,
            caller_context: None,
            run_grant: None,
            idempotency_key: String::new(),
        };
        let err = registry.admit_invoke("echo", &request).unwrap_err();
        assert!(matches!(err, OrchestratorError::Hsp(ref msg) if msg.contains("extra secret")));
    }

    #[tokio::test]
    async fn register_admitted_does_not_dial_on_deny() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let registry = HspRegistry::new(&store);
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("unused.sock");
        let mut client = HspClient::new(HspEndpoint::Unix(sock));
        let request = register_with(approved_manifest());
        let err = registry
            .register_admitted(&mut client, request)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            OrchestratorError::Hsp(ref msg) if msg.contains("unapproved broker")
        ));
    }

    #[test]
    fn unlisted_bindings_are_the_ones_no_export_names() {
        let mut manifest = approved_manifest();
        assert_eq!(
            unlisted_secret_bindings(&manifest),
            vec!["token".to_string()]
        );
        manifest.tools[0].required_secrets = vec!["token".to_string()];
        assert!(unlisted_secret_bindings(&manifest).is_empty());
    }

    #[test]
    fn admit_invoke_rejects_a_secret_the_export_does_not_list() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let mut manifest = approved_manifest();
        let tool = manifest.tools[0].tool_name.clone();
        store.upsert_hsp_broker_approval(&manifest).unwrap();
        let registry = HspRegistry::new(&store);
        let mut request = InvokeRequest {
            tool_name: tool.clone(),
            ..Default::default()
        };
        request
            .resolved_secrets
            .insert("token".to_string(), "v".to_string());
        // Bound, but the export lists no secrets: refused.
        let err = registry.admit_invoke("echo", &request).unwrap_err();
        assert!(format!("{err}").contains("extra secret"), "{err}");
        // Listed on the export: admitted.
        manifest.tools[0].required_secrets = vec!["token".to_string()];
        store.upsert_hsp_broker_approval(&manifest).unwrap();
        assert!(registry.admit_invoke("echo", &request).is_ok());
    }

    #[test]
    fn invoke_egress_rejects_off_allowlist_url() {
        let manifest = approved_manifest();
        let args = Struct {
            fields: [(
                "url".to_string(),
                Value {
                    kind: Some(Kind::StringValue("https://evil.example.com/x".into())),
                },
            )]
            .into(),
        };
        let err = assert_invoke_egress(&manifest, Some(&args)).unwrap_err();
        assert!(matches!(
            err,
            OrchestratorError::Hsp(ref msg) if msg.starts_with("hsp denied: extra domain:")
        ));
    }

    fn str_val(s: &str) -> Value {
        Value {
            kind: Some(Kind::StringValue(s.into())),
        }
    }

    fn run_args(title: Option<&str>) -> Struct {
        let run = Struct {
            fields: [(
                "message".to_string(),
                str_val("check https://status.example.com"),
            )]
            .into(),
        };
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "run".to_string(),
            Value {
                kind: Some(Kind::StructValue(run)),
            },
        );
        if let Some(t) = title {
            fields.insert("title".to_string(), str_val(t));
        }
        Struct {
            fields: fields.into_iter().collect(),
        }
    }

    fn grant_manifest() -> CapabilityManifest {
        let mut m = approved_manifest();
        m.egress_domains = vec![];
        m.run_request_permission = Some(permission());
        m
    }

    #[test]
    fn grant_tool_run_object_is_not_egress() {
        let m = grant_manifest();
        let args = run_args(None);
        assert!(assert_invoke_egress_for_tool(&m, "echo.ping", Some(&args)).is_ok());
        let err = assert_invoke_egress_for_tool(
            &m,
            "echo.ping",
            Some(&run_args(Some("see https://status.example.com"))),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("egress not allowlisted"), "{err}");
    }

    #[test]
    fn run_object_on_non_grant_tool_or_without_permission_is_scanned() {
        let m = grant_manifest();
        let args = run_args(None);
        assert!(assert_invoke_egress_for_tool(&m, "echo.other", Some(&args)).is_err());
        let mut plain = approved_manifest();
        plain.egress_domains = vec![];
        assert!(assert_invoke_egress_for_tool(&plain, "echo.ping", Some(&args)).is_err());
    }

    #[test]
    fn invoke_egress_rejects_loopback_url() {
        let mut manifest = approved_manifest();
        manifest.egress_domains = vec!["127.0.0.1".to_string()];
        let args = Struct {
            fields: [(
                "url".to_string(),
                Value {
                    kind: Some(Kind::StringValue("http://127.0.0.1/secret".into())),
                },
            )]
            .into(),
        };
        let err = assert_invoke_egress(&manifest, Some(&args)).unwrap_err();
        assert!(matches!(
            err,
            OrchestratorError::Hsp(ref msg) if msg.starts_with("hsp denied: ssrf restricted:")
        ));
    }

    #[test]
    fn identity_requirement_enforced_at_broker_admission() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let approved = approved_manifest();
        store.upsert_hsp_broker_approval(&approved).unwrap();
        let mut presented = approved.clone();
        presented.identity_requirement = Some(IdentityRequirement {
            tenant_id: false,
            user_id: true,
            agent: false,
            workspace_handle: false,
            thread_handle: false,
        });
        let registry = HspRegistry::new(&store);
        let err = registry
            .admit_register(&register_with(presented))
            .unwrap_err();
        assert!(
            matches!(err, OrchestratorError::Hsp(ref msg) if msg.contains("drifted manifest")),
            "{err:?}"
        );

        let mut approved_with_identity = approved_manifest();
        approved_with_identity.identity_requirement = Some(IdentityRequirement {
            tenant_id: false,
            user_id: true,
            agent: true,
            workspace_handle: false,
            thread_handle: false,
        });
        store
            .upsert_hsp_broker_approval(&approved_with_identity)
            .unwrap();
        let presented_ok = CapabilityManifest {
            identity_requirement: Some(IdentityRequirement {
                tenant_id: false,
                user_id: true,
                agent: false,
                workspace_handle: false,
                thread_handle: false,
            }),
            ..approved_with_identity.clone()
        };
        HspRegistry::new(&store)
            .admit_register(&register_with(presented_ok))
            .unwrap();
    }

    fn permission() -> vam_spec::v1::RunRequestPermission {
        vam_spec::v1::RunRequestPermission {
            grant_tools: vec!["echo.ping".to_string(), "echo.status".to_string()],
            loop_guarded_tools: vec!["echo.ping".to_string(), "echo.status".to_string()],
            surface: "schedule".to_string(),
            max_requests_per_minute: 30,
            ..Default::default()
        }
    }

    #[test]
    fn notification_channel_subset_gate() {
        let ch = |channel: &str, surface: &str, rate: i32| vam_spec::v1::NotificationChannel {
            channel: channel.into(),
            surface: surface.into(),
            max_actions_per_minute: rate,
        };
        let a = ch("slack", "slack", 30);
        assert!(notification_channel_is_subset(None, None));
        assert!(notification_channel_is_subset(None, Some(&a)));
        assert!(!notification_channel_is_subset(Some(&a), None));
        assert!(notification_channel_is_subset(Some(&a), Some(&a)));
        assert!(notification_channel_is_subset(
            Some(&ch("slack", "slack", 10)),
            Some(&a)
        ));
        assert!(!notification_channel_is_subset(
            Some(&ch("slack", "slack", 31)),
            Some(&a)
        ));
        assert!(!notification_channel_is_subset(
            Some(&ch("slack", "slack", 0)),
            Some(&a)
        ));
        assert!(!notification_channel_is_subset(
            Some(&ch("slack", "other", 30)),
            Some(&a)
        ));
        assert!(!notification_channel_is_subset(
            Some(&ch("email", "slack", 30)),
            Some(&a)
        ));
    }

    #[test]
    fn run_request_permission_subset_gate() {
        let approved = Some(permission());
        let ok = |p: Option<vam_spec::v1::RunRequestPermission>| {
            run_request_permission_is_subset(p.as_ref(), approved.as_ref())
        };
        assert!(ok(None));
        assert!(ok(Some(permission())));

        let mut p = permission();
        p.grant_tools = vec!["echo.ping".to_string()];
        assert!(ok(Some(p)));

        let mut p = permission();
        p.grant_tools.push("echo.other".to_string());
        assert!(!ok(Some(p)));

        let mut p = permission();
        p.loop_guarded_tools = vec!["echo.ping".to_string()];
        assert!(!ok(Some(p)));

        let mut p = permission();
        p.surface = "other".to_string();
        assert!(!ok(Some(p)));

        // 0 is the node default (60), which is above the approved 30.
        let mut p = permission();
        p.max_requests_per_minute = 0;
        assert!(!ok(Some(p)));
        let mut p = permission();
        p.max_requests_per_minute = 10;
        assert!(ok(Some(p)));

        // Approved 0 equals the default, so an explicit 60 is not wider.
        let mut a = permission();
        a.max_requests_per_minute = 0;
        let mut p = permission();
        p.max_requests_per_minute = 60;
        assert!(run_request_permission_is_subset(Some(&p), Some(&a)));
        p.max_requests_per_minute = 61;
        assert!(!run_request_permission_is_subset(Some(&p), Some(&a)));

        // Presented without an approved declaration is drift.
        assert!(!run_request_permission_is_subset(Some(&permission()), None));
    }

    #[test]
    fn admit_register_applies_the_run_request_gate() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let mut approved = approved_manifest();
        let mut narrow = permission();
        narrow.grant_tools = vec!["echo.ping".to_string()];
        approved.run_request_permission = Some(narrow);
        store.upsert_hsp_broker_approval(&approved).unwrap();
        let registry = HspRegistry::new(&store);
        assert!(registry
            .admit_register(&register_with(approved.clone()))
            .is_ok());
        let mut widened = approved.clone();
        if let Some(p) = widened.run_request_permission.as_mut() {
            p.grant_tools.push("echo.status".to_string());
        }
        assert!(registry.admit_register(&register_with(widened)).is_err());
        let mut undeclared = approved;
        undeclared.run_request_permission = None;
        assert!(registry.admit_register(&register_with(undeclared)).is_ok());
    }
}
