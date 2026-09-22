/// In-app update: query GitHub latest Release → compare version → download installer → launch install.
///
/// Silent principle: any failure on the auto-check path (offline, rate-limited, parse failure, no installer for this platform)
/// is just "no update" — better to miss than to bother the user; only "user clicked update but install failed"
/// gets a real error reported. This module does not depend on tauri (pure functions + std + HTTP); event and thread orchestration lives in
/// main.rs.
///
/// Installer matching depends on CI artifact naming (build.yml): Windows `*_x64-setup.exe`,
/// macOS Intel `*_x64.dmg` / Apple Silicon `*_aarch64.dmg` — renaming/adding an arch must
/// be synced with `pick_asset` (key-rules #12).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// GitHub repository (matches git remote and CI Release)
pub const REPO: &str = "Masterchiefm/zcode-speed-panel";

/// Successfully parsed latest Release (only the installer for this platform is kept)
#[derive(Clone, Debug)]
pub struct Release {
    /// Release tag, e.g. "v0.3.0"
    pub tag: String,
    /// Version number without the v prefix, e.g. "0.3.0"
    pub version: String,
    /// Release page URL (opened in system browser for "release notes")
    pub url: String,
    /// Release body (changelog)
    pub notes: String,
    /// Installer file name (includes version, naturally avoids collision with old versions)
    pub asset_name: String,
    /// Installer download URL (302 redirects followed automatically)
    pub asset_url: String,
    /// Installer byte size reported by the API (fallback for progress denominator)
    pub asset_size: u64,
}

/// "v0.3.1" / "0.3.1" → (0, 3, 1). v prefix is optional; `-rc.1` pre-release suffix and
/// `+build` metadata are ignored (this project does not ship pre-releases); any non-numeric segment or more than three segments → None
pub fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let core = tag.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next()?;
    let mut it = core.split('.');
    let maj = it.next()?.parse().ok()?;
    let min = it.next().unwrap_or("0").parse().ok()?;
    let pat = it.next().unwrap_or("0").parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((maj, min, pat))
}

/// Whether candidate is newer than current. Either side failing to parse → false: an unparseable tag
/// must not be treated as a new version to lure the user into "updating"
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

/// Platform key for installer matching (unsupported platform always yields "no update")
pub fn platform() -> &'static str {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "win-x64"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "mac-aarch64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "mac-x64"
    } else {
        "unsupported"
    }
}

/// Pick this platform's installer from asset file names (CI naming convention): win-x64 → `*_x64-setup.exe`
/// (NSIS installer; portable edition is not auto-installed); mac-x64 → `*_x64.dmg`;
/// mac-aarch64 → `*_aarch64.dmg`. `_x64.dmg` will not be confused with `_aarch64.dmg`
/// (the char before x64 is 'h', not underscore), so a single suffix match is sufficient
pub fn pick_asset(assets: &[String], platform: &str) -> Option<usize> {
    let suffix = match platform {
        "win-x64" => "_x64-setup.exe",
        "mac-x64" => "_x64.dmg",
        "mac-aarch64" => "_aarch64.dmg",
        _ => return None,
    };
    assets.iter().position(|n| n.ends_with(suffix))
}

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(60))
        .build()
}

/// Fetch the latest Release (the GitHub API latest endpoint naturally excludes drafts/pre-releases).
/// Any failure (offline, 403 rate-limited, parse failure, no installer for this platform) → None, silently
pub fn fetch_latest(user_agent: &str) -> Option<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = http_agent()
        .get(&url)
        .set("User-Agent", user_agent) // API rejects requests without a User-Agent outright
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(15))
        .call()
        .ok()?;
    let v: serde_json::Value = resp.into_json().ok()?;
    let tag = v.get("tag_name")?.as_str()?.trim().to_string();
    let html_url = v.get("html_url")?.as_str()?.to_string();
    let notes = v.get("body").and_then(|b| b.as_str()).unwrap_or("").trim().to_string();
    let assets = v.get("assets")?.as_array()?;
    // Only keep assets with a name (theoretically all have one), ensuring pick_asset's index aligns with the list
    let named: Vec<(String, &serde_json::Value)> = assets
        .iter()
        .filter_map(|a| {
            let n = a.get("name").and_then(|n| n.as_str())?;
            Some((n.to_string(), a))
        })
        .collect();
    let names: Vec<String> = named.iter().map(|(n, _)| n.clone()).collect();
    let idx = pick_asset(&names, platform())?;
    let (asset_name, asset) = &named[idx];
    Some(Release {
        version: tag.trim_start_matches(['v', 'V']).to_string(),
        asset_url: asset.get("browser_download_url")?.as_str()?.to_string(),
        asset_size: asset.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
        asset_name: asset_name.clone(),
        tag,
        url: html_url,
        notes,
    })
}

/// Download the installer to the system temp dir (`zcode-speed-panel-update/<asset name>`), invoking
/// on_progress(bytes downloaded, total bytes [0 if unknown]). Total bytes prefer the final post-redirect
/// Content-Length header, falling back to the API-reported size; on failure the partial file is deleted
pub fn download(rel: &Release, user_agent: &str, on_progress: &mut dyn FnMut(u64, u64)) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("zcode-speed-panel-update");
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create download directory: {e}"))?;
    let path = dir.join(&rel.asset_name);
    let resp = http_agent()
        .get(&rel.asset_url)
        .set("User-Agent", user_agent)
        .call()
        .map_err(|e| format!("Download failed: {e}"))?;
    let total = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&t| t > 0)
        .unwrap_or(rel.asset_size);
    let mut reader = resp.into_reader();
    let mut file = fs::File::create(&path).map_err(|e| fail(&path, format!("Failed to create file: {e}")))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| fail(&path, format!("Download interrupted: {e}")))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| fail(&path, format!("Failed to write file: {e}")))?;
        written += n as u64;
        on_progress(written, total);
    }
    file.flush().map_err(|e| fail(&path, format!("Failed to flush to disk: {e}")))?;
    if written == 0 {
        return Err(fail(&path, "Downloaded content is empty".into()));
    }
    if total > 0 && written != total {
        return Err(fail(&path, format!("Download incomplete ({written}/{total} bytes)")));
    }
    Ok(path)
}

/// Delete the partial artifact and return the error message (used by map_err for unified cleanup)
fn fail(path: &Path, msg: String) -> String {
    let _ = fs::remove_file(path);
    msg
}

/// Launch the installer. Windows: run the NSIS installer directly (caller then exits the app to hand over);
/// macOS: `open` mounts the dmg for the user to drag into Applications (app does not exit, old version keeps
/// running until the user restarts). Other platforms theoretically never reach here (platform() already returns unsupported)
pub fn launch_installer(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed to launch installer: {e}"))
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed to open disk image: {e}"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = path;
        Err("Auto-install is not supported on this platform".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing() {
        assert_eq!(parse_version("v0.3.1"), Some((0, 3, 1)));
        assert_eq!(parse_version("0.3.1"), Some((0, 3, 1)));
        assert_eq!(parse_version("V1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v0.3.1-rc.1"), Some((0, 3, 1))); // pre-release suffix ignored
        assert_eq!(parse_version("v0.3.1+build.2"), Some((0, 3, 1)));
        assert_eq!(parse_version("v10.0.0"), Some((10, 0, 0)));
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn version_ordering() {
        assert!(is_newer("v0.3.0", "0.2.1"));
        assert!(is_newer("v0.2.2", "v0.2.1"));
        assert!(is_newer("v1.0.0", "v0.99.99"));
        assert!(!is_newer("v0.2.1", "0.2.1")); // equal is not an update
        assert!(!is_newer("v0.2.0", "v0.2.1"));
        assert!(!is_newer("garbage", "0.2.1")); // parse failure: better to miss than to false-report
        assert!(!is_newer("0.2.1", "garbage"));
    }

    #[test]
    fn asset_picking() {
        let assets = vec![
            "zcode-speed-panel_0.3.0_x64-setup.exe".to_string(),
            "zcode-speed-panel_0.3.0_x64-portable.exe".to_string(),
            "zcode-speed-panel_0.3.0_x64.dmg".to_string(),
            "zcode-speed-panel_0.3.0_aarch64.dmg".to_string(),
        ];
        assert_eq!(pick_asset(&assets, "win-x64"), Some(0)); // installer preferred, portable not considered
        assert_eq!(pick_asset(&assets, "mac-x64"), Some(2));
        assert_eq!(pick_asset(&assets, "mac-aarch64"), Some(3));
        assert_eq!(pick_asset(&assets, "unsupported"), None);
        // with only one artifact, must not cross-match architectures
        let only_arm = vec!["zcode-speed-panel_0.3.0_aarch64.dmg".to_string()];
        assert_eq!(pick_asset(&only_arm, "mac-x64"), None);
        assert_eq!(pick_asset(&only_arm, "mac-aarch64"), Some(0));
    }
}
