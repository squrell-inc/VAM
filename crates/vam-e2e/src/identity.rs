//! The steps that act as a user with a password: `login`, `change-password` and
//! `identity-request`. An `org` piece keeps its username, user id and generated password in
//! memory (`Account`); nothing here prints, logs or briefs a password or a token.
//!
//! This module is a child of `engine`, so it reads the engine's private state; the shared
//! runner files only register the steps.

use super::{conn, secret_hex, Engine, Fail, Secret};
use crate::scenario::Class;
use crate::world::PieceKind;
use serde::Deserialize;
use serde_json::json;

// ---- step shapes -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoginPassword {
    /// The identity's current password.
    #[default]
    Current,
    /// The password it had before its last successful change.
    Previous,
}

/// `login: { as, password, refused }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    #[serde(rename = "as")]
    pub as_: String,
    #[serde(default)]
    pub password: LoginPassword,
    /// `true`: the login must be refused (401) and nothing is stored.
    #[serde(default)]
    pub refused: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeVia {
    /// The identity's current login session token.
    #[default]
    Session,
    /// The identity's API key (the bootstrap key when `as` is omitted).
    Key,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CurrentForm {
    #[default]
    Right,
    Wrong,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NewForm {
    #[default]
    Fresh,
    Same,
}

fn yes() -> bool {
    true
}

/// `change-password: { as, via, current, new, idempotencyKey, status, contains }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangePassword {
    /// The identity; omitted means the bootstrap key (only meaningful with `via: key`).
    #[serde(rename = "as")]
    pub as_: Option<String>,
    #[serde(default)]
    pub via: ChangeVia,
    #[serde(default)]
    pub current: CurrentForm,
    #[serde(default)]
    pub new: NewForm,
    /// `false` omits the `Idempotency-Key` header.
    #[serde(default = "yes", rename = "idempotencyKey")]
    pub idempotency_key: bool,
    /// The exact status vamd must answer.
    pub status: u16,
    /// A substring the response body must contain.
    pub contains: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Credential {
    Session,
    PreviousSession,
    Key,
}

/// `identity-request: { as, credential, method, path, status }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityRequest {
    #[serde(rename = "as")]
    pub as_: String,
    pub credential: Credential,
    #[serde(default = "get")]
    pub method: String,
    pub path: String,
    /// The exact status vamd must answer.
    pub status: u16,
    /// A substring the response body must contain (judged after the status).
    pub contains: Option<String>,
    /// A substring the response body must not contain (judged after the status).
    pub lacks: Option<String>,
}

/// `put-draft: { piece, as, status, contains }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutDraft {
    /// Alias of a `playbook` or `agent` world piece in `given` (usually `deferred`).
    pub piece: String,
    /// The identity that writes it; omitted means the bootstrap key.
    #[serde(rename = "as")]
    pub as_: Option<String>,
    /// The exact status vamd must answer.
    pub status: u16,
    /// A substring the response body must contain.
    pub contains: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoleChange {
    /// `POST /v1/users/{id}/publish-grant`.
    Publisher,
    /// `DELETE /v1/users/{id}/publish-grant`.
    Member,
}

/// `set-role: { as, role }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetRole {
    #[serde(rename = "as")]
    pub as_: String,
    pub role: RoleChange,
}

fn get() -> String {
    "get".into()
}

// ---- state -----------------------------------------------------------------------------------

/// What the runner knows of an `org` identity beyond its API key. Never printed.
#[derive(Debug, Clone)]
pub(super) struct Account {
    pub username: String,
    pub user_id: String,
    pub current: Secret,
    pub previous: Option<Secret>,
    pub session: Option<Secret>,
    pub previous_session: Option<Secret>,
}

/// Replaces each `{{urn:<piece>:<as>}}` in `text` with what `lookup(piece, as)` finds
/// (percent-encoded with `encode`). `Err` names the first one with no URN. `{{urn:<piece>}}`
/// (no identity) is left alone: the world substitution owns it.
pub fn expand_urns(
    text: &str,
    encode: bool,
    mut lookup: impl FnMut(&str, &str) -> Option<String>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("{{urn:") {
        let after = &rest[i + 6..];
        let Some(j) = after.find("}}") else { break };
        let inner = &after[..j];
        out.push_str(&rest[..i]);
        match inner.split_once(':') {
            Some((piece, who)) => {
                let urn = lookup(piece, who).ok_or_else(|| inner.to_string())?;
                out.push_str(&if encode { crate::api::seg(&urn) } else { urn });
            }
            None => out.push_str(&rest[i..i + 6 + j + 2]),
        }
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn product(expected: impl Into<String>, observed: impl Into<String>) -> Fail {
    Fail::new(Class::Product, expected, observed)
}

fn scenario(expected: impl Into<String>, observed: impl Into<String>) -> Fail {
    Fail::new(Class::Scenario, expected, observed)
}

/// `text` with every non-empty secret hidden.
fn scrub(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        out = out.replace(s, "<secret>");
    }
    out
}

impl Engine {
    fn account(&self, alias: &str) -> Result<&Account, Fail> {
        self.ctx.accounts.get(alias).ok_or_else(|| {
            scenario(
                format!(
                    "`{alias}` is an org piece of a member role in `given` (it has a password)"
                ),
                "no such identity",
            )
        })
    }

    fn account_mut(&mut self, alias: &str) -> Result<&mut Account, Fail> {
        if !self.ctx.accounts.contains_key(alias) {
            self.account(alias)?;
        }
        Ok(self.ctx.accounts.get_mut(alias).expect("checked"))
    }

    pub(super) fn login(&mut self, l: &Login) -> Result<(), Fail> {
        let acc = self.account(&l.as_)?.clone();
        let password = match l.password {
            LoginPassword::Current => acc.current.clone(),
            LoginPassword::Previous => acc.previous.clone().ok_or_else(|| {
                scenario(
                    format!(
                        "`{}` has a previous password (a successful `change-password` before this step)",
                        l.as_
                    ),
                    "none yet",
                )
            })?,
        };
        let want = format!(
            "POST /v1/login as `{}` with the {} password is {}",
            l.as_,
            match l.password {
                LoginPassword::Current => "current",
                LoginPassword::Previous => "previous",
            },
            if l.refused {
                "refused (401)"
            } else {
                "accepted (200)"
            }
        );
        let r = self
            .api
            .call(
                reqwest::Method::POST,
                "/v1/login",
                Some(&json!({ "username": acc.username, "password": password.0 })),
                None,
                false,
            )
            .map_err(conn)?;
        let token = r.json()["token"].as_str().unwrap_or_default().to_string();
        let seen = |r: &crate::api::Reply| {
            // The success body carries tokens: only the status is shown for it.
            if r.ok() {
                format!("{} (body hidden)", r.status)
            } else {
                scrub(&format!("{} {}", r.status, r.body), &[&password.0])
            }
        };
        if l.refused {
            if r.status != 401 {
                return Err(product(want, seen(&r)));
            }
            return Ok(());
        }
        if r.status != 200 {
            return Err(product(want, seen(&r)));
        }
        if token.is_empty() {
            return Err(product(want, "200 without a token"));
        }
        // The session must be the identity's own (a wrong account would make every later step lie).
        let who = r.json()["userId"].as_str().unwrap_or_default().to_string();
        if !who.is_empty() && who != acc.user_id {
            return Err(Fail::new(
                Class::Harness,
                format!("the session of `{}` belongs to its user id", l.as_),
                "a login answered for another user id",
            ));
        }
        let acc = self.account_mut(&l.as_)?;
        if let Some(old) = acc.session.take() {
            acc.previous_session = Some(old);
        }
        acc.session = Some(Secret(token));
        Ok(())
    }

    pub(super) fn change_password(&mut self, c: &ChangePassword) -> Result<(), Fail> {
        let acc = match &c.as_ {
            Some(a) => self.ctx.accounts.get(a).cloned(),
            None => None,
        };
        let who = c.as_.as_deref().unwrap_or("the bootstrap key");
        let bearer = match c.via {
            ChangeVia::Session => {
                let a = c.as_.as_deref().ok_or_else(|| {
                    scenario(
                        "`as` with `via: session` (the bootstrap key has no session)",
                        "no `as`",
                    )
                })?;
                self.account(a)?
                    .session
                    .clone()
                    .ok_or_else(|| {
                        scenario(
                            format!("a session for `{a}` (a `login` step before this one)"),
                            "none yet",
                        )
                    })?
                    .0
            }
            ChangeVia::Key => match &c.as_ {
                Some(a) => self
                    .key_of(Some(a))?
                    .unwrap_or_else(|| self.api.key().to_string()),
                None => self.api.key().to_string(),
            },
        };
        let current_right = acc.as_ref().map(|a| a.current.0.clone());
        let current = match c.current {
            CurrentForm::Right => current_right.clone().unwrap_or_else(|| secret_hex(24)),
            CurrentForm::Wrong => secret_hex(24),
        };
        let new = match c.new {
            NewForm::Fresh => secret_hex(24),
            NewForm::Same => current_right.clone().unwrap_or_else(|| current.clone()),
        };
        let want = format!(
            "POST /v1/me/password as {who} via {} answers {}{}",
            match c.via {
                ChangeVia::Session => "its session",
                ChangeVia::Key => "its key",
            },
            c.status,
            c.contains
                .as_deref()
                .map(|s| format!(" with `{s}`"))
                .unwrap_or_default()
        );
        let r = self
            .api
            .call(
                reqwest::Method::POST,
                "/v1/me/password",
                Some(&json!({ "currentPassword": current, "newPassword": new })),
                Some(&bearer),
                c.idempotency_key,
            )
            .map_err(conn)?;
        let hidden = [bearer.as_str(), current.as_str(), new.as_str()];
        let seen = scrub(&format!("{} {}", r.status, r.body), &hidden);
        if r.status != c.status {
            return Err(product(want, seen));
        }
        if let Some(sub) = &c.contains {
            if !r.body.contains(sub.as_str()) {
                return Err(product(want, seen));
            }
        }
        if r.status == 200 && c.current == CurrentForm::Right && c.new == NewForm::Fresh {
            if let Some(a) = c
                .as_
                .as_deref()
                .filter(|a| self.ctx.accounts.contains_key(*a))
            {
                let acc = self.account_mut(a)?;
                acc.previous = Some(acc.current.clone());
                acc.current = Secret(new);
            }
        }
        Ok(())
    }

    pub(super) fn identity_request(&mut self, x: &IdentityRequest) -> Result<(), Fail> {
        // An `org` piece of role Owner is no second identity: it has no account, and its key is
        // the bootstrap key. `credential: key` as such a piece acts as the bootstrap key (an
        // admin's read of every draft).
        let owner_key = matches!(x.credential, Credential::Key)
            && !self.ctx.accounts.contains_key(&x.as_)
            && self
                .ctx
                .pieces
                .get(&x.as_)
                .is_some_and(|p| p.kind == PieceKind::Org);
        let acc = if owner_key {
            None
        } else {
            Some(self.account(&x.as_)?.clone())
        };
        let (bearer, shown) = match x.credential {
            Credential::Session => (
                acc.and_then(|a| a.session).map(|s| s.0),
                "a login session (a `login` step before this one)",
            ),
            Credential::PreviousSession => (
                acc.and_then(|a| a.previous_session).map(|s| s.0),
                "a previous session (two `login` steps before this one)",
            ),
            Credential::Key if owner_key => (Some(self.api.key().to_string()), "an API key"),
            Credential::Key => (self.key_of(Some(&x.as_))?, "an API key"),
        };
        let bearer =
            bearer.ok_or_else(|| scenario(format!("{shown} for `{}`", x.as_), "none yet"))?;
        let method = match x.method.to_lowercase().as_str() {
            "get" => reqwest::Method::GET,
            "post" => reqwest::Method::POST,
            "put" => reqwest::Method::PUT,
            "patch" => reqwest::Method::PATCH,
            "delete" => reqwest::Method::DELETE,
            other => {
                return Err(scenario(
                    "a method of get, post, put, patch or delete",
                    other.to_string(),
                ))
            }
        };
        let write = method != reqwest::Method::GET;
        let body = (method == reqwest::Method::POST).then(|| json!({}));
        let path = self.expand_draft_urns(&x.path, true)?;
        let contains = x
            .contains
            .as_deref()
            .map(|t| self.expand_draft_urns(t, false))
            .transpose()?;
        let lacks = x
            .lacks
            .as_deref()
            .map(|t| self.expand_draft_urns(t, false))
            .transpose()?;
        let want = format!(
            "{} {} as `{}` with {} answers {}",
            x.method.to_uppercase(),
            path,
            x.as_,
            match x.credential {
                Credential::Session => "its session",
                Credential::PreviousSession => "its previous session",
                Credential::Key => "its key",
            },
            x.status
        );
        let r = self
            .api
            .call(method, &path, body.as_ref(), Some(&bearer), write)
            .map_err(conn)?;
        let seen = scrub(&format!("{} {}", r.status, r.body), &[&bearer]);
        if r.status != x.status {
            return Err(product(want, seen));
        }
        if let Some(sub) = &contains {
            if !r.body.contains(sub.as_str()) {
                return Err(product(format!("{want} with `{sub}`"), seen));
            }
        }
        if let Some(sub) = &lacks {
            if r.body.contains(sub.as_str()) {
                return Err(product(format!("{want} without `{sub}`"), seen));
            }
        }
        Ok(())
    }

    /// Writes a draft of the piece as `as`. The URN's `{{name}}` becomes
    /// `<piece>-<as>-<scenario>-<runTag>`, so each identity writes a row of its own and an owner
    /// check never decides the answer.
    pub(super) fn put_draft(&mut self, p: &PutDraft, scenario_id: &str) -> Result<(), Fail> {
        let key = self.key_of(p.as_.as_deref())?;
        let (kind, body) = self.draft_body(&p.piece, p.as_.as_deref(), scenario_id)?;
        let coll = match kind {
            PieceKind::Playbook => "playbooks",
            PieceKind::Agent => "agents",
            _ => {
                return Err(scenario(
                    format!("`{}` is a playbook or agent piece", p.piece),
                    format!("kind {kind:?}"),
                ))
            }
        };
        let path = format!("/v1/{coll}");
        let want = format!(
            "PUT {path} as {} answers {}{}",
            p.as_
                .as_deref()
                .map(|a| format!("`{a}`"))
                .unwrap_or_else(|| "the bootstrap key".into()),
            p.status,
            p.contains
                .as_deref()
                .map(|s| format!(" with `{s}`"))
                .unwrap_or_default()
        );
        let r = self
            .api
            .put_as(key.as_deref(), &path, &body)
            .map_err(conn)?;
        let seen = match &key {
            Some(k) => scrub(&format!("{} {}", r.status, r.body), &[k]),
            None => format!("{} {}", r.status, r.body),
        };
        if r.status != p.status {
            return Err(product(want, seen));
        }
        if let Some(sub) = &p.contains {
            if !r.body.contains(sub.as_str()) {
                return Err(product(want, seen));
            }
        }
        Ok(())
    }

    /// Promotes or demotes an `org` identity as the bootstrap key. A refusal is the harness's
    /// failure, not the product's: the role change is arrangement, not the thing under test.
    pub(super) fn set_role(&mut self, s: &SetRole) -> Result<(), Fail> {
        let user_id = self.account(&s.as_)?.user_id.clone();
        let path = format!("/v1/users/{}/publish-grant", crate::api::seg(&user_id));
        let (verb, r) = match s.role {
            RoleChange::Publisher => ("POST", self.api.post(&path, &json!({})).map_err(conn)?),
            RoleChange::Member => ("DELETE", self.api.delete(&path).map_err(conn)?),
        };
        if !r.ok() {
            return Err(Fail::new(
                Class::Harness,
                format!("{verb} {path} for `{}` is accepted", s.as_),
                format!("{} {}", r.status, r.body),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_parse_with_defaults_and_reject_unknown_keys() {
        let l: Login = serde_yaml::from_str("{ as: m }").unwrap();
        assert_eq!((l.password, l.refused), (LoginPassword::Current, false));
        let l: Login =
            serde_yaml::from_str("{ as: m, password: previous, refused: true }").unwrap();
        assert_eq!((l.password, l.refused), (LoginPassword::Previous, true));
        assert!(serde_yaml::from_str::<Login>("{ as: m, pasword: current }").is_err());
        assert!(serde_yaml::from_str::<Login>("{ password: current }").is_err());

        let c: ChangePassword = serde_yaml::from_str("{ as: m, status: 200 }").unwrap();
        assert_eq!(
            (c.via, c.current, c.new, c.idempotency_key),
            (ChangeVia::Session, CurrentForm::Right, NewForm::Fresh, true)
        );
        let c: ChangePassword = serde_yaml::from_str(
            "{ via: key, current: wrong, new: same, idempotencyKey: false, status: 403, contains: x }",
        )
        .unwrap();
        assert_eq!(
            (c.as_.as_deref(), c.via, c.current, c.new, c.idempotency_key),
            (
                None,
                ChangeVia::Key,
                CurrentForm::Wrong,
                NewForm::Same,
                false
            )
        );
        assert!(serde_yaml::from_str::<ChangePassword>("{ as: m }").is_err());
        assert!(serde_yaml::from_str::<ChangePassword>("{ status: 1, via: cookie }").is_err());

        let x: IdentityRequest = serde_yaml::from_str(
            "{ as: m, credential: previous-session, path: /v1/me, status: 401 }",
        )
        .unwrap();
        assert_eq!(
            (x.credential, x.method.as_str()),
            (Credential::PreviousSession, "get")
        );
        assert!(
            serde_yaml::from_str::<IdentityRequest>("{ as: m, path: /v1/me, status: 1 }").is_err()
        );
        assert_eq!((x.contains, x.lacks), (None, None));
        let x: IdentityRequest = serde_yaml::from_str(
            "{ as: m, credential: key, path: /v1/me, status: 200, contains: a, lacks: b }",
        )
        .unwrap();
        assert_eq!(
            (x.contains.as_deref(), x.lacks.as_deref()),
            (Some("a"), Some("b"))
        );
    }

    #[test]
    fn put_draft_and_set_role_parse_and_reject_unknown_keys() {
        let p: PutDraft = serde_yaml::from_str("{ piece: probe, status: 403 }").unwrap();
        assert_eq!((p.as_.as_deref(), p.contains.as_deref()), (None, None));
        let p: PutDraft =
            serde_yaml::from_str("{ piece: probe, as: m, status: 403, contains: x }").unwrap();
        assert_eq!(
            (p.as_.as_deref(), p.contains.as_deref()),
            (Some("m"), Some("x"))
        );
        assert!(serde_yaml::from_str::<PutDraft>("{ as: m, status: 200 }").is_err());
        assert!(serde_yaml::from_str::<PutDraft>("{ piece: p, status: 200, lacks: x }").is_err());

        let r: SetRole = serde_yaml::from_str("{ as: m, role: publisher }").unwrap();
        assert_eq!(r.role, RoleChange::Publisher);
        let r: SetRole = serde_yaml::from_str("{ as: m, role: member }").unwrap();
        assert_eq!(r.role, RoleChange::Member);
        assert!(serde_yaml::from_str::<SetRole>("{ as: m, role: owner }").is_err());
        assert!(serde_yaml::from_str::<SetRole>("{ role: member }").is_err());
    }

    #[test]
    fn urn_placeholders_expand_per_identity_and_encode_in_paths() {
        let find = |piece: &str, who: &str| {
            (piece == "probe" && who == "pub").then(|| "urn:vam:agent:h/e2e/probe-pub".to_string())
        };
        assert_eq!(
            expand_urns("/v1/agents/{{urn:probe:pub}}/versions", true, find).unwrap(),
            "/v1/agents/urn%3Avam%3Aagent%3Ah%2Fe2e%2Fprobe-pub/versions"
        );
        assert_eq!(
            expand_urns("a {{urn:probe:pub}} b {{urn:probe:pub}}", false, find).unwrap(),
            "a urn:vam:agent:h/e2e/probe-pub b urn:vam:agent:h/e2e/probe-pub"
        );
        assert_eq!(
            expand_urns("{{urn:probe:member}}", false, find).unwrap_err(),
            "probe:member"
        );
        // The world's own `{{urn:<piece>}}` and plain text pass through.
        assert_eq!(
            expand_urns("x {{urn:probe}} {{name}} {{urn:", false, find).unwrap(),
            "x {{urn:probe}} {{name}} {{urn:"
        );
    }

    #[test]
    fn secrets_never_reach_a_message() {
        assert_eq!(
            scrub("403 old=abc new=def", &["abc", "def", ""]),
            "403 old=<secret> new=<secret>"
        );
    }
}
