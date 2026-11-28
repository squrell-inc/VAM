//! Caps and timeouts for lockfile reading, fetching and unpacking (plan sections 5 and 6).

use std::time::Duration;

const MIB: u64 = 1024 * 1024;

/// Every cap the module enforces. `Default` is the production setting; tests inject smaller values.
#[derive(Debug, Clone)]
pub struct Limits {
    /// A lockfile (or `package.json`) larger than this is refused.
    pub max_lockfile_bytes: u64,
    /// Logical requirement lines in a `requirements.txt`.
    pub max_requirements: usize,
    /// Packages one skill may lock (all ecosystems together).
    pub max_packages: usize,
    /// Mirror index JSON.
    pub max_index_bytes: u64,
    /// One downloaded file.
    pub max_download_bytes: u64,
    /// Unpacked bytes of one archive.
    pub max_unpacked_archive_bytes: u64,
    /// Unpacked bytes of one skill (every package, every ecosystem).
    pub max_unpacked_skill_bytes: u64,
    /// One unpacked file.
    pub max_file_bytes: u64,
    /// Files in one archive.
    pub max_archive_files: usize,
    /// Files in one skill.
    pub max_skill_files: usize,
    /// TCP connect timeout.
    pub connect_timeout: Duration,
    /// Per request, headers and body together (the blocking client has no separate idle timeout).
    pub request_timeout: Duration,
    /// The whole `resolve` call; checked between reads.
    pub resolve_deadline: Duration,
    /// Redirect hops followed by hand.
    pub max_redirects: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_lockfile_bytes: 4 * MIB,
            max_requirements: 2000,
            max_packages: 500,
            max_index_bytes: 16 * MIB,
            max_download_bytes: 128 * MIB,
            max_unpacked_archive_bytes: 384 * MIB,
            max_unpacked_skill_bytes: 384 * MIB,
            max_file_bytes: 128 * MIB,
            max_archive_files: 20_000,
            max_skill_files: 50_000,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(120),
            resolve_deadline: Duration::from_secs(300),
            max_redirects: 3,
        }
    }
}

/// Fixed unpack shape limits (not tunable): path length, component length and depth.
pub(crate) const MAX_PATH_BYTES: usize = 512;
pub(crate) const MAX_COMPONENT_BYTES: usize = 255;
pub(crate) const MAX_DEPTH: usize = 32;
