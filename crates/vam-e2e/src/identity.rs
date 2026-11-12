//! The steps that act as a user with a password: `login`, `change-password` and
//! `identity-request`. An `org` piece keeps its username, user id and generated password in
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
        let write = method != reqwest::Method::GET;
        let body = (method == reqwest::Method::POST).then(|| json!({}));
        let want = format!(
            "{} {} as `{}` with {} answers {}",
            x.method.to_uppercase(),
            x.path,
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
            .call(method, &x.path, body.as_ref(), Some(&bearer), write)
            .map_err(conn)?;
        if r.status != x.status {
            return Err(product(
                want,
                scrub(&format!("{} {}", r.status, r.body), &[&bearer]),
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
    }

    #[test]
    fn secrets_never_reach_a_message() {
        assert_eq!(
            scrub("403 old=abc new=def", &["abc", "def", ""]),
            "403 old=<secret> new=<secret>"
        );
    }
}
