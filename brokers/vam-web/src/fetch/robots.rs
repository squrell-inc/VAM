//! robots.txt (RFC 9309): a small parser and a bounded per-host cache.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Only this much of a robots.txt is read and parsed.
pub const MAX_ROBOTS_BYTES: usize = 500 * 1024;
/// Hosts kept in the cache.
pub const CACHE_MAX_HOSTS: usize = 256;
/// How long a parsed robots.txt (or a 4xx "no rules") is trusted.
pub const TTL_FOUND: Duration = Duration::from_secs(60 * 60);
/// How long an unreachable robots.txt (5xx, 429, network) is remembered.
pub const TTL_UNREACHABLE: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    allow: bool,
    /// Normalized pattern, `$` kept at the end when anchored.
    pattern: String,
}

/// Rules that apply to our product token.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Robots {
    rules: Vec<Rule>,
    disallow_all: bool,
}

impl Robots {
    /// No restrictions (also the answer for a 4xx robots.txt).
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Everything is off limits (robots.txt unreachable, RFC 9309 2.3.1.4).
    pub fn disallow_all() -> Self {
        Self {
            rules: Vec::new(),
            disallow_all: true,
        }
    }

    /// Parse `text` for the product token `agent` (for example `VAM-WebFetch`).
    pub fn parse(text: &str, agent: &str) -> Self {
        let agent = agent
            .split('/')
            .next()
            .unwrap_or(agent)
            .trim()
            .to_ascii_lowercase();
        let mut text = text;
        if text.len() > MAX_ROBOTS_BYTES {
            let mut end = MAX_ROBOTS_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text = &text[..end];
        }

        struct Group {
            agents: Vec<String>,
            rules: Vec<Rule>,
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut last_was_agent = false;
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "user-agent" => {
                    if !last_was_agent || groups.is_empty() {
                        groups.push(Group {
                            agents: Vec::new(),
                            rules: Vec::new(),
                        });
                    }
                    if let Some(group) = groups.last_mut() {
                        group.agents.push(
                            value
                                .split('/')
                                .next()
                                .unwrap_or(value)
                                .trim()
                                .to_ascii_lowercase(),
                        );
                    }
                    last_was_agent = true;
                }
                "allow" | "disallow" => {
                    last_was_agent = false;
                    if let Some(group) = groups.last_mut() {
                        // An empty Disallow means "allow everything": no rule.
                        if !value.is_empty() {
                            group.rules.push(Rule {
                                allow: key == "allow",
                                pattern: normalize(value),
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        let specific: Vec<&Group> = groups
            .iter()
            .filter(|g| g.agents.iter().any(|a| a == &agent))
            .collect();
        let chosen: Vec<&Group> = if specific.is_empty() {
            groups
                .iter()
                .filter(|g| g.agents.iter().any(|a| a == "*"))
                .collect()
        } else {
            specific
        };
        Self {
            rules: chosen.iter().flat_map(|g| g.rules.clone()).collect(),
            disallow_all: false,
        }
    }

    /// Whether `path_and_query` (for example `/a/b?x=1`) may be fetched.
    pub fn allows(&self, path_and_query: &str) -> bool {
        if self.disallow_all {
            return false;
        }
        let path = normalize(if path_and_query.is_empty() {
            "/"
        } else {
            path_and_query
        });
        if path == "/robots.txt" {
            return true;
        }
        // Longest matching pattern wins; Allow wins a tie.
        let mut best: Option<(usize, bool)> = None;
        for rule in &self.rules {
            if pattern_matches(&rule.pattern, &path) {
                let len = rule.pattern.len();
                best = match best {
                    None => Some((len, rule.allow)),
                    Some((l, _)) if len > l => Some((len, rule.allow)),
                    Some((l, a)) if len == l => Some((l, a || rule.allow)),
                    keep => keep,
                };
            }
        }
        best.map(|(_, allow)| allow).unwrap_or(true)
    }
}

/// Canonical form: unreserved characters decoded, other escapes upper-case,
/// non-ASCII bytes escaped.
fn normalize(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                if v.is_ascii_alphanumeric() || matches!(v, b'-' | b'.' | b'_' | b'~') {
                    out.push(v as char);
                } else {
                    out.push_str(&format!("%{v:02X}"));
                }
                i += 3;
                continue;
            }
        }
        if b >= 0x80 {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push(b as char);
        }
        i += 1;
    }
    out
}

/// robots.txt pattern match: prefix match with `*` wildcards and a
/// trailing `$` anchor.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let parts: Vec<&str> = pattern.split('*').collect();
    let last = parts.len() - 1;
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !path.starts_with(part) {
                return false;
            }
            pos = part.len();
            if last == 0 {
                return !anchored || path.len() == part.len();
            }
        } else if i == last && anchored {
            return path.len() >= pos + part.len() && path.ends_with(part);
        } else {
            match path[pos..].find(part) {
                Some(idx) => pos += idx + part.len(),
                None => return false,
            }
        }
    }
    true
}

struct Entry {
    robots: Arc<Robots>,
    inserted: Instant,
    expires: Instant,
}

/// Parsed robots.txt per origin, bounded and expiring.
pub struct RobotsCache {
    inner: Mutex<HashMap<String, Entry>>,
    max_hosts: usize,
}

impl Default for RobotsCache {
    fn default() -> Self {
        Self::new(CACHE_MAX_HOSTS)
    }
}

impl RobotsCache {
    pub fn new(max_hosts: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_hosts: max_hosts.max(1),
        }
    }

    pub fn get(&self, origin: &str, now: Instant) -> Option<Arc<Robots>> {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match map.get(origin) {
            Some(e) if e.expires > now => Some(e.robots.clone()),
            Some(_) => {
                map.remove(origin);
                None
            }
            None => None,
        }
    }

    pub fn put(&self, origin: &str, robots: Arc<Robots>, ttl: Duration, now: Instant) {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, e| e.expires > now);
        while map.len() >= self.max_hosts && !map.contains_key(origin) {
            let oldest = map
                .iter()
                .min_by_key(|(_, e)| e.inserted)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    map.remove(&k);
                }
                None => break,
            }
        }
        map.insert(
            origin.to_string(),
            Entry {
                robots,
                inserted: now,
                expires: now + ttl,
            },
        );
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: &str = "VAM-WebFetch";

    fn rb(text: &str) -> Robots {
        Robots::parse(text, AGENT)
    }

    #[test]
    fn basic_disallow_and_allow() {
        let r = rb("User-agent: *\nDisallow: /private\nAllow: /private/open\n");
        assert!(r.allows("/"));
        assert!(r.allows("/public/x"));
        assert!(!r.allows("/private"));
        assert!(!r.allows("/private/secret.html"));
        assert!(r.allows("/private/open/page"));
    }

    #[test]
    fn specific_group_beats_star_and_groups_do_not_merge_with_star() {
        let r = rb("User-agent: *\nDisallow: /\n\nUser-agent: vam-webfetch\nDisallow: /nope\n");
        assert!(r.allows("/anything"));
        assert!(!r.allows("/nope/x"));
        let none = rb("User-agent: googlebot\nDisallow: /\n");
        assert!(none.allows("/x"));
    }

    #[test]
    fn consecutive_agent_lines_share_rules_and_version_suffix_is_ignored() {
        let r = rb("User-agent: bingbot\nUser-agent: VAM-WebFetch/1.0\nDisallow: /a\n");
        assert!(!r.allows("/a/b"));
        assert!(r.allows("/b"));
    }

    #[test]
    fn wildcards_anchors_and_longest_match() {
        let r = rb(
            "User-agent: *\nDisallow: /*.pdf$\nDisallow: /tmp*/cache\nDisallow: /a\nAllow: /a/b\nDisallow: /a/b/c\n",
        );
        assert!(!r.allows("/docs/file.pdf"));
        assert!(r.allows("/docs/file.pdf?x=1"));
        assert!(r.allows("/docs/file.pdfx"));
        assert!(!r.allows("/tmp123/cache"));
        assert!(!r.allows("/tmp/x/cache/y"));
        assert!(r.allows("/tmp/x/other"));
        assert!(!r.allows("/a/z"));
        assert!(r.allows("/a/b/x"));
        assert!(!r.allows("/a/b/c/d"));
    }

    #[test]
    fn allow_wins_a_tie() {
        let r = rb("User-agent: *\nDisallow: /x\nAllow: /x\n");
        assert!(r.allows("/x/y"));
    }

    #[test]
    fn empty_disallow_and_comments_and_unknown_lines() {
        let r = rb(
            "# c\nUser-agent: * # all\nDisallow:\nSitemap: https://a.test/s.xml\nCrawl-delay: 5\n",
        );
        assert!(r.allows("/anything"));
        let q = rb("User-agent: *\nDisallow: /search?q=\n");
        assert!(!q.allows("/search?q=cats"));
        assert!(q.allows("/search"));
    }

    #[test]
    fn percent_encoding_is_normalized() {
        let r = rb("User-agent: *\nDisallow: /a%2fb\nDisallow: /caf\u{e9}\nDisallow: /~user\n");
        assert!(!r.allows("/a%2Fb/c"));
        assert!(!r.allows("/caf%C3%A9/menu"));
        assert!(!r.allows("/%7Euser/home"));
    }

    #[test]
    fn robots_txt_itself_is_always_allowed_and_unreachable_means_disallow() {
        let r = rb("User-agent: *\nDisallow: /\n");
        assert!(r.allows("/robots.txt"));
        assert!(!r.allows("/"));
        assert!(!Robots::disallow_all().allows("/"));
        assert!(Robots::allow_all().allows("/x"));
    }

    #[test]
    fn oversize_text_is_cut_and_garbage_is_ignored() {
        let big = format!(
            "User-agent: *\nDisallow: /a\n{}\nDisallow: /late\n",
            "# pad\n".repeat(100_000)
        );
        let r = rb(&big);
        assert!(!r.allows("/a"));
        assert!(r.allows("/late"));
        let g = rb("\u{0}\u{1}:::\n:\nuser-agent\nDisallow /x\n");
        assert!(g.allows("/x"));
    }

    #[test]
    fn cache_is_bounded_and_expires() {
        let cache = RobotsCache::new(3);
        let t0 = Instant::now();
        let r = Arc::new(Robots::allow_all());
        for (i, host) in ["a", "b", "c", "d"].iter().enumerate() {
            cache.put(
                host,
                r.clone(),
                Duration::from_secs(100),
                t0 + Duration::from_secs(i as u64),
            );
        }
        assert_eq!(cache.len(), 3);
        assert!(cache.get("a", t0 + Duration::from_secs(5)).is_none());
        assert!(cache.get("d", t0 + Duration::from_secs(5)).is_some());
        assert!(cache.get("d", t0 + Duration::from_secs(500)).is_none());
        assert!(cache.is_empty() || cache.len() <= 3);
    }
}
