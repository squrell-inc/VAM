use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive9Config {
    pub root_dir: PathBuf,
}

impl Drive9Config {
    pub fn new<P: Into<PathBuf>>(root_dir: P) -> Self {
        Self {
            root_dir: root_dir.into(),
        }
    }

    pub fn layout(&self) -> Drive9Layout {
        Drive9Layout::new(&self.root_dir)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive9Layout {
    root: PathBuf,
}

impl Drive9Layout {
    pub fn new<P: Into<PathBuf>>(root: P) -> Self {
        Self { root: root.into() }
    }

    pub fn chunks_dir(&self) -> PathBuf {
        self.root.join("chunks")
    }

    pub fn chunk_path(&self, hash: &str) -> PathBuf {
        if hash.len() < 2 {
            self.chunks_dir().join("xx").join(hash)
        } else {
            self.chunks_dir().join(&hash[0..2]).join(hash)
        }
    }

    pub fn workspaces_dir(&self) -> PathBuf {
        self.root.join("workspaces")
    }

    pub fn workspace_dir(&self, workspace_id: &str) -> PathBuf {
        self.workspaces_dir().join(workspace_id)
    }

    pub fn manifests_dir(&self, workspace_id: &str) -> PathBuf {
        self.workspace_dir(workspace_id).join("manifests")
    }

    pub fn manifest_path(&self, workspace_id: &str, hash: &str) -> PathBuf {
        self.manifests_dir(workspace_id).join(hash)
    }

    pub fn head_path(&self, workspace_id: &str) -> PathBuf {
        self.workspace_dir(workspace_id).join("HEAD")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn cache_db_path(&self) -> PathBuf {
        self.cache_dir().join("db.sqlite")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(session_id)
    }

    /// Where a session tree whose commit failed is kept. Outside `sessions/`,
    /// which `WorkspaceManager::gc` sweeps, so nothing here is ever collected.
    pub fn kept_dir(&self) -> PathBuf {
        self.root.join("kept")
    }

    pub fn kept_session_dir(&self, kept_id: &str) -> PathBuf {
        self.kept_dir().join(kept_id)
    }

    pub fn session_image_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("drive9.ext4")
    }

    /// Private copies of the session image, one per turn commit
    /// (`<round>.ext4`). Next to the live image, never inside a jail.
    pub fn session_turns_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("turns")
    }

    pub fn session_turn_image_path(&self, session_id: &str, round: u32) -> PathBuf {
        self.session_turns_dir(session_id)
            .join(format!("{round}.ext4"))
    }

    pub fn templates_dir(&self) -> PathBuf {
        self.root.join("templates")
    }

    /// Golden ext4 template for `(workspace_id, head_hash | "empty", skills_digest)`.
    ///
    /// `workspace_id` is the host-prefixed id (`{owner}--{suffix}`). There is
    /// no fallback to an unprefixed path — a first session after the prefix
    /// lands misses once and re-bakes under the new key.
    pub fn template_path(
        &self,
        workspace_id: &str,
        head_hash: &str,
        skills_digest: &str,
    ) -> PathBuf {
        self.templates_dir()
            .join(workspace_id)
            .join(format!("{}_{}.ext4", head_hash, skills_digest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_owner_prefix;

    #[test]
    fn template_path_includes_prefixed_workspace() {
        let layout = Drive9Layout::new("/var/lib/vam/drive9");
        let alice = apply_owner_prefix("alice", "assistant").expect("alice prefix");
        let bob = apply_owner_prefix("bob", "assistant").expect("bob prefix");
        let alice_path = layout.template_path(&alice, "empty", "none");
        let bob_path = layout.template_path(&bob, "empty", "none");
        let unprefixed = layout.template_path("assistant", "empty", "none");

        assert_ne!(alice_path, bob_path);
        assert_ne!(alice_path, unprefixed);
        assert_ne!(bob_path, unprefixed);
        assert!(alice_path.ends_with("templates/alice--assistant/empty_none.ext4"));
        assert!(bob_path.ends_with("templates/bob--assistant/empty_none.ext4"));
        assert!(unprefixed.ends_with("templates/assistant/empty_none.ext4"));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageProfile {
    pub id: String,
    pub quota_bytes: u64,
}

impl StorageProfile {
    pub fn new(id: String, quota_bytes: u64) -> Self {
        Self { id, quota_bytes }
    }

    /// A profile whose mock image is 8 MiB. The default 1 GiB image is a sparse
    /// file that `fs::copy` (copy_file_range) materializes in full, so a test
    /// that clones a template twice writes several GiB.
    #[cfg(test)]
    pub(crate) fn small_for_test() -> Self {
        Self {
            id: "test".to_string(),
            quota_bytes: 8 * 1024 * 1024,
        }
    }

    pub fn default_profile() -> Self {
        Self {
            id: "default".to_string(),
            quota_bytes: 1024 * 1024 * 1024, // 1 GiB
        }
    }
}
