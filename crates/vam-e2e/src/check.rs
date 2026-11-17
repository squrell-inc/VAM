//! `vam-e2e check`: validates the catalog, runs nothing.

use crate::catalog::{Catalog, CheckError};
use crate::lint::plan_number;
use crate::mock;
use crate::scenario::{Needs, Scenario, Status, Step, TAGS};
use crate::world::PieceKind;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn placeholder_re() -> Regex {
    Regex::new(r"\{\{(urn|name):([a-z0-9-]+)\}\}").unwrap()
}

fn kebab() -> Regex {
    Regex::new(r"^[a-z][a-z0-9]*(-[a-z0-9]+)*$").unwrap()
}

fn lint(errors: &mut Vec<CheckError>, path: &str, what: &str, name: &str) {
    if let Some(tok) = plan_number(name) {
        errors.push(CheckError::new(
            path,
            "plan-number",
            format!("{what} `{name}` carries a plan number (`{tok}`)"),
        ));
    }
}

/// Checks the catalog under `root` and returns every error (empty = clean).
pub fn check(root: &Path) -> Vec<CheckError> {
    let cat = Catalog::load(root);
    let mut errors = cat.errors.clone();
    let kebab = kebab();

    // World pieces.
    let mut piece_names = BTreeSet::new();
    let mut fixed_urns = BTreeSet::new();
    for (path, w) in &cat.pieces {
        let p = path.display().to_string();
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        lint(&mut errors, &p, "file", stem);
        lint(&mut errors, &p, "piece", &w.piece);
        if w.piece != stem {
            errors.push(CheckError::new(
                &p,
                "id-file",
                format!("piece `{}` must equal the file stem `{stem}`", w.piece),
            ));
        }
        if !kebab.is_match(&w.piece) {
            errors.push(CheckError::new(
                &p,
                "name",
                format!("piece `{}` must be kebab-case", w.piece),
            ));
        }
        if !piece_names.insert(w.piece.clone()) {
            errors.push(CheckError::new(
                &p,
                "duplicate",
                format!("duplicate piece `{}`", w.piece),
            ));
        }
        if w.egress.is_some() && w.kind != PieceKind::Secret {
            errors.push(CheckError::new(
                &p,
                "piece-body",
                "`egress` belongs only to a `secret` piece".to_string(),
            ));
        }
        if let Some(e) = &w.egress {
            if e.binding_id.trim().is_empty() || e.allowed_destinations.is_empty() {
                errors.push(CheckError::new(
                    &p,
                    "piece-body",
                    "`egress` needs a bindingId and at least one allowedDestinations entry"
                        .to_string(),
                ));
            }
        }
        if let Some(u) = &w.username {
            let shape = u.replace("{tag}", "");
            let bad = w.kind != PieceKind::Org
                || shape.is_empty()
                || shape.matches('@').count() > 1
                || !shape
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._+@-".contains(c));
            if bad {
                errors.push(CheckError::new(
                    &p,
                    "piece-body",
                    "`username` belongs to an `org` piece and holds only letters, digits, `. _ + - @` (at most one `@`) and `{tag}`".to_string(),
                ));
            }
        }
        match w.kind {
            PieceKind::Org | PieceKind::Secret | PieceKind::Broker => {}
            PieceKind::GitMarketplace => {
                if w.fixture.is_none() || !w.url.as_deref().is_some_and(|u| u.starts_with("https://")) {
                    errors.push(CheckError::new(
                        &p,
                        "piece-body",
                        "a git-marketplace piece needs `fixture` and an https `url`",
                    ));
                }
            }
            PieceKind::MockProvider => {
                if !matches!(
                    w.provider.as_deref(),
                    Some("microsoft-login" | "microsoft-graph" | "linear-server")
                ) {
                    errors.push(CheckError::new(
                        &p,
                        "piece-body",
                        "a mock-provider piece needs `provider: microsoft-login`, `microsoft-graph` or `linear-server`",
                    ));
                }
            }
            PieceKind::McpConnection => {
                let ok = w.body.as_ref().is_some_and(|b| {
                    b["connectionId"].as_str().is_some_and(|c| !c.is_empty())
                        && b["url"].as_str().is_some_and(|u| u.starts_with("https://"))
                        && b["authMode"].as_str() == Some("oauth")
                        && b["oauth"].is_object()
                });
                if !ok {
                    errors.push(CheckError::new(
                        &p,
                        "piece-body",
                        "an mcp-connection piece needs body.connectionId, an https body.url, body.authMode: oauth and body.oauth",
                    ));
                }
            }
            PieceKind::WorkspaceSeed => {
                if w.fixture.is_none() || w.workspace_id.is_none() || w.thread.is_none() {
                    errors.push(CheckError::new(
                        &p,
                        "piece-body",
                        "a workspace-seed piece needs `fixture`, `workspaceId` and `thread`",
                    ));
                }
            }
            PieceKind::Agent | PieceKind::Playbook | PieceKind::Skill => match w.urn() {
                Some(u) if u.contains("{{name}}") => {}
                // A playbook may carry a fixed urn so a mock script can name it, and an agent may with `fixedUrn`; each must be
                // unique in the world (a re-run puts the same document again).
                Some(u) if w.kind == PieceKind::Playbook || (w.kind == PieceKind::Agent && w.fixed_urn) => {
                    if !fixed_urns.insert(u.to_string()) {
                        errors.push(CheckError::new(
                            &p,
                            "piece-body",
                            format!("fixed urn `{u}` is used by another piece"),
                        ));
                    }
                }
                Some(_) => errors.push(CheckError::new(
                    &p,
                    "piece-body",
                    "body.spec.resourceId.urn must contain `{{name}}` (a playbook, or an agent with `fixedUrn: true`, may use a unique fixed urn)",
                )),
                None => errors.push(CheckError::new(
                    &p,
                    "piece-body",
                    "agent and playbook pieces need body.spec.resourceId.urn",
                )),
            },
        }
        // `{{urn:<piece>}}` and `{{name:<piece>}}` may name only a piece this one requires.
        let text = w.body.as_ref().map(|b| b.to_string()).unwrap_or_default();
        for cap in placeholder_re().captures_iter(&text) {
            if !w.requires.iter().any(|r| r == &cap[2]) {
                errors.push(CheckError::new(
                    &p,
                    "piece-requires",
                    format!(
                        "`{{{{{}:{}}}}}` names a piece that is not in `requires`",
                        &cap[1], &cap[2]
                    ),
                ));
            }
        }
        for r in &w.requires {
            if cat.piece(r).is_none() {
                errors.push(CheckError::new(
                    &p,
                    "piece-requires",
                    format!("requires unknown piece `{r}`"),
                ));
            }
        }
    }
    // Cycles in `requires`.
    for (path, w) in &cat.pieces {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<&str> = w.requires.iter().map(String::as_str).collect();
        while let Some(n) = stack.pop() {
            if n == w.piece {
                errors.push(CheckError::new(
                    path.display().to_string(),
                    "piece-requires",
                    format!("`{}` requires itself", w.piece),
                ));
                break;
            }
            if seen.insert(n) {
                if let Some(x) = cat.piece(n) {
                    stack.extend(x.requires.iter().map(String::as_str));
                }
            }
        }
    }

    // Benchmark tasks pin their task-set version (benchmark decision 3).
    let sets = if cat.scenarios.iter().any(|(_, s)| s.model.is_some()) {
        let f = root.join(crate::bench::TASK_SETS_FILE);
        match crate::bench::TaskSets::load(&f) {
            Ok(t) => Some(t),
            Err(e) => {
                errors.push(CheckError::new(f.display().to_string(), "task-set", e));
                None
            }
        }
    } else {
        None
    };

    // Scenarios.
    let mut ids: BTreeMap<String, usize> = BTreeMap::new();
    for (path, s) in &cat.scenarios {
        let p = path.display().to_string();
        let stem = path.file_stem().and_then(|x| x.to_str()).unwrap_or("");
        *ids.entry(s.id.clone()).or_default() += 1;
        check_scenario(&mut errors, &cat, &kebab, &p, stem, s);
        check_model(&mut errors, &p, s, sets.as_ref());
        check_cli_roots(&mut errors, root, &p, s);
    }
    for (id, n) in ids {
        if n > 1 {
            errors.push(CheckError::new(
                root.join("scenarios").display().to_string(),
                "duplicate",
                format!("scenario id `{id}` is used {n} times"),
            ));
        }
    }
    errors
}

fn check_scenario(
    errors: &mut Vec<CheckError>,
    cat: &Catalog,
    kebab: &Regex,
    p: &str,
    stem: &str,
    s: &Scenario,
) {
    lint(errors, p, "file", stem);
    lint(errors, p, "id", &s.id);
    if s.id != stem {
        errors.push(CheckError::new(
            p,
            "id-file",
            format!("id `{}` must equal the file stem `{stem}`", s.id),
        ));
    }
    if !kebab.is_match(&s.id) {
        errors.push(CheckError::new(
            p,
            "name",
            format!("id `{}` must be kebab-case", s.id),
        ));
    }
    if s.tags.is_empty() {
        errors.push(CheckError::new(p, "tags", "at least one tag is required"));
    }
    for t in &s.tags {
        lint(errors, p, "tag", t);
        if !TAGS.contains(&t.as_str()) {
            errors.push(CheckError::new(
                p,
                "tags",
                format!("unknown tag `{t}` (areas: {})", TAGS.join(" ")),
            ));
        }
    }
    if s.when.is_empty() {
        errors.push(CheckError::new(
            p,
            "steps",
            "`when` needs at least one step",
        ));
    }

    // given: pieces exist, aliases unique.
    let mut aliases = BTreeSet::new();
    let mut org_aliases = BTreeSet::new();
    let mut secret_aliases = BTreeSet::new();
    let mut mock_aliases = BTreeSet::new();
    let mut mkt_aliases = BTreeSet::new();
    for g in &s.given {
        let (piece, alias) = (g.piece(), g.alias());
        if cat
            .piece(piece)
            .is_some_and(|w| w.kind == PieceKind::GitMarketplace)
        {
            mkt_aliases.insert(alias.to_string());
        }
        if cat
            .piece(piece)
            .is_some_and(|w| matches!(w.kind, PieceKind::Secret | PieceKind::McpConnection))
        {
            secret_aliases.insert(alias.to_string());
        }
        if cat.piece(piece).is_some_and(|w| w.kind == PieceKind::Org) {
            org_aliases.insert(alias.to_string());
        }
        if cat
            .piece(piece)
            .is_some_and(|w| w.kind == PieceKind::MockProvider)
        {
            mock_aliases.insert(alias.to_string());
            // A connection to Microsoft 365 made with the login mock holds tokens the runner made.
            if cat
                .piece(piece)
                .is_some_and(|w| w.provider.as_deref() == Some("microsoft-login"))
            {
                secret_aliases.insert("microsoft-365".to_string());
            }
        }
        lint(errors, p, "alias", alias);
        if cat.piece(piece).is_none() {
            errors.push(CheckError::new(
                p,
                "given",
                format!("unknown world piece `{piece}`"),
            ));
        }
        if !aliases.insert(alias.to_string()) {
            errors.push(CheckError::new(
                p,
                "given",
                format!("duplicate alias `{alias}` (use `as:` for a piece used twice)"),
            ));
        }
    }

    // Steps: expect families, alias references, chat labels.
    let mut chat_labels = BTreeSet::new();
    for (pos, step) in s.steps() {
        match step {
            Step::Chat(c) => {
                refer(
                    errors,
                    p,
                    &pos,
                    &aliases,
                    c.agent().filter(|a| !a.starts_with("urn:vam:agent:")),
                );
                refer_org(errors, p, &pos, &org_aliases, c.identity());
                match c.label() {
                    Some(l) => {
                        lint(errors, p, "label", &l);
                        chat_labels.insert(l);
                    }
                    None => errors.push(CheckError::new(
                        p,
                        "label",
                        format!("{pos}: chat text needs an `e2e-step:<label>`"),
                    )),
                }
            }
            Step::StartPlaybook(sp) => {
                refer(errors, p, &pos, &aliases, Some(&sp.playbook));
                refer(errors, p, &pos, &aliases, sp.agent.as_deref());
                refer_org(errors, p, &pos, &org_aliases, sp.as_.as_deref());
                if let Some(l) = &sp.label {
                    lint(errors, p, "label", l);
                    chat_labels.insert(l.clone());
                }
            }
            Step::Expect(e) => {
                for m in crate::engine::notifications::check_expect(e, &org_aliases) {
                    errors.push(CheckError::new(p, "expect", format!("{pos}: {m}")));
                }
                for m in crate::engine::outcomes::check_expect(e, &aliases) {
                    errors.push(CheckError::new(p, "expect", format!("{pos}: {m}")));
                }
                if let Some(l) = &e.ledger {
                    if !aliases.contains(&l.piece) {
                        errors.push(CheckError::new(
                            p,
                            "expect",
                            format!("{pos}: `ledger.piece: {}` is not in `given`", l.piece),
                        ));
                    }
                }
                if let Some(c) = &e.connectors {
                    refer_org(errors, p, &pos, &org_aliases, c.as_.as_deref());
                    check_connector_id(errors, p, &pos, &c.id);
                }
                if let Some(m) = &e.marketplaces {
                    refer_mkt(errors, p, &pos, &mkt_aliases, Some(&m.marketplace));
                    refer_org(errors, p, &pos, &org_aliases, m.as_.as_deref());
                }
                if let Some(m) = &e.listings {
                    refer_mkt(errors, p, &pos, &mkt_aliases, Some(&m.marketplace));
                    refer_org(errors, p, &pos, &org_aliases, m.as_.as_deref());
                }
                if let Some(m) = &e.catalog {
                    refer_org(errors, p, &pos, &org_aliases, m.as_.as_deref());
                }
                if let Some(m) = &e.mock_requests {
                    refer_mock(errors, p, &pos, &mock_aliases, Some(&m.piece));
                    if !m.path.starts_with(['/', '*']) && !m.path.ends_with('*') {
                        errors.push(CheckError::new(
                            p,
                            "expect",
                            format!("{pos}: `mock-requests.path` must start with `/`"),
                        ));
                    }
                }
                if e.families() != 1 {
                    errors.push(CheckError::new(p, "expect", format!("{pos}: an expect needs exactly one of run, event, approval, gate, step, file, thread, refused, node, receipt, plain-http, vmm, secret-absent, flood, grant, fire, spaces, threads, runs, scheduled-runs, ledger, notifications, notification-summary, connectors, mock-requests, marketplaces, listings, catalog, agent-stats, release-diff (has {})", e.families())));
                }
                if e.fields.is_some()
                    && e.event.is_none()
                    && e.approval.is_none()
                    && e.gate.is_none()
                {
                    errors.push(CheckError::new(
                        p,
                        "expect",
                        format!("{pos}: `fields` is read by event, approval and gate expects only"),
                    ));
                }
                if e.tool.is_some() && e.event.is_none() && e.approval.is_none() {
                    errors.push(CheckError::new(
                        p,
                        "expect",
                        format!("{pos}: `tool` is read by event and approval expects only"),
                    ));
                }
                if e.state.is_some() && (e.step.is_none() || e.gate.is_some()) {
                    errors.push(CheckError::new(
                        p,
                        "expect",
                        format!("{pos}: `state` belongs with `step` (a step state), not a gate"),
                    ));
                }
                if e.node.as_ref().is_some_and(|n| n.fields.is_empty()) {
                    errors.push(CheckError::new(
                        p,
                        "expect",
                        format!("{pos}: `node` needs at least one field"),
                    ));
                }
                if e.receipt.as_ref().is_some_and(|r| r.fields.is_empty()) {
                    errors.push(CheckError::new(
                        p,
                        "expect",
                        format!("{pos}: `receipt` needs at least one field"),
                    ));
                }
                if e.plain_http.as_deref().is_some_and(|v| v != "refused") {
                    errors.push(CheckError::new(
                        p,
                        "expect",
                        format!("{pos}: `plain-http` supports only `refused`"),
                    ));
                }
                if e.vmm.as_ref().is_some_and(|v| !v.is_jailed()) {
                    errors.push(CheckError::new(
                        p,
                        "expect",
                        format!("{pos}: `vmm` supports only `jailed`"),
                    ));
                }
                for (family, v) in [("approval", &e.approval), ("gate", &e.gate)] {
                    if v.as_deref().is_some_and(|v| v != "pending") {
                        errors.push(CheckError::new(
                            p,
                            "expect",
                            format!("{pos}: `{family}` supports only `pending`"),
                        ));
                    }
                }
                if let Some(x) = &e.secret_absent {
                    if !secret_aliases.contains(&x.secret) {
                        errors.push(CheckError::new(
                            p,
                            "step-alias",
                            format!(
                                "{pos}: `secret: {}` is not a secret piece in `given`",
                                x.secret
                            ),
                        ));
                    }
                }
                if let Some(x) = &e.flood {
                    if x.rate_limited.is_none() && x.retry_after.is_none() && x.recovers.is_none() {
                        errors.push(CheckError::new(
                            p,
                            "expect",
                            format!("{pos}: `flood` needs rateLimited, retryAfter or recovers"),
                        ));
                    }
                    if x.retry_after.as_deref().is_some_and(|v| v != "present") {
                        errors.push(CheckError::new(
                            p,
                            "expect",
                            format!("{pos}: `retryAfter` supports only `present`"),
                        ));
                    }
                    if !s
                        .steps()
                        .take_while(|(q, _)| *q != pos)
                        .any(|(_, st)| matches!(st, Step::Flood(_)))
                    {
                        errors.push(CheckError::new(
                            p,
                            "expect",
                            format!("{pos}: `flood` expects an earlier `flood` step"),
                        ));
                    }
                }
                if let Some(t) = &e.thread {
                    if t.max_bytes == Some(0) {
                        errors.push(CheckError::new(
                            p,
                            "expect",
                            format!("{pos}: thread `maxBytes` must be at least 1"),
                        ));
                    }
                    if !["user", "assistant", "tool", "system"].contains(&t.role.as_str()) {
                        errors.push(CheckError::new(
                            p,
                            "expect",
                            format!(
                                "{pos}: thread role `{}` is not one of user, assistant, tool, system",
                                t.role
                            ),
                        ));
                    }
                    if t.contains.trim().is_empty() {
                        errors.push(CheckError::new(
                            p,
                            "expect",
                            format!("{pos}: thread needs a non-empty `contains`"),
                        ));
                    }
                }
                if let Some(ev) = &e.event {
                    if !crate::events::is_known(ev) {
                        errors.push(CheckError::new(
                            p,
                            "event",
                            format!(
                                "{pos}: `{ev}` is not a vamd event (try: {})",
                                crate::events::suggestions(ev).join(", ")
                            ),
                        ));
                    }
                }
                if let Some(f) = &e.file {
                    refer(errors, p, &pos, &aliases, f.agent.as_deref());
                    if [f.present.is_some(), f.absent.is_some(), f.equals.is_some()]
                        .iter()
                        .filter(|b| **b)
                        .count()
                        != 1
                    {
                        errors.push(CheckError::new(
                            p,
                            "expect",
                            format!("{pos}: file needs exactly one of present, absent, equals"),
                        ));
                    }
                }
            }
            Step::VerifyReceipt(v) => {
                for m in crate::engine::outcomes::check_step(step, &aliases) {
                    errors.push(CheckError::new(p, "verify-receipt", format!("{pos}: {m}")));
                }
                if v.tamper && v.pack {
                    errors.push(CheckError::new(
                        p,
                        "verify-receipt",
                        format!("{pos}: `tamper` and `pack` are separate checks, not both"),
                    ));
                }
            }
            Step::StopSession(r) | Step::RestartVamd(r) => {
                refer(errors, p, &pos, &aliases, r.agent.as_deref())
            }
            Step::Approve(d) | Step::Deny(d) => {
                refer_org(errors, p, &pos, &org_aliases, d.opts().as_.as_deref())
            }
            Step::DecideGate(g) => refer_org(errors, p, &pos, &org_aliases, g.as_.as_deref()),
            Step::Send(m) => {
                refer_org(errors, p, &pos, &org_aliases, m.as_.as_deref());
                // A queued message is a run prompt: its label is one a mock rule may use.
                chat_labels.extend(crate::scenario::label_of(&m.text));
            }
            Step::Answer(a) => refer_org(errors, p, &pos, &org_aliases, a.as_.as_deref()),
            Step::DecideTogether(d) => refer_org(errors, p, &pos, &org_aliases, d.as_.as_deref()),
            Step::Stop(_) | Step::KillGuest(_) => {}
            Step::NotificationMark(m) => {
                for msg in crate::engine::notifications::check_mark(m, &org_aliases) {
                    errors.push(CheckError::new(
                        p,
                        "notification-mark",
                        format!("{pos}: {msg}"),
                    ));
                }
            }
            Step::FireTrigger(f) => {
                // The label of the message the grant carries: a mock rule may use it.
                if let Some(l) = &f.label {
                    lint(errors, p, "label", l);
                    chat_labels.insert(l.clone());
                }
                if f.times == 0 {
                    errors.push(CheckError::new(
                        p,
                        "fire-trigger",
                        format!("{pos}: `times` must be at least 1"),
                    ));
                }
            }
            Step::RevokeGrant(_) => {}
            Step::EditAgentDraft(_) | Step::ReleaseAgent(_) => {
                for m in crate::engine::outcomes::check_step(step, &aliases) {
                    errors.push(CheckError::new(p, "step", format!("{pos}: {m}")));
                }
            }
            Step::PublishSkill(_) => {}
            Step::Cli(x) => {
                if x.args.is_empty() {
                    errors.push(CheckError::new(
                        p,
                        "cli",
                        format!("{pos}: `cli` needs at least one argument"),
                    ));
                }
                if x.root.from.is_some() == x.root.same {
                    errors.push(CheckError::new(
                        p,
                        "cli",
                        format!("{pos}: `root` needs exactly one of `from` and `same`"),
                    ));
                }
                if x.contains.iter().chain(&x.absent).any(|t| t.is_empty()) {
                    errors.push(CheckError::new(
                        p,
                        "cli",
                        format!("{pos}: `contains` and `absent` entries are not empty"),
                    ));
                }
            }
            Step::ConnectorNode(x) => {
                refer_org(errors, p, &pos, &org_aliases, x.as_.as_deref());
                check_connector_id(errors, p, &pos, &x.id);
            }
            Step::ConnectorConnect(x) => {
                refer_org(errors, p, &pos, &org_aliases, x.as_.as_deref());
                check_connector_id(errors, p, &pos, &x.id);
                refer_mock(errors, p, &pos, &mock_aliases, x.piece.as_deref());
            }
            Step::ConnectorCheck(x) => {
                refer_org(errors, p, &pos, &org_aliases, x.as_.as_deref());
                check_connector_id(errors, p, &pos, &x.id);
            }
            Step::MockProvider(x) => {
                refer_mock(errors, p, &pos, &mock_aliases, Some(&x.piece));
            }
            Step::GitTag(x) => {
                refer_mkt(errors, p, &pos, &mkt_aliases, Some(&x.piece));
                lint(errors, p, "tag", &x.tag);
            }
            Step::MarketplaceAdd(x) => {
                refer_mkt(errors, p, &pos, &mkt_aliases, Some(&x.piece));
                refer_org(errors, p, &pos, &org_aliases, x.as_.as_deref());
            }
            Step::MarketplaceRefresh(x)
            | Step::MarketplaceInstall(x)
            | Step::MarketplaceUpdate(x) => {
                refer_mkt(errors, p, &pos, &mkt_aliases, Some(&x.marketplace));
                refer_org(errors, p, &pos, &org_aliases, x.as_.as_deref());
                let needs_listing = !matches!(step, Step::MarketplaceRefresh(_));
                if needs_listing && x.listing.is_none() {
                    errors.push(CheckError::new(
                        p,
                        "marketplace",
                        format!("{pos}: install and update need a `listing`"),
                    ));
                }
            }
            Step::Login(x) => refer_org(errors, p, &pos, &org_aliases, Some(&x.as_)),
            Step::ChangePassword(x) => {
                refer_org(errors, p, &pos, &org_aliases, x.as_.as_deref());
                if x.as_.is_none() && x.via != crate::engine::identity::ChangeVia::Key {
                    errors.push(CheckError::new(
                        p,
                        "change-password",
                        format!("{pos}: `change-password` without `as` needs `via: key` (the bootstrap key has no session)"),
                    ));
                }
                if !(100..600).contains(&x.status) {
                    errors.push(CheckError::new(
                        p,
                        "change-password",
                        format!("{pos}: `status` must be an HTTP status"),
                    ));
                }
            }
            Step::CreateUser(x) => {
                refer_org(errors, p, &pos, &org_aliases, x.as_.as_deref());
                if x.username.trim().is_empty() || !(100..600).contains(&x.status) {
                    errors.push(CheckError::new(
                        p,
                        "create-user",
                        format!("{pos}: `create-user` needs a `username` and an HTTP status"),
                    ));
                }
                refer_placeholders(errors, p, &pos, &org_aliases, &x.username);
            }
            Step::IdentityRequest(x) => {
                refer_org(errors, p, &pos, &org_aliases, Some(&x.as_));
                refer_placeholders(errors, p, &pos, &org_aliases, &x.path);
                if let Some(f) = &x.fields {
                    for ph in crate::engine::identity::field_placeholders(f) {
                        refer_placeholders(errors, p, &pos, &org_aliases, &format!("{{{ph}}}"));
                    }
                }
                if !x.path.starts_with('/')
                    || !["get", "post", "put", "patch", "delete"]
                        .contains(&x.method.to_lowercase().as_str())
                    || !(100..600).contains(&x.status)
                {
                    errors.push(CheckError::new(
                        p,
                        "identity-request",
                        format!("{pos}: `identity-request` needs a method, a path starting with `/` and an HTTP status"),
                    ));
                }
            }
            Step::CatalogWrite(x) => {
                refer_org(errors, p, &pos, &org_aliases, x.as_.as_deref());
                if !matches!(x.kind.as_str(), "agent" | "playbook" | "skill") {
                    errors.push(CheckError::new(
                        p,
                        "marketplace",
                        format!("{pos}: `kind` is agent, playbook or skill"),
                    ));
                }
                if !matches!(x.action.as_str(), "put" | "release" | "delete") {
                    errors.push(CheckError::new(
                        p,
                        "marketplace",
                        format!("{pos}: `action` is put, release or delete"),
                    ));
                }
            }
            Step::NodeNotify(_)
            | Step::LinkContact(_)
            | Step::UnlinkContact(_)
            | Step::NotifyPrefs(_)
            | Step::ChannelClick(_) => {}
            Step::StopBroker(_) | Step::StartBroker(_) => {}
            Step::WaitScheduledRun(w) => {
                // The label of the message the schedule carries: a mock rule may use it.
                if let Some(l) = &w.label {
                    lint(errors, p, "label", l);
                    chat_labels.insert(l.clone());
                }
                if w.within
                    .as_deref()
                    .is_some_and(|d| crate::engine::parse_duration(d).is_none())
                {
                    errors.push(CheckError::new(
                        p,
                        "wait-scheduled-run",
                        format!("{pos}: `within` must look like `30s`"),
                    ));
                }
            }
            Step::CreateSpace(c) => {
                lint(errors, p, "label", &c.label);
                refer(errors, p, &pos, &aliases, c.agent.as_deref())
            }
            Step::RenameSpace(x) => lint(errors, p, "label", &x.label),
            Step::RenameThread(x) => lint(errors, p, "label", &x.label),
            Step::BearerRequest(b) => {
                // The request is a run start whose prompt carries the `bearer` label.
                chat_labels.insert("bearer".to_string());
                if !b.path.starts_with('/')
                    || !["get", "post"].contains(&b.method.to_lowercase().as_str())
                {
                    errors.push(CheckError::new(
                        p,
                        "bearer-request",
                        format!("{pos}: `bearer-request` needs a get or post and a path starting with `/`"),
                    ));
                }
            }
            Step::SetNode(n) => {
                if n.allowed_models.is_empty()
                    || n.allowed_models.iter().any(|m| m.trim().is_empty())
                {
                    errors.push(CheckError::new(
                        p,
                        "set-node",
                        format!("{pos}: `set-node` needs a non-empty `allowedModels` of model ids"),
                    ));
                }
            }
            Step::Flood(f) => {
                refer_org(errors, p, &pos, &org_aliases, f.as_.as_deref());
                if f.count == 0 || !f.path.starts_with('/') {
                    errors.push(CheckError::new(
                        p,
                        "flood",
                        format!("{pos}: `flood` needs a count of at least 1 and a path starting with `/`"),
                    ));
                }
            }
        }
    }

    // status / expectFail.
    match (s.status, &s.expect_fail) {
        (Status::Pending, None) => errors.push(CheckError::new(
            p,
            "expect-fail",
            "a pending scenario needs `expectFail {step, class}`",
        )),
        (Status::Active | Status::Retired, Some(_)) => errors.push(CheckError::new(
            p,
            "expect-fail",
            "only a pending scenario has `expectFail`",
        )),
        _ => {}
    }
    if let Some(ef) = &s.expect_fail {
        match ef.position() {
            Some(("given", _)) if !s.given.is_empty() => {}
            Some(("when", i)) if i < s.when.len() => {}
            Some(("then", i)) if i < s.then.len() => {}
            _ => errors.push(CheckError::new(
                p,
                "expect-fail",
                format!(
                    "step `{}` is not a step of this scenario (use given, when.N or then.N)",
                    ef.step
                ),
            )),
        }
    }

    // mock.
    let sends_model = s
        .steps()
        .any(|(_, st)| matches!(st, Step::Chat(_) | Step::StartPlaybook(_)));
    if s.needs == Needs::Process && s.mock.is_empty() && sends_model {
        errors.push(CheckError::new(
            p,
            "mock",
            "a `needs: process` scenario that sends a model message needs a `mock:` block",
        ));
    }
    if !s.mock.is_empty() {
        lint(errors, p, "mock id", &s.id);
        match mock::to_script(s) {
            Err(e) => errors.push(CheckError::new(p, "mock", e)),
            Ok(_) => {
                let rule_labels: BTreeSet<String> = mock::rule_labels(s).into_iter().collect();
                for l in &rule_labels {
                    lint(errors, p, "mock label", l);
                    if !chat_labels.contains(l) {
                        errors.push(CheckError::new(
                            p,
                            "mock-label",
                            format!("mock rule step `{l}` is never sent by a step"),
                        ));
                    }
                }
                for l in &chat_labels {
                    if !rule_labels.contains(l) {
                        errors.push(CheckError::new(
                            p,
                            "mock-label",
                            format!("step label `{l}` has no mock rule with `when.step: {l}`"),
                        ));
                    }
                }
            }
        }
    }
}

/// A `cli` step's fixture directory exists under `world/fixtures`, and `same` follows another
/// `cli` step.
fn check_cli_roots(errors: &mut Vec<CheckError>, root: &Path, p: &str, s: &Scenario) {
    let mut seen = false;
    for (pos, st) in s.steps() {
        let Step::Cli(x) = st else { continue };
        if let Some(from) = &x.root.from {
            let dir = root.join("world").join("fixtures").join(from);
            if !dir.is_dir() {
                errors.push(CheckError::new(
                    p,
                    "cli",
                    format!("{pos}: fixture directory `{from}` is not under world/fixtures"),
                ));
            }
        } else if x.root.same && !seen {
            errors.push(CheckError::new(
                p,
                "cli",
                format!("{pos}: `root: {{ same: true }}` needs an earlier `cli` step"),
            ));
        }
        seen = true;
    }
}

/// The `model` block of a benchmark task: a pinned version, a known endpoint, mode `on`, a mock
/// to run it against, and k of at least 1.
fn check_model(
    errors: &mut Vec<CheckError>,
    p: &str,
    s: &Scenario,
    sets: Option<&crate::bench::TaskSets>,
) {
    let Some(m) = &s.model else { return };
    lint(errors, p, "task", &m.task);
    if m.k == 0 {
        errors.push(CheckError::new(p, "model", "`k` must be at least 1"));
    }
    if m.modes.is_empty() || m.modes.iter().any(|x| x != "on") {
        errors.push(CheckError::new(
            p,
            "model",
            "`modes` may only hold `on` (machine off comes with its own mode)",
        ));
    }
    if s.mock.is_empty() {
        errors.push(CheckError::new(
            p,
            "model",
            "a `model` scenario needs a `mock:` block for `--bench --mock`",
        ));
    }
    if let Some(e) = &m.endpoint {
        if vam_inference::ModelCatalog::new().get(e).is_none() {
            errors.push(CheckError::new(
                p,
                "model",
                format!("endpoint `{e}` is not in the model catalog"),
            ));
        }
    }
    if let Some(t) = &m.timeout {
        if crate::engine::parse_duration(t).is_none() {
            errors.push(CheckError::new(p, "model", format!("bad timeout `{t}`")));
        }
    }
    if let Some(sets) = sets {
        if let Err(e) = sets.check(&m.version, &m.task) {
            errors.push(CheckError::new(p, "task-set", e));
        }
    }
}

/// `as:` must name an `org` piece of `given`.
/// Every `{<alias>.<word>}` in `text` names an org alias in `given` and a known word.
fn refer_placeholders(
    errors: &mut Vec<CheckError>,
    p: &str,
    pos: &str,
    orgs: &BTreeSet<String>,
    text: &str,
) {
    for (alias, word) in crate::engine::identity::placeholders(text) {
        if !crate::engine::identity::PLACEHOLDER_FIELDS.contains(&word) {
            errors.push(CheckError::new(
                p,
                "step-placeholder",
                format!(
                    "{pos}: `{{{alias}.{word}}}` is not a placeholder (userId, username or USERNAME)"
                ),
            ));
        } else if !orgs.contains(alias) {
            errors.push(CheckError::new(
                p,
                "step-alias",
                format!("{pos}: `{{{alias}.{word}}}` names `{alias}`, which is not an org piece in `given`"),
            ));
        }
    }
}

fn refer_org(
    errors: &mut Vec<CheckError>,
    p: &str,
    pos: &str,
    orgs: &BTreeSet<String>,
    a: Option<&str>,
) {
    if let Some(a) = a {
        if !orgs.contains(a) {
            errors.push(CheckError::new(
                p,
                "step-alias",
                format!("{pos}: `as: {a}` is not an org piece in `given`"),
            ));
        }
    }
}

fn refer(
    errors: &mut Vec<CheckError>,
    p: &str,
    pos: &str,
    aliases: &BTreeSet<String>,
    a: Option<&str>,
) {
    if let Some(a) = a {
        if !aliases.contains(a) {
            errors.push(CheckError::new(
                p,
                "step-alias",
                format!("{pos}: `{a}` is not in `given`"),
            ));
        }
    }
}

fn check_connector_id(errors: &mut Vec<CheckError>, p: &str, pos: &str, id: &str) {
    if !kebab().is_match(id) {
        errors.push(CheckError::new(
            p,
            "connector",
            format!("{pos}: connector id `{id}` must be kebab-case"),
        ));
    }
}

/// A `git-marketplace` alias named by a step must be in `given`.
fn refer_mkt(
    errors: &mut Vec<CheckError>,
    p: &str,
    pos: &str,
    mkts: &BTreeSet<String>,
    alias: Option<&str>,
) {
    if let Some(a) = alias.filter(|a| !mkts.contains(*a)) {
        errors.push(CheckError::new(
            p,
            "refs",
            format!("{pos}: `{a}` is not a git-marketplace piece in `given`"),
        ));
    }
}

/// A `mock-provider` alias named by a step must be in `given`.
fn refer_mock(
    errors: &mut Vec<CheckError>,
    p: &str,
    pos: &str,
    mocks: &BTreeSet<String>,
    alias: Option<&str>,
) {
    if let Some(a) = alias.filter(|a| !mocks.contains(*a)) {
        errors.push(CheckError::new(
            p,
            "refs",
            format!("{pos}: `{a}` is not a mock-provider piece in `given`"),
        ));
    }
}
