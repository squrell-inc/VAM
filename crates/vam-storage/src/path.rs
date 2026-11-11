use crate::error::Drive9Error;
use std::path::{Path, PathBuf};

/// Sanitizes a path relative to a sandbox root, preventing any directory traversal escapes.
pub fn sanitize_path(root: &Path, rel_path: &Path) -> Result<PathBuf, Drive9Error> {
    // 1. Check for NUL bytes or control chars
    let s = rel_path.to_string_lossy();
    if s.contains('\0') || s.chars().any(|c| c.is_ascii_control()) {
        return Err(Drive9Error::InvalidPathCharacters(rel_path.to_path_buf()));
    }

    // 2. Resolve or analyze components to prevent ".." escape
    let mut normalized = PathBuf::new();
    for component in rel_path.components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                // Reject absolute path segments
                return Err(Drive9Error::SandboxViolation {
                    path: rel_path.to_path_buf(),
                    root: root.to_path_buf(),
                });
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(Drive9Error::SandboxViolation {
                        path: rel_path.to_path_buf(),
                        root: root.to_path_buf(),
                    });
                }
            }
            std::path::Component::Normal(val) => {
                normalized.push(val);
            }
        }
    }

    let absolute = root.join(normalized);

    // Double check with prefix verification on canonicalized root (if exists)
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    // For absolute, if it exists, canonicalize. If not, just ensure normal start_with.
    if let Ok(canonical_abs) = absolute.canonicalize() {
        if !canonical_abs.starts_with(&canonical_root) {
            return Err(Drive9Error::SandboxViolation {
                path: rel_path.to_path_buf(),
                root: root.to_path_buf(),
            });
        }
    } else {
        // If it doesn't exist, our normalization loop above already guaranteed no ".." escapes.
        // But let's perform a direct check to make sure it contains root's components.
        if !absolute.starts_with(&canonical_root) && !absolute.starts_with(root) {
            return Err(Drive9Error::SandboxViolation {
                path: rel_path.to_path_buf(),
                root: root.to_path_buf(),
            });
        }
    }

    Ok(absolute)
}

/// Checks that a manifest file path is a plain relative path: not empty, not
/// absolute, no empty, `.` or `..` component, and no NUL. A manifest can come
/// from a guest-written disk image, so a path is checked before it is used to
/// build a host path.
pub fn validate_manifest_path(path: &str) -> Result<(), Drive9Error> {
    let bad = path.is_empty()
        || path.contains('\0')
        || path.starts_with('/')
        || path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..");
    if bad {
        return Err(Drive9Error::InvalidManifestPath(
            path.chars().take(120).collect(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn manifest_paths_must_be_plain_relative_paths() {
        for ok in ["a", "a/b.txt", ".vam/threads/t/log/00000001.jsonl", "a b/c"] {
            assert!(validate_manifest_path(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/./b",
            "a//b",
            "a/",
            "a\0b",
            "./a",
            "..",
        ] {
            assert!(validate_manifest_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn test_sanitize_path_valid() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let p = sanitize_path(root, Path::new("foo/bar.txt")).unwrap();
        assert_eq!(p, root.join("foo/bar.txt"));

        let p2 = sanitize_path(root, Path::new("./foo/../bar/baz.txt")).unwrap();
        assert_eq!(p2, root.join("bar/baz.txt"));
    }

    #[test]
    fn test_sanitize_path_escapes() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        assert!(sanitize_path(root, Path::new("../escape.txt")).is_err());
        assert!(sanitize_path(root, Path::new("/absolute/escape.txt")).is_err());
        assert!(sanitize_path(root, Path::new("foo/../../escape.txt")).is_err());
    }

    #[test]
    fn test_sanitize_path_invalid_chars() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        assert!(sanitize_path(root, Path::new("foo\0bar.txt")).is_err());
        assert!(sanitize_path(root, Path::new("foo\x01bar.txt")).is_err());
    }
}
