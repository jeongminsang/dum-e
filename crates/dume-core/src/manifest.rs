use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedArtifact {
    pub path: String,
    pub hash: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultManifest {
    pub attempt_id: String,
    pub candidate_commit: String,
    pub modified_files: Vec<String>,
    pub changed_artifacts: Vec<ChangedArtifact>,
    pub test_results: Vec<TestResult>,
    pub summary: String,
}

impl ResultManifest {
    pub fn compute_hash(&self) -> String {
        let serialized = serde_json::to_vec(self).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(&serialized);
        hex::encode(hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_hash_reproducibility() {
        let manifest = ResultManifest {
            attempt_id: "att_123".to_string(),
            candidate_commit: "c0ffee1234".to_string(),
            modified_files: vec!["src/main.rs".to_string()],
            changed_artifacts: vec![],
            test_results: vec![TestResult {
                name: "cargo test".to_string(),
                passed: true,
                exit_code: 0,
                stdout: "ok".to_string(),
                stderr: "".to_string(),
            }],
            summary: "Implemented core types".to_string(),
        };

        let hash1 = manifest.compute_hash();
        let hash2 = manifest.compute_hash();
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64);
    }
}
