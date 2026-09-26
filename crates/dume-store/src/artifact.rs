use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
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

    fn validate_hash(hash: &str) -> Result<(), io::Error> {
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Invalid artifact hash '{}': must be 64-char hex SHA-256", hash),
            ));
        }
        Ok(())
    }

    fn open_artifact(&self, hash: &str) -> Result<File, io::Error> {
        Self::validate_hash(hash)?;
        let root = fs::canonicalize(&self.base_dir)?;
        let shard = self.base_dir.join(&hash[..2]);
        let shard_meta = fs::symlink_metadata(&shard)?;
        if shard_meta.file_type().is_symlink() || !shard_meta.is_dir() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Artifact shard must be a real directory"));
        }
        if !fs::canonicalize(&shard)?.starts_with(root) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Artifact shard escapes the configured store"));
        }

        let path = shard.join(hash);
        let path_meta = fs::symlink_metadata(&path)?;
        if path_meta.file_type().is_symlink() || !path_meta.is_file() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Artifact must be a regular file"));
        }
        let file = OpenOptions::new().read(true).open(&path)?;
        let opened_meta = file.metadata()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if path_meta.dev() != opened_meta.dev() || path_meta.ino() != opened_meta.ino() {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Artifact path changed while opening"));
            }
        }
        #[cfg(not(unix))]
        if !fs::canonicalize(&path)?.starts_with(root) || !opened_meta.is_file() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Artifact path escapes the configured store"));
        }
        Ok(file)
    }

    pub fn save_artifact(&self, content: &[u8]) -> Result<String, io::Error> {
        let mut hasher = Sha256::new();
        hasher.update(content);
        let hash = hex::encode(hasher.finalize());

        let shard = &hash[..2];
        let shard_dir = self.base_dir.join(shard);
        fs::create_dir_all(&shard_dir)?;
        let shard_metadata = fs::symlink_metadata(&shard_dir)?;
        if shard_metadata.file_type().is_symlink() || !shard_metadata.is_dir() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Artifact shard must be a real directory"));
        }
        if !fs::canonicalize(&shard_dir)?.starts_with(fs::canonicalize(&self.base_dir)?) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Artifact shard escapes the configured store"));
        }

        let final_path = shard_dir.join(&hash);
        if final_path.exists() {
            self.get_artifact(&hash)?;
            return Ok(hash);
        }

        // Unique temp file name prevents race conditions across concurrent writers of identical/different artifacts
        let unique_id = format!("{}_{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        let temp_path = shard_dir.join(format!("{}.tmp.{}", &hash, unique_id));
        {
            let mut file = File::create(&temp_path)?;
            file.write_all(content)?;
            file.sync_all()?;
        }
        // Atomic rename with error handling
        if let Err(e) = fs::rename(&temp_path, &final_path) {
            let _ = fs::remove_file(&temp_path);
            if final_path.exists() {
                // Another writer succeeded concurrently. Verify target content hash.
                let mut existing_file = File::open(&final_path)?;
                let mut hasher = Sha256::new();
                let mut buffer = [0u8; 8192];
                loop {
                    let n = existing_file.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buffer[..n]);
                }
                let existing_hash = hex::encode(hasher.finalize());
                if existing_hash.eq_ignore_ascii_case(&hash) {
                    return Ok(hash);
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Concurrent artifact collision with hash mismatch for {}", hash),
                    ));
                }
            } else {
                return Err(e);
            }
        }

        Ok(hash)
    }

    pub fn get_artifact(&self, hash: &str) -> Result<Vec<u8>, io::Error> {
        let mut file = self.open_artifact(hash)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;

        // Verify content SHA-256 matches expected hash (corruption / tampering guard)
        let mut hasher = Sha256::new();
        hasher.update(&buf);
        let actual_hash = hex::encode(hasher.finalize());
        if !actual_hash.eq_ignore_ascii_case(hash) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Artifact corruption detected: expected hash {} but content has {}", hash, actual_hash),
            ));
        }

        Ok(buf)
    }

    /// Read bounded range from an artifact with offset and limit using streaming and seeking
    pub fn get_artifact_range(&self, hash: &str, offset: usize, limit: usize) -> Result<Vec<u8>, io::Error> {
        let (bytes, _) = self.get_artifact_range_with_total(hash, offset, limit)?;
        Ok(bytes)
    }

    /// Read bounded range from an artifact with offset and limit using streaming and seeking, returning slice and total file size
    pub fn get_artifact_range_with_total(
        &self,
        hash: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<u8>, usize), io::Error> {
        use std::io::Seek;
        let mut file = self.open_artifact(hash)?;

        // Stream and verify SHA-256 to ensure file integrity without loading full file into memory
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 8192];
        let mut total_bytes = 0usize;
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            total_bytes += n;
            hasher.update(&buffer[..n]);
        }
        let actual_hash = hex::encode(hasher.finalize());
        if !actual_hash.eq_ignore_ascii_case(hash) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Artifact corruption detected: expected hash {} but content has {}", hash, actual_hash),
            ));
        }

        if offset >= total_bytes {
            return Ok((Vec::new(), total_bytes));
        }

        file.seek(io::SeekFrom::Start(offset as u64))?;
        let to_read = limit.min(total_bytes - offset);
        let mut result = vec![0u8; to_read];
        file.read_exact(&mut result)?;
        Ok((result, total_bytes))
    }

    pub fn has_artifact(&self, hash: &str) -> bool {
        self.get_artifact_range_with_total(hash, 0, 0).is_ok()
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
        assert_eq!(store.save_artifact(data).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_artifact_missing() {
        let dir = tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();

        let res = store.get_artifact("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn test_artifact_range_and_concurrent_writes() {
        let dir = tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();

        let data = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let hash1 = store.save_artifact(data).unwrap();
        // Concurrent identical save should succeed and return the same hash
        let hash2 = store.save_artifact(data).unwrap();
        assert_eq!(hash1, hash2);

        // Bounded range read
        let slice = store.get_artifact_range(&hash1, 10, 6).unwrap();
        assert_eq!(slice, b"abcdef");

        // Range read beyond file len
        let empty_slice = store.get_artifact_range(&hash1, 100, 10).unwrap();
        assert!(empty_slice.is_empty());

        // Range read tampering detection
        let shard = &hash1[..2];
        let file_path = dir.path().join(shard).join(&hash1);
        fs::write(&file_path, b"Tampered data for range").unwrap();
        let err = store.get_artifact_range(&hash1, 0, 5);
        assert!(err.is_err());
        assert_eq!(err.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
