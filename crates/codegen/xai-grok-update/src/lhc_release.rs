//! Fork release identity and managed-store ownership for the liminal-ai
//! **grok-build-lhc** build. Fork-owned file: upstream never touches it.
//!
//! The native `xai_grok_version::VERSION` stays the upstream base (it goes out
//! in protocol headers and feeds every stock semver consumer). The fork's own
//! release identity lives here: `lhc-release/VERSION`, `<base>` for fork
//! revision 0 or `<base>-lhc.<n>` for a repair of the same base, ordered as
//! the source tuple plus the fork revision. Only the fork's release discovery,
//! artifacts, and store receipts use it.
//!
//! Ownership follows the Codex-LHC pattern: the running executable is
//! *managed* only when it lives in `<store>/versions/<release>/bin/grok[.exe]`
//! and the store carries the installer's marker and receipts. No PATH or
//! wrapper scanning; anything else is an unmanaged LHC build that gets
//! managed-installer guidance instead of a stock update path.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Fork release identity embedded at build time from `lhc-release/VERSION`.
pub const LHC_RELEASE_VERSION: &str = include_str!("../../../../lhc-release/VERSION").trim_ascii();

/// GitHub repository that publishes fork releases.
pub const LHC_RELEASE_REPO: &str = "liminal-ai/grok-build-lhc";
/// Unauthenticated latest-release lookup (`tag_name`).
pub const LHC_LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/liminal-ai/grok-build-lhc/releases/latest";
/// Public asset download base: `<base>/v<release>/<asset>`.
pub const LHC_RELEASE_DOWNLOAD_BASE: &str =
    "https://github.com/liminal-ai/grok-build-lhc/releases/download";
/// Human-facing release page.
pub const LHC_RELEASES_URL: &str = "https://github.com/liminal-ai/grok-build-lhc/releases/latest";
/// Always-latest installer asset.
pub const LHC_INSTALLER_URL: &str =
    "https://github.com/liminal-ai/grok-build-lhc/releases/latest/download/install.sh";
/// Install documentation.
pub const LHC_INSTALL_DOCS_URL: &str =
    "https://github.com/liminal-ai/grok-build-lhc/blob/lhc/lhc-docs/INSTALL.md";

/// Loopback-only test transport: `<base>/latest` answers the latest-release
/// JSON and `<base>/download/v<release>/<asset>` serves assets. The installer
/// honours the same variable (it is inherited by the child), so one setting
/// points both halves at a local server. Non-loopback values are ignored.
pub const LHC_RELEASE_BASE_ENV: &str = "GROK_LHC_RELEASE_BASE";

/// Installer kind reported by `get_installer` for a managed store.
pub const INSTALLER_LHC_MANAGED: &str = "lhc-managed";
/// Installer kind for an LHC build that is not running from a managed store.
pub const INSTALLER_LHC_UNMANAGED: &str = "lhc-unmanaged";

/// The one shell installer, embedded so the running build installs a release
/// with the installer it was qualified with (no unverified script download).
pub const INSTALLER_SCRIPT: &str = include_str!("../../../../scripts/grok-lhc-release/install.sh");

/// Parse `<major>.<minor>.<patch>[-lhc.<revision>]`; a bare base is revision 0.
/// Anything else (stock pre-releases, garbage) is `None`.
pub fn parse_lhc_release(release: &str) -> Option<(u64, u64, u64, u64)> {
    let release = release.trim();
    let (base, revision) = match release.split_once("-lhc.") {
        Some((base, revision)) => (base, parse_component(revision)?),
        None => (release, 0),
    };
    let mut parts = base.split('.');
    let major = parse_component(parts.next()?)?;
    let minor = parse_component(parts.next()?)?;
    let patch = parse_component(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch, revision))
}

fn parse_component(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// The upstream base a fork release was built from (`1.0.16-lhc.2` -> `1.0.16`).
pub fn lhc_release_base(release: &str) -> Option<String> {
    let (major, minor, patch, _) = parse_lhc_release(release)?;
    Some(format!("{major}.{minor}.{patch}"))
}

/// `Some(true)` when `candidate` orders after `current` as a fork release.
pub fn lhc_release_is_newer(candidate: &str, current: &str) -> Option<bool> {
    Some(parse_lhc_release(candidate)? > parse_lhc_release(current)?)
}

/// A managed store the running executable belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LhcManagedInstall {
    /// Store root (`~/.local/share/grok-lhc` by default).
    pub store: PathBuf,
    /// `installed-version` receipt: the release currently activated on disk,
    /// which may already be newer than the running process.
    pub release: String,
    /// `installed-name` receipt: the command the installer links.
    pub name: String,
}

fn managed_bin_name() -> &'static str {
    if cfg!(windows) { "grok.exe" } else { "grok" }
}

impl LhcManagedInstall {
    /// The store's activated binary; what a restart must exec.
    pub fn current_bin(&self) -> PathBuf {
        self.store
            .join("current")
            .join("bin")
            .join(managed_bin_name())
    }

    /// Store-owned update cache (never `~/.grok/version.json`).
    pub fn version_cache_path(&self) -> PathBuf {
        self.store.join("version.json")
    }
}

fn read_receipt(store: &Path, name: &str) -> Option<String> {
    let value = std::fs::read_to_string(store.join(name)).ok()?;
    let value = value.trim();
    if value.is_empty() || value.contains('/') || value.contains('\\') {
        return None;
    }
    Some(value.to_string())
}

/// Recognize the managed layout from an executable path:
/// `<store>/versions/<release>/bin/grok[.exe]` with `<store>/.grok-lhc-managed`,
/// `installed-name`, and `installed-version`. Both sides are canonicalized so
/// the command symlink and the versioned target classify alike. Any failure
/// reports unmanaged.
pub fn managed_install_for_exe(exe: &Path) -> Option<LhcManagedInstall> {
    let exe = dunce::canonicalize(exe).ok()?;
    if exe.file_name()?.to_str()? != managed_bin_name() {
        return None;
    }
    let bin = exe.parent()?;
    if bin.file_name()?.to_str()? != "bin" {
        return None;
    }
    let release_dir = bin.parent()?;
    let versions = release_dir.parent()?;
    if versions.file_name()?.to_str()? != "versions" {
        return None;
    }
    let store = versions.parent()?.to_path_buf();
    if !store.join(".grok-lhc-managed").is_file() {
        return None;
    }
    let name = read_receipt(&store, "installed-name")?;
    let release = read_receipt(&store, "installed-version")?;
    parse_lhc_release(&release)?;
    Some(LhcManagedInstall {
        store,
        release,
        name,
    })
}

/// The managed store of the running executable, if any.
pub fn managed_install() -> Option<LhcManagedInstall> {
    managed_install_for_exe(&std::env::current_exe().ok()?)
}

/// Installer kind for this build: managed store or not. Never a stock kind.
pub fn installer_kind() -> &'static str {
    if managed_install().is_some() {
        INSTALLER_LHC_MANAGED
    } else {
        INSTALLER_LHC_UNMANAGED
    }
}

fn release_base_override() -> Option<String> {
    let base = std::env::var(LHC_RELEASE_BASE_ENV).ok()?;
    let base = base.trim().trim_end_matches('/').to_string();
    if crate::version::is_loopback_base(&base) {
        Some(base)
    } else {
        if !base.is_empty() {
            tracing::warn!("{LHC_RELEASE_BASE_ENV} ignored: only loopback bases are honored");
        }
        None
    }
}

/// Latest published fork release (tag with the leading `v` removed), from the
/// unauthenticated GitHub API. No `gh` dependency.
pub async fn fetch_latest_release() -> Result<String> {
    let url = match release_base_override() {
        Some(base) => format!("{base}/latest"),
        None => LHC_LATEST_RELEASE_API_URL.to_string(),
    };
    let client = xai_grok_extra_ca::build_reqwest_client(|b| {
        b.timeout(std::time::Duration::from_secs(30))
            .user_agent(format!("grok-lhc/{LHC_RELEASE_VERSION}"))
    })?;
    let body: serde_json::Value = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("latest release lookup failed: {url}"))?
        .error_for_status()
        .with_context(|| format!("latest release lookup failed: {url}"))?
        .json()
        .await
        .context("latest release response is not JSON")?;
    let tag = body
        .get("tag_name")
        .and_then(|t| t.as_str())
        .context("latest release has no tag_name")?;
    let release = tag.strip_prefix('v').unwrap_or(tag).trim().to_string();
    parse_lhc_release(&release)
        .with_context(|| format!("latest release tag is not a fork release: {tag}"))?;
    Ok(release)
}

/// What to tell a user whose LHC build cannot update itself.
pub fn managed_installer_guidance() -> String {
    format!(
        "This grok-build-lhc build is not running from a managed store, so it does not update itself.\n\
         Install or reinstall with the fork installer (never the official x.ai install script):\n  \
         curl -fsSL {LHC_INSTALLER_URL} -o install.sh && sh install.sh --download\n\
         See {LHC_INSTALL_DOCS_URL}"
    )
}

/// Windows managed updates wait for the PowerShell installer (release slice).
pub fn windows_update_guidance() -> String {
    format!(
        "Windows managed update arrives with the next release; reinstall from the release page:\n  {LHC_RELEASES_URL}"
    )
}

/// Run the embedded installer against the managed store for `release`.
/// Name and prefix come from the store's receipts; nothing under `~/.grok` is
/// touched. Windows returns guidance instead of invoking a script that does
/// not exist yet.
pub async fn run_managed_installer(managed: &LhcManagedInstall, release: &str) -> Result<()> {
    if cfg!(windows) {
        anyhow::bail!("{}", windows_update_guidance());
    }
    parse_lhc_release(release).with_context(|| format!("not a fork release: {release}"))?;
    let script = std::env::temp_dir().join(format!(
        "grok-lhc-install-{}-{}.sh",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    tokio::fs::write(&script, INSTALLER_SCRIPT)
        .await
        .with_context(|| format!("cannot stage installer at {}", script.display()))?;
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg(&script)
        .arg("--download")
        .arg("--version")
        .arg(release)
        .arg("--install-root")
        .arg(&managed.store)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = cmd.output().await;
    let _ = tokio::fs::remove_file(&script).await;
    let output = output.context("cannot run the fork installer with sh")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!(
            "fork installer failed ({}): {}",
            output.status,
            stderr.trim().lines().last().unwrap_or("no output")
        );
    }
    for line in stdout.lines() {
        eprintln!("  {line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_identity_pairs_with_native_version() {
        // Identity pair: the fork release's base is exactly the native VERSION.
        let (major, minor, patch, _) =
            parse_lhc_release(LHC_RELEASE_VERSION).expect("fork release parses");
        assert_eq!(
            format!("{major}.{minor}.{patch}"),
            xai_grok_version::VERSION,
            "lhc-release/VERSION base must equal the native upstream VERSION"
        );
        assert_eq!(
            lhc_release_base(LHC_RELEASE_VERSION).as_deref(),
            Some(xai_grok_version::VERSION)
        );
    }

    #[test]
    fn parse_and_order_fork_releases() {
        assert_eq!(parse_lhc_release("1.0.16"), Some((1, 0, 16, 0)));
        assert_eq!(parse_lhc_release("1.0.16-lhc.3"), Some((1, 0, 16, 3)));
        assert_eq!(parse_lhc_release(" v"), None);
        assert_eq!(parse_lhc_release("1.0.16-alpha.1"), None);
        assert_eq!(parse_lhc_release("1.0"), None);
        assert_eq!(parse_lhc_release("1.0.16.1"), None);
        assert_eq!(parse_lhc_release("1.0.16-lhc."), None);
        assert_eq!(parse_lhc_release("1.0.16-lhc.x"), None);
        // Revision orders after its base; a newer base orders after any revision.
        assert_eq!(lhc_release_is_newer("1.0.16-lhc.1", "1.0.16"), Some(true));
        assert_eq!(lhc_release_is_newer("1.0.16", "1.0.16-lhc.1"), Some(false));
        assert_eq!(
            lhc_release_is_newer("1.0.16-lhc.2", "1.0.16-lhc.1"),
            Some(true)
        );
        assert_eq!(lhc_release_is_newer("1.0.17", "1.0.16-lhc.9"), Some(true));
        assert_eq!(lhc_release_is_newer("1.0.16", "1.0.16"), Some(false));
        assert_eq!(lhc_release_is_newer("0.3.1", "1.0.16"), Some(false));
        assert_eq!(lhc_release_is_newer("1.0.16-rc.1", "1.0.16"), None);
    }

    fn fake_store(root: &Path, release: &str, name: &str) -> PathBuf {
        let store = root.join("grok-lhc");
        let bin = store.join("versions").join(release).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(managed_bin_name()), b"binary").unwrap();
        std::fs::write(store.join(".grok-lhc-managed"), b"managed\n").unwrap();
        std::fs::write(store.join("installed-name"), format!("{name}\n")).unwrap();
        std::fs::write(store.join("installed-version"), format!("{release}\n")).unwrap();
        store
    }

    #[test]
    fn managed_detection_requires_layout_and_receipts() {
        let tmp = tempfile::tempdir().unwrap();
        let store = fake_store(tmp.path(), "1.0.16", "grok-lhc");
        let exe = store.join("versions/1.0.16/bin").join(managed_bin_name());
        let found = managed_install_for_exe(&exe).expect("managed");
        assert_eq!(found.store, dunce::canonicalize(&store).unwrap());
        assert_eq!(found.release, "1.0.16");
        assert_eq!(found.name, "grok-lhc");
        assert_eq!(
            found.current_bin(),
            found.store.join("current/bin").join(managed_bin_name())
        );
        assert_eq!(found.version_cache_path(), found.store.join("version.json"));

        // The receipt is disk truth: a newer activated release reports even from an older running dir.
        std::fs::write(store.join("installed-version"), "1.0.16-lhc.1\n").unwrap();
        assert_eq!(
            managed_install_for_exe(&exe).unwrap().release,
            "1.0.16-lhc.1"
        );

        // Missing marker, receipt, or the wrong layout -> unmanaged.
        std::fs::remove_file(store.join(".grok-lhc-managed")).unwrap();
        assert!(managed_install_for_exe(&exe).is_none());
        std::fs::write(store.join(".grok-lhc-managed"), b"managed\n").unwrap();
        std::fs::remove_file(store.join("installed-name")).unwrap();
        assert!(managed_install_for_exe(&exe).is_none());
        std::fs::write(store.join("installed-name"), b"grok-lhc\n").unwrap();
        std::fs::write(store.join("installed-version"), b"0.3.1-alpha\n").unwrap();
        assert!(
            managed_install_for_exe(&exe).is_none(),
            "receipt must be a fork release"
        );

        // Stock layout (~/.grok/bin/grok -> downloads/grok-<v>-<platform>) is never managed by us.
        let home = tmp.path().join("dotgrok");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::create_dir_all(home.join("downloads")).unwrap();
        std::fs::write(home.join("downloads/grok-1.0.13-linux-x86_64"), b"stock").unwrap();
        assert!(
            managed_install_for_exe(&home.join("downloads/grok-1.0.13-linux-x86_64")).is_none()
        );
        assert!(managed_install_for_exe(&tmp.path().join("missing")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn managed_detection_follows_the_command_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let store = fake_store(tmp.path(), "1.0.16", "grok");
        std::os::unix::fs::symlink(store.join("versions/1.0.16"), store.join("current")).unwrap();
        let prefix_bin = tmp.path().join("prefix/bin");
        std::fs::create_dir_all(&prefix_bin).unwrap();
        std::os::unix::fs::symlink(store.join("current/bin/grok"), prefix_bin.join("grok"))
            .unwrap();
        let found = managed_install_for_exe(&prefix_bin.join("grok")).expect("managed via link");
        assert_eq!(found.name, "grok");
        assert_eq!(found.store, dunce::canonicalize(&store).unwrap());
    }

    fn host_platform() -> &'static str {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => "linux-x86_64",
            ("linux", "aarch64") => "linux-aarch64",
            ("macos", "x86_64") => "darwin-x86_64",
            ("macos", "aarch64") => "darwin-aarch64",
            _ => "unsupported",
        }
    }

    /// Rust launches the embedded installer against the store: download mode over a
    /// loopback server, checksums, receipts (name/prefix reused), activation, old
    /// versions kept. Nothing outside the store and the recorded prefix is written.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn managed_update_runs_the_embedded_installer_against_the_store() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        if host_platform() == "unsupported" {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let prefix = tmp.path().join("custom-prefix");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        let store = fake_store(tmp.path(), "1.0.16", "grok-custom");
        std::os::unix::fs::symlink(store.join("versions/1.0.16"), store.join("current")).unwrap();
        std::os::unix::fs::symlink(
            store.join("current/bin/grok"),
            prefix.join("bin/grok-custom"),
        )
        .unwrap();
        std::fs::write(
            store.join("installed-prefix"),
            format!("{}\n", prefix.display()),
        )
        .unwrap();
        let dotgrok = tmp.path().join("dotgrok");
        std::fs::create_dir_all(&dotgrok).unwrap();

        let release = "1.0.16-lhc.1";
        let asset_name = format!("grok-{release}-{}", host_platform());
        let body = b"#!/bin/sh\nprintf 'updated\\n'\n".to_vec();
        let digest = format!("{}  {asset_name}\n", sha256_hex(&body));
        let manifest =
            format!("{{\"product\": \"grok-lhc\", \"release_version\": \"{release}\"}}\n");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"tag_name": format!("v{release}")})),
            )
            .mount(&server)
            .await;
        for (name, bytes) in [
            ("SHA256SUMS", digest.into_bytes()),
            ("release-manifest.json", manifest.into_bytes()),
            (asset_name.as_str(), body.clone()),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/download/v{release}/{name}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
                .mount(&server)
                .await;
        }
        // SAFETY: serial test; the variable is removed before returning.
        unsafe { std::env::set_var(LHC_RELEASE_BASE_ENV, server.uri()) };
        let result = async {
            assert_eq!(fetch_latest_release().await.unwrap(), release);
            let managed = managed_install_for_exe(&store.join("versions/1.0.16/bin/grok")).unwrap();
            run_managed_installer(&managed, release).await
        }
        .await;
        unsafe { std::env::remove_var(LHC_RELEASE_BASE_ENV) };
        result.expect("managed installer run");

        let store = dunce::canonicalize(&store).unwrap();
        assert_eq!(
            std::fs::read_to_string(store.join("installed-version"))
                .unwrap()
                .trim(),
            release
        );
        assert_eq!(
            std::fs::read_to_string(store.join("installed-name"))
                .unwrap()
                .trim(),
            "grok-custom"
        );
        assert_eq!(
            std::fs::read_to_string(store.join("installed-prefix"))
                .unwrap()
                .trim(),
            prefix.to_str().unwrap()
        );
        assert_eq!(
            std::fs::read_link(store.join("current")).unwrap(),
            store.join("versions").join(release)
        );
        assert!(
            store.join("versions/1.0.16/bin/grok").is_file(),
            "old versions are kept"
        );
        let out = std::process::Command::new(prefix.join("bin/grok-custom"))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "updated");
        assert_eq!(
            managed_install_for_exe(&prefix.join("bin/grok-custom"))
                .unwrap()
                .release,
            release
        );
        assert!(
            std::fs::read_dir(&dotgrok).unwrap().next().is_none(),
            "nothing written to a stock home"
        );
        assert!(
            !tmp.path().join("home").exists(),
            "default prefix never created"
        );
    }

    #[cfg(unix)]
    fn sha256_hex(bytes: &[u8]) -> String {
        let mut child = std::process::Command::new("sha256sum")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), bytes).unwrap();
        let out = child.wait_with_output().unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    }

    #[test]
    fn guidance_never_points_at_official_install() {
        for text in [managed_installer_guidance(), windows_update_guidance()] {
            assert!(text.contains("liminal-ai/grok-build-lhc"), "{text}");
            assert!(!text.contains("x.ai/cli/install"), "{text}");
            assert!(!text.contains("gh release"), "{text}");
        }
    }
}
