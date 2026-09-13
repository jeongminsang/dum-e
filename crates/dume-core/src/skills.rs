use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    pub file_path: PathBuf,
}

#[derive(Debug, Default, Clone)]
pub struct SkillRegistry {
    pub skills: HashMap<String, Skill>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
        }
    }

    /// Load skills from standard paths:
    /// 1. `.dume/skills/*.md` in current directory or git root
    /// 2. `~/.dume/skills/*.md` in home directory
    pub fn load_default() -> Self {
        let mut registry = Self::new();

        // 1. Current workspace .dume/skills
        let local_skills = PathBuf::from(".dume/skills");
        if local_skills.is_dir() {
            registry.load_from_dir(&local_skills);
        }

        // 2. User home ~/.dume/skills
        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            let user_skills = PathBuf::from(home).join(".dume/skills");
            if user_skills.is_dir() {
                registry.load_from_dir(&user_skills);
            }
        }

        registry
    }

    pub fn load_from_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
                if let Ok(skill) = Self::parse_skill_file(&path) {
                    self.skills.insert(skill.name.clone(), skill);
                }
            }
        }
    }

    pub fn parse_skill_file(path: &Path) -> std::io::Result<Skill> {
        let raw = std::fs::read_to_string(path)?;
        let (name, description, content) = parse_frontmatter(&raw, path);
        Ok(Skill {
            name,
            description,
            content,
            file_path: path.to_path_buf(),
        })
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    pub fn list(&self) -> Vec<&Skill> {
        let mut list: Vec<&Skill> = self.skills.values().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }
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
}
