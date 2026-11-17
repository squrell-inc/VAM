//! The steps that act as a user with a password: `login`, `change-password`,
//! `identity-request` and `create-user`. An `org` piece keeps its username, user id and generated password in
//! memory (`Account`); nothing here prints, logs or briefs a password or a token.
//!
//! This module is a child of `engine`, so it reads the engine's private state; the shared
//! runner files only register the steps.

use super::{conn, secret_hex, Engine, Fail, Secret};
use crate::scenario::Class;
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

/// The letter case the login name is sent in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoginCase {
    /// The name the user was created under.
    #[default]
    AsCreated,
    Lower,
    Upper,
}

impl LoginCase {
    fn apply(self, name: &str) -> String {
        match self {
            LoginCase::AsCreated => name.to_string(),
            LoginCase::Lower => name.to_ascii_lowercase(),
            LoginCase::Upper => name.to_ascii_uppercase(),
        }
    }
}

/// `login: { as, password, case, refused }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    #[serde(rename = "as")]
    pub as_: String,
    #[serde(default)]
    pub password: LoginPassword,
    /// The case of the login name sent: as created (default), lower or upper.
    #[serde(default)]
    pub case: LoginCase,
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

/// `identity-request: { as, credential, method, path, status, fields }`
///
/// `path` and the string values of `fields` may hold `{<alias>.userId}`, `{<alias>.username}`
/// and `{<alias>.USERNAME}` (the login name upper-cased) of any `org` identity in `given`; in
/// `path` the value is percent-encoded.
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
    /// Dotted paths into the JSON body, matched as in other `fields` (exact, `contains`,
    /// `matches`, `present`, `absent`).
    pub fields: Option<std::collections::BTreeMap<String, serde_yaml::Value>>,
}

/// `create-user: { as, username, userId, status, contains }`: `POST /v1/users` with a fresh
/// password the runner keeps to itself. `as` omitted is the bootstrap key. `username` may hold
/// `{tag}` (a token unique to the scenario and run) and the placeholders of `identity-request`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateUser {
    #[serde(rename = "as")]
    pub as_: Option<String>,
    pub username: String,
    /// Sent as `userId` when given (a client-chosen id; the node refuses it).
    #[serde(rename = "userId")]
    pub user_id: Option<String>,
    /// The exact status vamd must answer.
    pub status: u16,
    /// A substring the response body must contain.
    pub contains: Option<String>,
}

/// A short token (eight lowercase hex digits) that is the same for the same parts and differs
/// between runs, scenarios and aliases: the parts carry the run tag.
pub(super) fn unique_token(parts: &[&str]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

/// The words a `{<alias>.<word>}` placeholder can use.
pub const PLACEHOLDER_FIELDS: [&str; 3] = ["userId", "username", "USERNAME"];

/// Every `{<alias>.<word>}` in `text`: `(alias, word)`. `alias` is letters, digits, `_` and `-`;
/// `word` is letters. Text such as `{{name}}` or `{contains}` is not one.
pub fn placeholders(text: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('}') else { break };
        let inner = &rest[..close];
        if let Some((alias, word)) = inner.split_once('.') {
            let alias_ok = !alias.is_empty()
                && alias
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            let word_ok = !word.is_empty() && word.chars().all(|c| c.is_ascii_alphabetic());
            if alias_ok && word_ok {
                out.push((alias, word));
            }
        }
    }
    out
}

/// Every placeholder (`alias.word`) in the string values of a `fields` map.
pub fn field_placeholders(
    fields: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> Vec<String> {
    fn walk(v: &serde_yaml::Value, out: &mut Vec<String>) {
        match v {
            serde_yaml::Value::String(s) => {
                out.extend(placeholders(s).into_iter().map(|(a, w)| format!("{a}.{w}")))
            }
            serde_yaml::Value::Mapping(m) => m.values().for_each(|v| walk(v, out)),
            serde_yaml::Value::Sequence(q) => q.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    fields.values().for_each(|v| walk(v, &mut out));
    out
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
                Some(&json!({ "username": l.case.apply(&acc.username), "password": password.0 })),
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

    /// `text` with every `{<alias>.userId|username|USERNAME}` replaced; `encode` percent-encodes
    /// each value (for a path segment).
    fn expand(&self, text: &str, encode: bool) -> Result<String, Fail> {
        let mut out = text.to_string();
        for (alias, word) in placeholders(text) {
            if !PLACEHOLDER_FIELDS.contains(&word) {
                continue;
            }
            let acc = self.account(alias)?;
            let value = match word {
                "userId" => acc.user_id.clone(),
                "username" => acc.username.clone(),
                _ => acc.username.to_ascii_uppercase(),
            };
            let value = if encode {
                crate::api::seg(&value)
            } else {
                value
            };
            out = out.replace(&format!("{{{alias}.{word}}}"), &value);
        }
        Ok(out)
    }

    fn expand_value(&self, v: &serde_yaml::Value) -> Result<serde_yaml::Value, Fail> {
        use serde_yaml::Value as Y;
        Ok(match v {
            Y::String(s) => Y::String(self.expand(s, false)?),
            Y::Mapping(m) => {
                let mut out = serde_yaml::Mapping::new();
                for (k, v) in m {
                    out.insert(k.clone(), self.expand_value(v)?);
                }
                Y::Mapping(out)
            }
            Y::Sequence(q) => Y::Sequence(
                q.iter()
                    .map(|v| self.expand_value(v))
                    .collect::<Result<_, _>>()?,
            ),
            other => other.clone(),
        })
    }

    pub(super) fn identity_request(&mut self, x: &IdentityRequest) -> Result<(), Fail> {
        let acc = self.account(&x.as_)?.clone();
        let (bearer, shown) = match x.credential {
            Credential::Session => (
                acc.session.clone().map(|s| s.0),
                "a login session (a `login` step before this one)",
            ),
            Credential::PreviousSession => (
                acc.previous_session.clone().map(|s| s.0),
                "a previous session (two `login` steps before this one)",
            ),
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
        let path = self.expand(&x.path, true)?;
        let fields = match &x.fields {
            Some(f) => Some(
                f.iter()
                    .map(|(k, v)| Ok((k.clone(), self.expand_value(v)?)))
                    .collect::<Result<std::collections::BTreeMap<_, _>, Fail>>()?,
            ),
            None => None,
        };
        let write = method != reqwest::Method::GET;
        let body = (method == reqwest::Method::POST).then(|| json!({}));
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
        if r.status != x.status {
            return Err(product(
                want,
                scrub(&format!("{} {}", r.status, r.body), &[&bearer]),
            ));
        }
        if let Some(fields) = fields {
            let bad = super::field_problems(&r.json(), &fields);
            if !bad.is_empty() {
                return Err(product(
                    format!("{want} with the expected fields"),
                    scrub(&bad.join("; "), &[&bearer]),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn create_user(&mut self, c: &CreateUser, scenario_id: &str) -> Result<(), Fail> {
        let bearer = match &c.as_ {
            Some(a) => self
                .key_of(Some(a))?
                .unwrap_or_else(|| self.api.key().to_string()),
            None => self.api.key().to_string(),
        };
        let token = unique_token(&[scenario_id, &self.eff_tag().0]);
        let username = self.expand(&c.username, false)?.replace("{tag}", &token);
        let password = secret_hex(24);
        let mut body = json!({ "username": username, "password": password, "role": "member" });
        if let Some(id) = &c.user_id {
            body["userId"] = json!(id);
        }
        let want = format!(
            "POST /v1/users as {} for `{username}`{} answers {}{}",
            c.as_.as_deref().unwrap_or("the bootstrap key"),
            c.user_id
                .as_deref()
                .map(|i| format!(" with userId `{i}`"))
                .unwrap_or_default(),
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
                "/v1/users",
                Some(&body),
                Some(&bearer),
                true,
            )
            .map_err(conn)?;
        let seen = scrub(&format!("{} {}", r.status, r.body), &[&bearer, &password]);
        if r.status != c.status {
            return Err(product(want, seen));
        }
        if let Some(sub) = &c.contains {
            if !r.body.contains(sub.as_str()) {
                return Err(product(want, seen));
            }
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
        let x: IdentityRequest = serde_yaml::from_str(
            "{ as: m, credential: key, path: /v1/me, status: 200, fields: { userId: { matches: '^usr_' } } }",
        )
        .unwrap();
        assert_eq!(x.fields.map(|f| f.len()), Some(1));
    }

    #[test]
    fn login_case_and_create_user_parse() {
        let l: Login = serde_yaml::from_str("{ as: m, case: upper }").unwrap();
        assert_eq!(l.case, LoginCase::Upper);
        let l: Login = serde_yaml::from_str("{ as: m }").unwrap();
        assert_eq!(l.case, LoginCase::AsCreated);
        assert_eq!(LoginCase::Upper.apply("Ab+c@Ex.Test"), "AB+C@EX.TEST");
        assert_eq!(LoginCase::Lower.apply("Ab+c@Ex.Test"), "ab+c@ex.test");
        assert_eq!(LoginCase::AsCreated.apply("Ab+c@Ex.Test"), "Ab+c@Ex.Test");
        assert!(serde_yaml::from_str::<Login>("{ as: m, case: title }").is_err());
        let c: CreateUser =
            serde_yaml::from_str("{ username: x, userId: usr_a, status: 400 }").unwrap();
        assert_eq!((c.as_, c.user_id.as_deref()), (None, Some("usr_a")));
        assert!(serde_yaml::from_str::<CreateUser>("{ username: x }").is_err());
        assert!(serde_yaml::from_str::<CreateUser>("{ status: 200 }").is_err());
    }

    #[test]
    fn placeholders_are_found_and_plain_braces_are_not() {
        assert_eq!(
            placeholders(
                "/v1/users/{member.userId}/keys {a-b.USERNAME} {{name}} {contains} {x.y.z}"
            ),
            vec![("member", "userId"), ("a-b", "USERNAME")]
        );
        assert_eq!(unique_token(&["a", "b"]), unique_token(&["a", "b"]));
        assert_ne!(unique_token(&["a", "b"]), unique_token(&["a", "c"]));
        assert_eq!(unique_token(&["a"]).len(), 8);
    }

    #[test]
    fn secrets_never_reach_a_message() {
        assert_eq!(
            scrub("403 old=abc new=def", &["abc", "def", ""]),
            "403 old=<secret> new=<secret>"
        );
    }
}
