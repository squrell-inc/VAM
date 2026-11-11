//! `vam catalog import <git url> --path <dir> --ref <tag>` (CN-4.5): install a listing of a trusted
//! marketplace at its tag. It is the same import as the Marketplace page: the node does the work
//! (`POST /v1/marketplaces/<id>/listings/<listing>/install`, or `/update` when an older tag of the
//! listing is installed). The URL must be a marketplace the node already trusts (an org admin adds
//! one with `vam marketplace add`); nothing is fetched from a URL the node does not trust.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::client::{self, Client};
use crate::config;
use crate::output::{fail, ExitCode};

/// A URL compared the way the node compares them: scheme and host in lower case, no trailing slash
/// and no `.git`.
pub fn normalize_url(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix(".git").unwrap_or(u).trim_end_matches('/');
    match u.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
            if path.is_empty() {
                format!("{}://{}", scheme.to_lowercase(), host.to_lowercase())
            } else {
                format!(
                    "{}://{}/{}",
                    scheme.to_lowercase(),
                    host.to_lowercase(),
                    path
                )
            }
        }
        None => u.to_string(),
    }
}

/// The marketplace of `GET /v1/marketplaces` whose URL is `url`.
pub fn find_marketplace<'a>(list: &'a Value, url: &str) -> Result<&'a Value> {
    let want = normalize_url(url);
    list["marketplaces"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["url"].as_str().is_some_and(|u| normalize_url(u) == want))
        .ok_or_else(|| {
            anyhow!(
                "{url} is not a marketplace this node trusts; an org admin adds one with \
                 `vam marketplace add <url> --publisher-domain <domain>`"
            )
        })
}

/// The listing of `GET /v1/marketplaces/<id>/listings` that lives in directory `dir` (the
/// directory its artifacts are under, e.g. `listings/microsoft-365`).
pub fn find_listing<'a>(listings: &'a Value, dir: &str) -> Result<&'a Value> {
    let dir = dir.trim().trim_matches('/');
    if dir.is_empty() {
        bail!("--path is the listing's directory in the repository, e.g. listings/microsoft-365");
    }
    let prefix = format!("{dir}/");
    let all: Vec<&Value> = listings["listings"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    let hits: Vec<&&Value> = all
        .iter()
        .filter(|l| {
            l["artifacts"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|a| a["path"].as_str().is_some_and(|p| p.starts_with(&prefix)))
        })
        .collect();
    match hits.as_slice() {
        [one] => Ok(**one),
        [] => {
            let known: Vec<String> = all
                .iter()
                .flat_map(|l| {
                    l["artifacts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|a| a["path"].as_str())
                        .filter_map(|p| {
                            p.rsplit_once("/skills/")
                                .map(|x| x.0)
                                .or_else(|| p.rsplit_once("/agents/").map(|x| x.0))
                                .or_else(|| p.rsplit_once("/playbooks/").map(|x| x.0))
                        })
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .collect();
            bail!(
                "no listing of that marketplace lives under `{dir}`{}",
                if known.is_empty() {
                    String::new()
                } else {
                    format!(" (listings are under: {})", known.join(", "))
                }
            )
        }
        _ => bail!("more than one listing lives under `{dir}`; name the listing's own directory"),
    }
}

/// `install` or `update`: an older tag of the listing is installed ⇒ update.
pub fn action_for(listing: &Value, git_ref: &str) -> &'static str {
    match listing["installed"]["ref"].as_str() {
        Some(installed) if installed != git_ref => "update",
        _ => "install",
    }
}

pub async fn run(
    host: Option<String>,
    token: Option<String>,
    url: &str,
    path: &str,
    git_ref: &str,
) -> Result<()> {
    if git_ref.trim().is_empty() {
        return Err(fail(
            ExitCode::Usage,
            "--ref is the tag to install, e.g. microsoft-365-v1.0.0",
        ));
    }
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    let all: Value = serde_json::from_str(&c.get("/v1/marketplaces").await?)
        .map_err(|_| anyhow!("the node's marketplace list is not JSON"))?;
    let market = find_marketplace(&all, url)?;
    let id = market["id"]
        .as_str()
        .ok_or_else(|| anyhow!("the node returned a marketplace without an id"))?;
    let listings: Value =
        serde_json::from_str(&c.get(&format!("/v1/marketplaces/{id}/listings")).await?)
            .map_err(|_| anyhow!("the node's listings are not JSON"))?;
    let listing = find_listing(&listings, path)?;
    let listing_id = listing["id"]
        .as_str()
        .ok_or_else(|| anyhow!("the node returned a listing without an id"))?;
    let index_ref = listing["ref"].as_str().unwrap_or_default();
    if index_ref != git_ref {
        bail!(
            "the marketplace index this node last read names `{index_ref}` for that listing, not \
             `{git_ref}`: run `vam marketplace refresh {id}` or install the tag it names"
        );
    }
    let action = action_for(listing, git_ref);
    let key = client::resolve_idempotency_key(None);
    let out = c
        .post_raw_json(
            &format!("/v1/marketplaces/{id}/listings/{listing_id}/{action}"),
            &json!({ "ref": git_ref }).to_string(),
            &key,
        )
        .await?;
    client::print_json(&out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_compare_the_way_the_node_compares_them() {
        assert_eq!(
            normalize_url("HTTPS://GitHub.com/squrell-inc/vam-marketplace.git/"),
            "https://github.com/squrell-inc/vam-marketplace"
        );
        assert_eq!(
            normalize_url("https://github.com/squrell-inc/vam-marketplace"),
            "https://github.com/squrell-inc/vam-marketplace"
        );
    }

    #[test]
    fn only_a_trusted_marketplace_is_found() {
        let list = json!({"marketplaces": [
            {"id": "squrell", "url": "https://github.com/squrell-inc/vam-marketplace"}
        ]});
        assert_eq!(
            find_marketplace(&list, "https://github.com/Squrell-inc/vam-marketplace.git")
                .map(|m| m["id"].as_str().unwrap().to_string())
                .ok(),
            // The path stays case-sensitive: this one is a different repository.
            None
        );
        assert_eq!(
            find_marketplace(&list, "https://GITHUB.com/squrell-inc/vam-marketplace/").unwrap()
                ["id"],
            "squrell"
        );
        let err = find_marketplace(&list, "https://evil.example/m").unwrap_err();
        assert!(err
            .to_string()
            .contains("not a marketplace this node trusts"));
    }

    #[test]
    fn the_listing_is_found_by_its_directory_and_an_update_by_the_installed_tag() {
        let listings = json!({"listings": [
            {"id": "microsoft-365", "ref": "v2", "installed": {"ref": "v1"},
             "artifacts": [{"path": "listings/microsoft-365/skills/m365-mail"}]},
            {"id": "other", "ref": "v1", "installed": null,
             "artifacts": [{"path": "listings/other/agents/a"}]}
        ]});
        let l = find_listing(&listings, "listings/microsoft-365/").unwrap();
        assert_eq!(l["id"], "microsoft-365");
        assert_eq!(action_for(l, "v2"), "update");
        assert_eq!(action_for(l, "v1"), "install");
        let o = find_listing(&listings, "listings/other").unwrap();
        assert_eq!(action_for(o, "v1"), "install");
        assert!(find_listing(&listings, "listings/nope")
            .unwrap_err()
            .to_string()
            .contains("listings/microsoft-365"));
        assert!(find_listing(&listings, "").is_err());
    }
}
