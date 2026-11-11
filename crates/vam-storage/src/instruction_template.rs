//! Host-side seeding of the guest system instruction template onto Drive9.

use std::fs;
use std::path::Path;

use crate::error::Drive9Error;

pub const SHIPPED_TEMPLATE: &str = include_str!("../../vam-runtime/templates/SYSTEM.md");
pub const SHIPPED_TEMPLATE_VERSION: u32 = 11;
pub const SYSTEM_MD_REL_PATH: &str = "workspace/.vam/SYSTEM.md";
pub const PLAN_MD_REL_PATH: &str = "workspace/PLAN.md";
pub const PLAN_MD_STUB: &str = "# Plan\n\nUpdate this file as you work.\n";

/// Parses `template_version` from YAML frontmatter (`---` delimited).
pub fn parse_template_version(content: &str) -> Option<u32> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let rest = trimmed.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("template_version:") {
            return val.trim().parse().ok();
        }
    }
    None
}

/// Copies or refreshes `workspace/.vam/SYSTEM.md` when missing or template version is stale.
pub fn seed_system_instruction_template(materialized_dir: &Path) -> Result<(), Drive9Error> {
    let dest = materialized_dir.join(SYSTEM_MD_REL_PATH);
    let should_write = if !dest.exists() {
        true
    } else {
        match fs::read_to_string(&dest) {
            Ok(existing) => {
                parse_template_version(&existing).unwrap_or(0) < SHIPPED_TEMPLATE_VERSION
            }
            Err(_) => true,
        }
    };

    if should_write {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, SHIPPED_TEMPLATE)?;
    }
    Ok(())
}

/// Writes a short `PLAN.md` stub when missing. Never overwrites an existing file.
pub fn seed_plan_md_stub(materialized_dir: &Path) -> Result<(), Drive9Error> {
    let dest = materialized_dir.join(PLAN_MD_REL_PATH);
    if dest.exists() {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&dest, PLAN_MD_STUB)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn seed_creates_workspace_system_md() {
        let dir = tempdir().unwrap();
        seed_system_instruction_template(dir.path()).unwrap();
        let path = dir.path().join(SYSTEM_MD_REL_PATH);
        assert!(path.is_file());
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(
            parse_template_version(&content),
            Some(SHIPPED_TEMPLATE_VERSION)
        );
        assert!(content.contains("{{tools.inventory}}"));
    }

    #[test]
    fn seed_skips_when_version_current() {
        let dir = tempdir().unwrap();
        seed_system_instruction_template(dir.path()).unwrap();
        let path = dir.path().join(SYSTEM_MD_REL_PATH);
        let original = fs::read_to_string(&path).unwrap();
        let customized = original.replace("autonomous agent", "customized agent");
        fs::write(&path, &customized).unwrap();
        seed_system_instruction_template(dir.path()).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("customized agent"));
    }

    #[test]
    fn seed_refreshes_stale_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(SYSTEM_MD_REL_PATH);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, "---\ntemplate_version: 0\n---\n\n# Old template\n").unwrap();
        seed_system_instruction_template(dir.path()).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            parse_template_version(&after),
            Some(SHIPPED_TEMPLATE_VERSION)
        );
    }

    #[test]
    fn seed_refreshes_previous_shipped_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(SYSTEM_MD_REL_PATH);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(
            &path,
            "---\ntemplate_version: 8\n---\n\n# Old template\n\nNo working habits.\n",
        )
        .unwrap();
        seed_system_instruction_template(dir.path()).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            parse_template_version(&after),
            Some(SHIPPED_TEMPLATE_VERSION)
        );
        assert_eq!(SHIPPED_TEMPLATE_VERSION, 11);
        assert!(after.contains("{{environment.guidance}}"));
        assert!(after.contains("{{playbook.procedure}}"));
        assert!(!after.contains("/mnt/drive9/workspace"));
        assert!(!after.contains("No working habits"));
    }

    #[test]
    fn shipped_template_v11_includes_playbook_procedure_and_guidance_slot() {
        assert_eq!(SHIPPED_TEMPLATE_VERSION, 11);
        assert!(SHIPPED_TEMPLATE.contains("{{playbook.procedure}}"));
        assert!(SHIPPED_TEMPLATE.contains("Playbook procedure"));
        assert!(SHIPPED_TEMPLATE.contains("{{environment.guidance}}"));
        assert!(!SHIPPED_TEMPLATE.contains("/mnt/drive9/workspace"));
        assert!(!SHIPPED_TEMPLATE.contains("read_file"));
    }

    #[test]
    fn seed_plan_md_stub_only_when_missing() {
        let dir = tempdir().unwrap();
        seed_plan_md_stub(dir.path()).unwrap();
        let path = dir.path().join(PLAN_MD_REL_PATH);
        assert!(path.is_file());
        let first = fs::read_to_string(&path).unwrap();
        assert!(first.contains("# Plan"));

        fs::write(&path, "# Custom plan\n\nAgent edits preserved.\n").unwrap();
        seed_plan_md_stub(dir.path()).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert!(second.contains("Agent edits preserved"));
        assert!(!second.contains("Update this file as you work"));
    }
}
