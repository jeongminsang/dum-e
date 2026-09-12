use crate::manifest::ResultManifest;
use crate::types::Task;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationVerdict {
    pub passed: bool,
    pub allowed_paths_passed: bool,
    pub acceptance_passed: bool,
    pub reason: String,
}

pub fn verify_allowed_paths(task: &Task, manifest: &ResultManifest) -> bool {
    let allowed = match &task.allowed_paths {
        Some(paths) if !paths.is_empty() => paths,
        _ => return true, // No restriction
    };

    manifest.modified_files.iter().all(|file| {
        allowed.iter().any(|pattern| {
            if pattern.ends_with('/') {
                file.starts_with(pattern)
            } else if pattern.starts_with("*.") {
                let ext = &pattern[1..];
                file.ends_with(ext)
            } else {
                file == pattern || file.starts_with(&format!("{}/", pattern))
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_allowed_paths() {
        let task = Task {
            id: "t1".to_string(),
            goal_id: "g1".to_string(),
            title: "T".to_string(),
            description: "D".to_string(),
            status: crate::types::TaskStatus::Ready,
            dependencies: vec![],
            acceptance_criteria: vec![],
            allowed_paths: Some(vec!["src/".to_string(), "docs/readme.md".to_string()]),
            target_branch: "main".to_string(),
            created_at: 0,
            updated_at: 0,
        };

        let mut manifest = ResultManifest {
            attempt_id: "a1".to_string(),
            candidate_commit: "c1".to_string(),
            modified_files: vec!["src/lib.rs".to_string(), "docs/readme.md".to_string()],
            changed_artifacts: vec![],
            test_results: vec![],
            summary: "ok".to_string(),
        };

        assert!(verify_allowed_paths(&task, &manifest));

        // Add unauthorized file
        manifest.modified_files.push("secret.env".to_string());
        assert!(!verify_allowed_paths(&task, &manifest));
    }
}
