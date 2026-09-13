use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    base_dir: PathBuf,
}

impl ArtifactStore {
    pub fn new<P: AsRef<Path>>(base_dir: P) -> Result<Self, io::Error> {
        let path = base_dir.as_ref().to_path_buf();
        fs::create_dir_all(&path)?;
        Ok(Self { base_dir: path })
    }

    pub fn save_artifact(&self, content: &[u8]) -> Result<String, io::Error> {
        let mut hasher = Sha256::new();
        hasher.update(content);
        let hash = hex::encode(hasher.finalize());

        let shard = &hash[..2];
        let shard_dir = self.base_dir.join(shard);
        fs::create_dir_all(&shard_dir)?;

        let final_path = shard_dir.join(&hash);
        if final_path.exists() {
            return Ok(hash);
        }

        // Write to temp file first, fsync, and atomic rename
        let temp_path = shard_dir.join(format!("{}.tmp", &hash));
        {
            let mut file = File::create(&temp_path)?;
            file.write_all(content)?;
            file.sync_all()?;
        }
        fs::rename(temp_path, final_path)?;

        Ok(hash)
    }

    pub fn get_artifact(&self, hash: &str) -> Result<Vec<u8>, io::Error> {
        if hash.len() < 2 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Hash too short"));
        }
        let shard = &hash[..2];
        let file_path = self.base_dir.join(shard).join(hash);
        let mut file = File::open(file_path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;

        // R7: Verify content SHA-256 matches expected hash (corruption / tampering guard)
        let mut hasher = Sha256::new();
        hasher.update(&buf);
        let actual_hash = hex::encode(hasher.finalize());
        if actual_hash != hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Artifact corruption detected: expected hash {} but content has {}", hash, actual_hash),
            ));
        }

        Ok(buf)
    }


    pub fn has_artifact(&self, hash: &str) -> bool {
        if hash.len() < 2 {
            return false;
        }
        let shard = &hash[..2];
        self.base_dir.join(shard).join(hash).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_artifact_save_and_retrieve() {
        let dir = tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();

        let data = b"Hello DUM-E artifact store";
        let hash = store.save_artifact(data).unwrap();

        assert!(store.has_artifact(&hash));
        let retrieved = store.get_artifact(&hash).unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn test_artifact_tampering_corruption_detection() {
        let dir = tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();

        let data = b"Original uncorrupted content";
        let hash = store.save_artifact(data).unwrap();

        // Tamper with the underlying file directly
        let shard = &hash[..2];
        let file_path = dir.path().join(shard).join(&hash);
        fs::write(&file_path, b"Tampered corrupted content").unwrap();

        // get_artifact MUST return InvalidData error, not return corrupted data
        let res = store.get_artifact(&hash);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_artifact_missing() {
        let dir = tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();

        let res = store.get_artifact("0123456789abcdef");
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}

