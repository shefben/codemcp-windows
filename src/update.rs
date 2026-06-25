//! Version update checking and self-updating.

use std::process;

use serde::Deserialize;

use crate::error::Error;

/// GitHub release info returned by the API.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: Option<String>,
}

/// Information about the latest release.
#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    /// Version string without leading `v` (e.g. `"0.5.1"`).
    pub version: String,
    /// GitHub releases page URL.
    pub html_url: Option<String>,
}

/// User-Agent sent with every GitHub request. The GitHub API rejects requests
/// without a User-Agent header (HTTP 403), so this is mandatory.
const USER_AGENT: &str = concat!("codemcp/", env!("CARGO_PKG_VERSION"));

/// Query the GitHub API for the latest release of codemcp.
pub async fn check_latest() -> Result<ReleaseInfo, Error> {
    let resp = reqwest::Client::new()
        .get("https://api.github.com/repos/skymoore/codemcp/releases/latest")
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .map_err(|e| Error::Other(format!("failed to query GitHub API: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "GitHub API returned status {}",
            resp.status()
        )));
    }

    let release: Release = resp.json().await.map_err(|e| Error::Other(format!("invalid API response: {e}")))?;
    let version = release.tag_name.strip_prefix('v').unwrap_or(&release.tag_name).to_string();

    Ok(ReleaseInfo {
        version,
        html_url: release.html_url,
    })
}

/// Compare the installed version string with the latest version string.
/// Returns true if `installed` is strictly older than `latest`.
pub fn is_outdated(installed: &str, latest: &str) -> bool {
    let inst_parts = version_parts(installed);
    let lat_parts = version_parts(latest);

    for (i, l) in inst_parts.iter().zip(lat_parts.iter()) {
        if l > i {
            return true;
        }
        if i > l {
            return false;
        }
    }

    // Installed has fewer components (e.g. "0.5" vs "0.5.1") — treat as outdated
    lat_parts.len() > inst_parts.len()
}

fn version_parts(v: &str) -> Vec<u64> {
    v.split('.')
        .filter_map(|p| p.parse::<u64>().ok())
        .collect()
}

/// Map Rust's `std::env::consts::OS` to the release asset OS token.
///
/// Release assets use `darwin`/`linux`; Rust reports `macos`/`linux`.
fn asset_os() -> Result<&'static str, Error> {
    match std::env::consts::OS {
        "macos" => Ok("darwin"),
        "linux" => Ok("linux"),
        other => Err(Error::Other(format!(
            "unsupported OS for self-update: {other} (supported: macOS, Linux)"
        ))),
    }
}

/// Map Rust's `std::env::consts::ARCH` to the release asset arch token.
///
/// Release assets use `arm64`/`x86_64`; Rust reports `aarch64`/`x86_64`.
fn asset_arch() -> Result<&'static str, Error> {
    match std::env::consts::ARCH {
        "aarch64" => Ok("arm64"),
        "x86_64" => Ok("x86_64"),
        other => Err(Error::Other(format!(
            "unsupported architecture for self-update: {other} (supported: arm64, x86_64)"
        ))),
    }
}

/// Download and install the latest version of codemcp, replacing the current binary.
pub async fn update() -> Result<(), Error> {
    let os = asset_os()?;
    let arch = asset_arch()?;

    // Release a tarball at: https://github.com/skymoore/codemcp/releases/download/<tag>/codemcp-<os>-<arch>.tar.gz
    let latest = check_latest().await?;

    // Check if already up to date.
    let current = env!("CARGO_PKG_VERSION");
    if !is_outdated(current, &latest.version) {
        println!("codemcp is already up to date ({}).", current);
        return Ok(());
    }

    println!("Updating codemcp from {} to {}…", current, latest.version);
    println!();

    let asset = format!("codemcp-{os}-{arch}.tar.gz");
    // Strip `v` from tag for the URL.
    let tag = latest.version.strip_prefix('v').unwrap_or(&latest.version);
    let url = format!("https://github.com/skymoore/codemcp/releases/download/v{tag}/{asset}");
    let sum_url = format!("{url}.sha256");

    // Download the release tarball.
    let data = reqwest::Client::new()
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .map_err(|e| Error::Other(format!("failed to download: {url}: {e}")))?
        .bytes()
        .await
        .map_err(|e| Error::Other(format!("failed reading response: {e}")))?;

    // Verify checksum if available (best-effort).
    let client = reqwest::Client::new();

    if let Ok(sum_resp) = client
        .get(&sum_url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
    {
        if sum_resp.status().is_success() {
            let expected_sum = sum_resp.text().await.unwrap_or_default();
            let expected_sum: String = expected_sum.split_whitespace().next().unwrap_or("").to_string();

            if !expected_sum.is_empty() {
                let actual_sum = sha256(&data);
                if actual_sum != expected_sum {
                    println!("warning: checksum mismatch (expected {}, got {}); update aborted for safety", expected_sum, actual_sum);
                    return Err(Error::Other("checksum verification failed".into()));
                }
                } else {
                    // no published checksum, skip verification (same as install.sh)
                }
            } else {
                // no .sha256 file, skip verification (same as install.sh)
            }

        } else {
            // no .sha256 file, skip verification (same as install.sh)
        }

    println!("checksum ok");

    // Find the current binary path.
    let current_exe = std::env::current_exe().map_err(|e| Error::Other(format!("cannot determine current binary path: {e}")))?;

    // Extract the tarball into a temp directory.
    let tmp = std::env::temp_dir().join(format!("codemcp-update-{}", process::id()));
    std::fs::create_dir_all(&tmp)?;

    let archive = flate2::read::GzDecoder::new(&data[..]);
    let mut archive = tar::Archive::new(archive);
    archive.unpack(&tmp)?;

    // The tarball contains the binary named after the asset (e.g.
    // `codemcp-darwin-arm64`). Fall back to a bare `codemcp` for safety.
    let asset_stem = format!("codemcp-{os}-{arch}");
    let extracted = if tmp.join(&asset_stem).exists() {
        tmp.join(&asset_stem)
    } else {
        tmp.join("codemcp")
    };

    if !extracted.exists() {
        return Err(Error::Other("binary not found inside tarball".into()));
    }

    // Make it executable.
    #[cfg(unix)]
    std::fs::set_permissions(&extracted, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;

    // Atomic replace: copy to a temp file in the same directory, then rename.
    let install_dir = current_exe.parent().ok_or_else(|| Error::Other("no parent directory".into()))?;
    let dest = install_dir.join("codemcp.new");

    std::fs::copy(&extracted, &dest)?;
    std::fs::rename(&dest, &current_exe).map_err(|_| {
        // Fallback: try with sudo if the rename fails (permission-denied on target).
        Error::Other(format!(
            "failed to replace binary – is '{}' writable? Try: sudo codemcp update",
            current_exe.display()
        ))
    })?;

    // Clean up temp.
    let _ = std::fs::remove_dir_all(&tmp);

    println!("Updated codemcp {} -> {}", current, latest.version);
    Ok(())
}

fn sha256(data: &[u8]) -> String {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    result.as_slice().iter().map(|b| format!("{b:02x}")).collect()
}
