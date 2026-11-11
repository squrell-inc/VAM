//! Skill script paths relative to the directory that contains `manifest.json`.
//!
//! An installable package cannot assume the Drive9 mount root or the directory
//! name it is installed under. Host catalog digests and guest execution both
//! call [`resolve_manifest_relative_entrypoint`], so the file that is hashed is
//! the file that runs.

use std::path::{Path, PathBuf};

/// Why a declared `localExec.entrypoint` cannot be resolved inside its skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillEntrypointError {
    /// Empty, or only `.` / `./` components.
    Empty,
    /// Absolute, or otherwise not a path inside the skill directory.
    NotRelative,
    /// `..`, a backslash, or an empty path component.
    EscapesSkillDir,
}

impl std::fmt::Display for SkillEntrypointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "skill entrypoint is empty"),
            Self::NotRelative => write!(
                f,
                "skill entrypoint must be relative to the directory that contains manifest.json (for example \"./execute.js\")"
            ),
            Self::EscapesSkillDir => write!(
                f,
                "skill entrypoint must stay inside the skill directory"
            ),
        }
    }
}

impl std::error::Error for SkillEntrypointError {}

/// Accept `execute.js` and `./execute.js`. Reject absolute paths and `..`.
pub fn validate_manifest_relative_entrypoint(entrypoint: &str) -> Result<(), SkillEntrypointError> {
    relative_components(entrypoint).map(|_| ())
}

/// Join `entrypoint` onto `skill_dir`, the directory that contains `manifest.json`.
pub fn resolve_manifest_relative_entrypoint(
    skill_dir: &Path,
    entrypoint: &str,
) -> Result<PathBuf, SkillEntrypointError> {
    let mut resolved = skill_dir.to_path_buf();
    for component in relative_components(entrypoint)? {
        resolved.push(component);
    }
    Ok(resolved)
}

fn relative_components(entrypoint: &str) -> Result<Vec<&str>, SkillEntrypointError> {
    if entrypoint.is_empty()
        || entrypoint.contains('\0')
        || entrypoint.contains('\\')
        || entrypoint.starts_with('/')
        || Path::new(entrypoint).is_absolute()
    {
        return Err(if entrypoint.is_empty() {
            SkillEntrypointError::Empty
        } else {
            SkillEntrypointError::NotRelative
        });
    }

    let mut parts = Vec::new();
    for component in entrypoint.split('/') {
        if component.is_empty() || component == ".." {
            return Err(SkillEntrypointError::EscapesSkillDir);
        }
        if component == "." {
            continue;
        }
        parts.push(component);
    }
    if parts.is_empty() {
        return Err(SkillEntrypointError::Empty);
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_from_a_relocated_directory() {
        let skill_dir = Path::new("/var/vam/catalog/acme--jira");
        let resolved =
            resolve_manifest_relative_entrypoint(skill_dir, "./execute.js").expect("relative");
        assert_eq!(resolved, skill_dir.join("execute.js"));
        assert!(!resolved.starts_with("/mnt/drive9/skills/jira-integration"));
    }

    #[test]
    fn rejects_mount_root_and_parent_segments() {
        assert!(validate_manifest_relative_entrypoint(
            "/mnt/drive9/skills/jira-integration/execute.js"
        )
        .is_err());
        assert!(validate_manifest_relative_entrypoint("../other/execute.js").is_err());
        assert!(validate_manifest_relative_entrypoint("").is_err());
        assert!(validate_manifest_relative_entrypoint("nested/execute.js").is_ok());
    }
}
