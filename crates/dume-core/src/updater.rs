use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub tag_name: String,
    pub release_notes: String,
    pub asset_name: String,
    pub download_url: String,
    pub checksum_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    body: Option<String>,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// Detects the current native platform identifier matching the release archive matrix.
pub fn detect_platform() -> Result<&'static str> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    match (os, arch) {
        ("macos", "aarch64") => Ok("darwin-arm64"),
        ("macos", "x86_64") => Ok("darwin-x64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        ("linux", "x86_64") => Ok("linux-x64"),
        ("windows", "aarch64") => Ok("windows-arm64"),
        ("windows", "x86_64") => Ok("windows-x64"),
        _ => anyhow::bail!("Unsupported platform: {}-{}", os, arch),
    }
}

/// Compares two semver strings (with or without 'v' prefix) and returns true if `latest` is strictly newer than `current`.
pub fn is_newer_version(current: &str, latest: &str) -> bool {
    let parse_semver = |v: &str| -> Option<(u64, u64, u64)> {
        let clean = v.trim().trim_start_matches('v');
        let parts: Vec<&str> = clean.split('.').collect();
        if parts.len() < 3 {
            return None;
        }
        let major = parts[0].parse::<u64>().ok()?;
        let minor = parts[1].parse::<u64>().ok()?;
        // Handle any pre-release suffix like 0-rc1
        let patch_str = parts[2].split('-').next().unwrap_or(parts[2]);
        let patch = patch_str.parse::<u64>().ok()?;
        Some((major, minor, patch))
    };

    match (parse_semver(current), parse_semver(latest)) {
        (Some((c_maj, c_min, c_pat)), Some((l_maj, l_min, l_pat))) => {
            (l_maj, l_min, l_pat) > (c_maj, c_min, c_pat)
        }
        _ => false,
    }
}

/// Resolves an ambient GitHub token from environment variables or the `gh` CLI.
pub fn resolve_ambient_github_token() -> Option<String> {
    if let Ok(token) = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    // Fall back to local `gh auth token` if available
    let output = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;

    if output.status.success() {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !token.is_empty() {
            return Some(token);
        }
    }

    None
}

/// Checks GitHub Releases for updates without blocking or stalling if network fails.
pub fn check_for_update(repo: &str, current_version: &str) -> Result<Option<UpdateInfo>> {
    let platform = detect_platform()?;
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo);

    // Use curl CLI for robust, zero-dependency TLS/HTTP request with timeout
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sSL",
        "-m",
        "5", // 5 seconds max timeout so it never hangs TUI or CLI
        "-H",
        "Accept: application/vnd.github.v3+json",
        "-H",
        "User-Agent: dume-updater",
    ]);

    if let Some(token) = resolve_ambient_github_token() {
        cmd.args(["-H", &format!("Authorization: Bearer {}", token)]);
    }

    cmd.arg(&url);

    let output = match cmd.output() {
        Ok(out) if out.status.success() => out.stdout,
        _ => return Ok(None),
    };

    let release: GitHubRelease = match serde_json::from_slice(&output) {
        Ok(rel) => rel,
        Err(_) => return Ok(None),
    };

    let latest_tag = &release.tag_name;
    if !is_newer_version(current_version, latest_tag) {
        return Ok(None);
    }

    // Expected archive name format:
    // dume-darwin-arm64.tar.gz or dume-windows-x64.zip
    let ext = if platform.starts_with("windows-") {
        ".zip"
    } else {
        ".tar.gz"
    };
    let expected_asset_name = format!("dume-{}{}", platform, ext);

    let mut download_url = None;
    let mut checksum_url = None;

    for asset in &release.assets {
        if asset.name == expected_asset_name {
            download_url = Some(asset.browser_download_url.clone());
        } else if asset.name == "SHA256SUMS" {
            checksum_url = Some(asset.browser_download_url.clone());
        }
    }

    let download_url = match download_url {
        Some(u) => u,
        None => return Ok(None),
    };

    Ok(Some(UpdateInfo {
        current_version: current_version.to_string(),
        latest_version: latest_tag.trim_start_matches('v').to_string(),
        tag_name: latest_tag.clone(),
        release_notes: release.body.unwrap_or_default(),
        asset_name: expected_asset_name,
        download_url,
        checksum_url,
    }))
}

/// Download a file to disk with curl
fn download_file(url: &str, dest: &Path) -> Result<()> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sSL",
        "-f",
        "-m",
        "120", // 2 minutes max for binary download
        "-H",
        "User-Agent: dume-updater",
    ]);

    if let Some(token) = resolve_ambient_github_token() {
        cmd.args(["-H", &format!("Authorization: Bearer {}", token)]);
    }

    cmd.args(["-o", dest.to_str().context("Invalid destination path")?, url]);

    let status = cmd.status().context("Failed to spawn curl for download")?;
    anyhow::ensure!(status.success(), "Download failed for URL: {}", url);
    Ok(())
}

/// Verifies that the file at `file_path` matches the SHA256 checksum recorded in SHA256SUMS content
pub fn verify_sha256(file_path: &Path, file_name: &str, sha256sums_content: &str) -> Result<()> {
    let mut expected_hash = None;
    for line in sha256sums_content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() >= 2 {
            let hash = parts[0];
            let name = parts[1].trim_start_matches("./").trim_start_matches('*');
            if name == file_name {
                expected_hash = Some(hash.to_lowercase());
                break;
            }
        }
    }

    let expected = expected_hash.context("Archive not found in SHA256SUMS")?;

    let file_bytes = std::fs::read(file_path).context("Failed to read downloaded archive")?;
    let mut hasher = Sha256::new();
    hasher.update(&file_bytes);
    let actual = hex::encode(hasher.finalize()).to_lowercase();

    anyhow::ensure!(
        actual == expected,
        "SHA-256 checksum mismatch! Expected {}, got {}",
        expected,
        actual
    );
    Ok(())
}

/// Extracts the binary named `dume` (or `dume.exe`) from the archive into `out_binary_path`
pub fn extract_binary(archive_path: &Path, out_binary_path: &Path) -> Result<()> {
    let temp_extract_dir = archive_path
        .parent()
        .context("Missing parent")?
        .join("extracted");
    let _ = std::fs::remove_dir_all(&temp_extract_dir);
    std::fs::create_dir_all(&temp_extract_dir)?;

    let archive_name = archive_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    if archive_name.ends_with(".zip") {
        let mut cmd = Command::new("tar");
        // Windows and macOS include tar or unzip
        cmd.args([
            "-xf",
            archive_path.to_str().unwrap(),
            "-C",
            temp_extract_dir.to_str().unwrap(),
        ]);
        let status = cmd.status().context("Failed to unpack zip archive")?;
        anyhow::ensure!(status.success(), "Failed to unpack zip archive");
    } else {
        let mut cmd = Command::new("tar");
        cmd.args([
            "-xzf",
            archive_path.to_str().unwrap(),
            "-C",
            temp_extract_dir.to_str().unwrap(),
        ]);
        let status = cmd.status().context("Failed to unpack tar.gz archive")?;
        anyhow::ensure!(status.success(), "Failed to unpack tar.gz archive");
    }

    // Inside the extracted archive, the directory is `dume/dume` or `dume/dume.exe`
    let binary_name = if cfg!(windows) { "dume.exe" } else { "dume" };
    let candidate1 = temp_extract_dir.join("dume").join(binary_name);
    let candidate2 = temp_extract_dir.join(binary_name);

    let found_binary = if candidate1.is_file() {
        candidate1
    } else if candidate2.is_file() {
        candidate2
    } else {
        // Fallback: search anywhere in temp_extract_dir
        let mut found = None;
        for entry in std::fs::read_dir(&temp_extract_dir)? {
            if let Ok(entry) = entry {
                let p = entry.path();
                if p.is_dir() {
                    let sub = p.join(binary_name);
                    if sub.is_file() {
                        found = Some(sub);
                        break;
                    }
                } else if p.file_name().and_then(|s| s.to_str()) == Some(binary_name) {
                    found = Some(p);
                    break;
                }
            }
        }
        found.context("Could not find dume binary inside extracted archive")?
    };

    // Ensure executable permission on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&found_binary)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&found_binary, perms)?;
    }

    std::fs::copy(&found_binary, out_binary_path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(out_binary_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(out_binary_path, perms)?;
    }

    Ok(())
}

/// Safely performs an atomic in-place replacement of the currently running executable.
/// - On POSIX (macOS/Linux): A file can be atomically replaced by `rename` even while executing
///   because the existing open inode remains mapped for the running process until exit.
/// - On Windows: A running executable cannot be written to, but it can be renamed.
///   We rename `dume.exe` to `dume.exe.old`, then move the new `dume.exe` in place.
pub fn replace_current_executable(new_binary_path: &Path) -> Result<PathBuf> {
    let current_exe = std::env::current_exe().context("Failed to locate current executable path")?;
    let current_dir = current_exe
        .parent()
        .context("Missing parent directory for current executable")?;

    // Target executable path
    let target_path = current_exe.clone();

    // Stage temporary file in the same directory/filesystem to ensure atomic rename
    let staged_new = current_dir.join(format!(".dume-update-new-{}", std::process::id()));
    std::fs::copy(new_binary_path, &staged_new)
        .context("Failed to stage new binary into destination directory")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&staged_new)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&staged_new, perms)?;
    }

    #[cfg(windows)]
    {
        let old_backup = current_dir.join(format!("dume.exe.old-{}", std::process::id()));
        // Rename current running exe to .old
        let _ = std::fs::rename(&target_path, &old_backup);
        // Rename new staged binary to target path
        if let Err(e) = std::fs::rename(&staged_new, &target_path) {
            // Rollback on failure
            let _ = std::fs::rename(&old_backup, &target_path);
            return Err(e).context("Failed to rename new binary to target exe");
        }
        let _ = std::fs::remove_file(&old_backup);
    }

    #[cfg(not(windows))]
    {
        // On Unix, atomic rename directly replaces the directory entry
        std::fs::rename(&staged_new, &target_path)
            .context("Failed to atomically replace current executable")?;
    }

    Ok(target_path)
}

/// Applies the in-place update using the provided `UpdateInfo`.
pub fn apply_update<F>(info: &UpdateInfo, on_progress: F) -> Result<PathBuf>
where
    F: Fn(&str),
{
    on_progress("Creating temporary workspace...");
    let temp_dir = tempfile::Builder::new()
        .prefix("dume-update-")
        .tempdir()
        .context("Failed to create temporary directory for update")?;

    let archive_path = temp_dir.path().join(&info.asset_name);

    on_progress(&format!("Downloading {}...", info.asset_name));
    download_file(&info.download_url, &archive_path)?;

    if let Some(checksum_url) = &info.checksum_url {
        on_progress("Downloading and verifying SHA256SUMS...");
        let sums_path = temp_dir.path().join("SHA256SUMS");
        download_file(checksum_url, &sums_path)?;
        let sums_content = std::fs::read_to_string(&sums_path)?;
        verify_sha256(&archive_path, &info.asset_name, &sums_content)?;
    }

    on_progress("Extracting new binary...");
    let extracted_binary = temp_dir.path().join(if cfg!(windows) { "dume.exe" } else { "dume" });
    extract_binary(&archive_path, &extracted_binary)?;

    on_progress("Replacing active executable (atomic hot-swap)...");
    let replaced_path = replace_current_executable(&extracted_binary)?;

    on_progress("Update completed successfully!");
    Ok(replaced_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer_version() {
        assert!(is_newer_version("0.1.0", "0.1.1"));
        assert!(is_newer_version("0.1.0", "v0.1.1"));
        assert!(is_newer_version("v0.1.0", "v0.2.0"));
        assert!(is_newer_version("0.1.5", "1.0.0"));
        assert!(!is_newer_version("0.1.1", "0.1.1"));
        assert!(!is_newer_version("0.2.0", "0.1.9"));
        assert!(!is_newer_version("1.0.0", "0.9.9"));
    }

    #[test]
    fn test_detect_platform() {
        let p = detect_platform();
        assert!(p.is_ok());
        let val = p.unwrap();
        assert!(
            val == "darwin-arm64"
                || val == "darwin-x64"
                || val == "linux-arm64"
                || val == "linux-x64"
                || val == "windows-arm64"
                || val == "windows-x64"
        );
    }

    #[test]
    fn test_verify_sha256() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("dume-darwin-arm64.tar.gz");
        std::fs::write(&test_file, b"test binary payload content").unwrap();

        let mut hasher = Sha256::new();
        hasher.update(b"test binary payload content");
        let hash = hex::encode(hasher.finalize());

        let sha256sums_content = format!("{}  dume-darwin-arm64.tar.gz\n", hash);
        let res = verify_sha256(&test_file, "dume-darwin-arm64.tar.gz", &sha256sums_content);
        assert!(res.is_ok());

        let bad_sha256sums = "0123456789abcdef  dume-darwin-arm64.tar.gz\n";
        let bad_res = verify_sha256(&test_file, "dume-darwin-arm64.tar.gz", bad_sha256sums);
        assert!(bad_res.is_err());
    }
}
