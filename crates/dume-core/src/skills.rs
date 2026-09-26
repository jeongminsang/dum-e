use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    pub file_path: PathBuf,
    pub content_hash: String,
    pub mtime_nanos: u128,
}

#[derive(Debug, Default, Clone)]
pub struct SkillRegistry {
    pub skills: HashMap<String, Skill>,
    allowed_roots: Vec<PathBuf>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
            allowed_roots: Vec::new(),
        }
    }

    /// Load skills from standard paths:
    /// 1. `.dume/skills/*.md` in current directory or git root
    /// 2. `~/.dume/skills/*.md` in home directory
    pub fn load_default() -> Self {
        Self::load_default_for(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    pub fn load_default_for(worktree: &Path) -> Self {
        let mut registry = Self::new();

        if let Ok(worktree_root) = std::fs::canonicalize(worktree) {
            let local_skills = worktree_root.join(".dume/skills");
            if local_skills.is_dir() && !is_symlink(&local_skills) {
                if let Ok(skills_root) = std::fs::canonicalize(&local_skills) {
                    if skills_root.starts_with(&worktree_root) {
                        registry.load_from_dir(&skills_root);
                    }
                }
            }
        }

        // 2. User home ~/.dume/skills
        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            let user_skills = PathBuf::from(&home).join(".dume/skills");
            if user_skills.is_dir() && !is_symlink(&user_skills) {
                if let (Ok(home_root), Ok(skills_root)) = (
                    std::fs::canonicalize(PathBuf::from(&home)),
                    std::fs::canonicalize(&user_skills),
                ) {
                    if skills_root.starts_with(home_root) {
                        registry.load_from_dir(&skills_root);
                    }
                }
            }
        }

        registry
    }

    /// Reload all skills from standard directories to discover new or modified files.
    pub fn reload(&mut self) {
        *self = Self::load_default();
    }

    pub fn load_from_dir(&mut self, dir: &Path) {
        if is_symlink(dir) {
            return;
        }
        let Ok(root) = std::fs::canonicalize(dir) else {
            return;
        };
        if !self.allowed_roots.contains(&root) {
            self.allowed_roots.push(root.clone());
        }
        let Ok(entries) = std::fs::read_dir(&root) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|kind| kind.is_symlink())
                && path.is_file()
                && path.extension().is_some_and(|ext| ext == "md")
            {
                if let Ok(skill) = Self::parse_skill_file(&path) {
                    self.skills.insert(skill.name.clone(), skill);
                }
            }
        }
    }

    pub fn parse_skill_file(path: &Path) -> std::io::Result<Skill> {
        if is_symlink(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Skill files must not be symbolic links",
            ));
        }
        let raw = std::fs::read_to_string(path)?;
        let (name, description, content) = parse_frontmatter(&raw, path);
        let mtime_nanos = std::fs::metadata(path)?
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let content_hash = hex::encode(hasher.finalize());

        Ok(Skill {
            name,
            description,
            content,
            file_path: path.to_path_buf(),
            content_hash,
            mtime_nanos,
        })
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// Retrieve skill, checking if the file was modified on disk and reloading if changed.
    pub fn get_or_reload(&mut self, name: &str) -> Option<&Skill> {
        if let Some(existing) = self.skills.get(name) {
            let path = existing.file_path.clone();
            let allowed = std::fs::canonicalize(&path).is_ok_and(|canonical| {
                self.allowed_roots
                    .iter()
                    .any(|root| canonical.starts_with(root))
            });
            if !allowed {
                return self.skills.get(name);
            }
            if let Ok(meta) = std::fs::metadata(&path) {
                let current_mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                if current_mtime != existing.mtime_nanos {
                    if let Ok(updated) = Self::parse_skill_file(&path) {
                        self.skills.insert(name.to_string(), updated);
                    }
                }
            }
        }
        self.skills.get(name)
    }

    pub fn list(&self) -> Vec<&Skill> {
        let mut list: Vec<&Skill> = self.skills.values().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    pub fn catalog(&self) -> Vec<crate::types::SkillMetadata> {
        let mut items: Vec<crate::types::SkillMetadata> = self
            .skills
            .values()
            .map(|s| crate::types::SkillMetadata {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        items
    }

    /// Format catalog into a concise deterministic description for tool definitions.
    pub fn format_catalog_description(&self) -> String {
        let cat = self.catalog();
        if cat.is_empty() {
            "Load the full instruction body of a specific skill into active context by name.".to_string()
        } else {
            let entries: Vec<String> = cat
                .into_iter()
                .map(|s| format!("'{}': {}", s.name, s.description))
                .collect();
            format!(
                "Load the full instruction body of a specific skill into active context by name. Available skills: [{}]",
                entries.join("; ")
            )
        }
    }


    /// Check if the skill's content is present in the provided message contents.
    pub fn is_content_in_messages<'a, I>(content: &str, message_contents: I) -> bool
    where
        I: IntoIterator<Item = &'a str>,
    {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return false;
        }
        message_contents.into_iter().any(|c| c.contains(trimmed))
    }
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}



/// Simple YAML frontmatter parser for markdown files without heavy dependencies.
fn parse_frontmatter(raw: &str, path: &Path) -> (String, String, String) {
    let fallback_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return (fallback_name, String::new(), raw.to_string());
    }

    let rest = &trimmed[3..];
    let Some(end_idx) = rest.find("\n---") else {
        return (fallback_name, String::new(), raw.to_string());
    };

    let frontmatter = &rest[..end_idx];
    let body = rest[end_idx + 4..].trim_start().to_string();

    let mut name = fallback_name;
    let mut description = String::new();

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = v.trim().trim_matches('"').trim_matches('\'');
            if key.eq_ignore_ascii_case("name") && !val.is_empty() {
                name = val.to_string();
            } else if key.eq_ignore_ascii_case("description") {
                description = val.to_string();
            }
        }
    }

    (name, description, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_standard() {
        let raw = r#"---
name: release
description: Prepare, verify, publish releases.
---

# Releasing DUM-E
Some content here.
"#;
        let (name, desc, body) = parse_frontmatter(raw, Path::new("release.md"));
        assert_eq!(name, "release");
        assert_eq!(desc, "Prepare, verify, publish releases.");
        assert!(body.starts_with("# Releasing DUM-E"));
    }

    #[test]
    fn test_parse_frontmatter_missing() {
        let raw = "# No Frontmatter\nJust content";
        let (name, desc, body) = parse_frontmatter(raw, Path::new("my-tool.md"));
        assert_eq!(name, "my-tool");
        assert_eq!(desc, "");
        assert_eq!(body, raw);
    }

    #[test]
    fn test_skill_reload_on_disk_change() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test-skill.md");
        std::fs::write(&file_path, "---\nname: test-skill\ndescription: initial\n---\nInitial body").unwrap();

        let mut registry = SkillRegistry::new();
        registry.load_from_dir(dir.path());
        assert_eq!(registry.get("test-skill").unwrap().content, "Initial body");

        // Sleep briefly to ensure mtime changes
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&file_path, "---\nname: test-skill\ndescription: updated\n---\nUpdated body").unwrap();

        let reloaded = registry.get_or_reload("test-skill").unwrap();
        assert_eq!(reloaded.description, "updated");
        assert_eq!(reloaded.content, "Updated body");
    }

    #[test]
    fn skills_are_scoped_to_the_requested_worktree() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_skills = first.path().join(".dume/skills");
        let second_skills = second.path().join(".dume/skills");
        std::fs::create_dir_all(&first_skills).unwrap();
        std::fs::create_dir_all(&second_skills).unwrap();
        std::fs::write(first_skills.join("first-worktree-only.md"), "first").unwrap();
        std::fs::write(second_skills.join("second-worktree-only.md"), "second").unwrap();

        let registry = SkillRegistry::load_default_for(second.path());

        assert!(registry.get("second-worktree-only").is_some());
        assert!(registry.get("first-worktree-only").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn skill_loader_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "private content").unwrap();
        symlink(outside.path().join("secret.md"), root.path().join("secret.md")).unwrap();

        let mut registry = SkillRegistry::new();
        registry.load_from_dir(root.path());

        assert!(registry.get("secret").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn worktree_skill_loader_rejects_symlinked_parent_and_reload_escape() {
        use std::os::unix::fs::symlink;

        let worktree = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_skills = outside.path().join(".dume/skills");
        std::fs::create_dir_all(&outside_skills).unwrap();
        std::fs::write(
            outside_skills.join("outside-only.md"),
            "---\nname: outside-only\ndescription: outside\n---\nPrivate",
        )
        .unwrap();
        symlink(outside.path().join(".dume"), worktree.path().join(".dume")).unwrap();
        let registry = SkillRegistry::load_default_for(worktree.path());
        assert!(registry.get("outside-only").is_none());

        std::fs::remove_file(worktree.path().join(".dume")).unwrap();
        let local_skills = worktree.path().join(".dume/skills");
        std::fs::create_dir_all(&local_skills).unwrap();
        std::fs::write(
            local_skills.join("reload-check.md"),
            "---\nname: reload-check\ndescription: local\n---\nLocal content",
        )
        .unwrap();
        let mut registry = SkillRegistry::load_default_for(worktree.path());
        std::fs::remove_dir_all(worktree.path().join(".dume")).unwrap();
        symlink(outside.path().join(".dume"), worktree.path().join(".dume")).unwrap();
        std::fs::write(
            outside_skills.join("reload-check.md"),
            "---\nname: reload-check\ndescription: escaped\n---\nOutside content",
        )
        .unwrap();
        assert_eq!(
            registry.get_or_reload("reload-check").unwrap().content,
            "Local content"
        );
    }
}
