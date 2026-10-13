//! Release discovery on GitHub. Checking is anonymous: one request to the
//! public GitHub API, with no account, token, cookie or installation identity.
//! Packages are downloaded by the user in the browser; this module never
//! fetches or runs an installer.
use anyhow::{Result, anyhow, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{cmp::Ordering, io::Read, path::Path, time::Duration};
use tokio::sync::Mutex;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const NETWORK_TIMEOUT: Duration = Duration::from_secs(30);
/// Releases are listed in one page; more than enough for the newest one.
const RELEASES_API: &str =
    "https://api.github.com/repos/SurMistrz666/BazaStudio/releases?per_page=30";
/// The list of releases is small; anything larger is not a GitHub answer.
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
const NOTES_LIMIT: usize = 4000;

/// Public source code repository of the application.
pub const SOURCE_URL: &str = "https://github.com/SurMistrz666/BazaStudio";
/// The releases of the application, with their packages.
pub const RELEASES_URL: &str = "https://github.com/SurMistrz666/BazaStudio/releases";
/// The newest release; GitHub redirects it to the release page.
pub const DOWNLOADS_URL: &str = "https://github.com/SurMistrz666/BazaStudio/releases/latest";
/// The user guide: the README of the repository.
pub const GUIDE_URL: &str = "https://github.com/SurMistrz666/BazaStudio#readme";

/// A newer release published on GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateOffer {
    pub version: String,
    pub notes: String,
    /// Size of the package for this system, 0 when the release has none.
    pub size: u64,
    /// Lowercase hexadecimal SHA256 of the package as GitHub reports it;
    /// empty when GitHub does not report one.
    pub sha256: String,
    /// Name of the package for this system; empty when the release has none.
    pub filename: String,
    /// Page of the release on GitHub.
    pub page_url: String,
}
impl UpdateOffer {
    /// Compare a package the user downloaded in the browser with the size and
    /// SHA256 GitHub reports for it. Blocking: call it off the UI thread. The
    /// file is only read, never executed, moved or changed.
    pub fn matches_file(&self, path: &Path) -> Result<bool> {
        ensure!(hex_digest(&self.sha256), "update_digest_unavailable");
        let mut file = std::fs::File::open(path).map_err(|_| anyhow!("update_file_unreadable"))?;
        let metadata = file
            .metadata()
            .map_err(|_| anyhow!("update_file_unreadable"))?;
        ensure!(metadata.is_file(), "update_file_unreadable");
        if metadata.len() != self.size {
            return Ok(false);
        }
        let mut digest = Sha256::new();
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut total = 0u64;
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|_| anyhow!("update_file_unreadable"))?;
            if read == 0 {
                break;
            }
            total += read as u64;
            if total > self.size {
                return Ok(false);
            }
            digest.update(&buffer[..read]);
        }
        Ok(total == self.size && hex(digest.finalize().as_slice()) == self.sha256)
    }

    /// Open the page of this release in the default browser.
    pub fn open_page(&self) -> Result<()> {
        open_project_url(&self.page_url)
    }
}

/// Open [`DOWNLOADS_URL`] in the default browser.
pub fn open_downloads_page() -> Result<()> {
    open_project_url(DOWNLOADS_URL)
}

/// Open a page of the project on GitHub in the default browser. Only
/// addresses inside [`SOURCE_URL`] made of plain URL characters are accepted,
/// so nothing from a profile or user input can reach a command line.
pub fn open_project_url(url: &str) -> Result<()> {
    ensure!(project_url_allowed(url), "update_browser_address_invalid");
    let mut command = if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("/usr/bin/open");
        command.arg(url);
        command
    } else if cfg!(windows) {
        let mut command = std::process::Command::new("rundll32.exe");
        command.args(["url.dll,FileProtocolHandler", url]);
        command
    } else {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| anyhow!("update_browser_unavailable"))?;
    // Reap the short-lived helper without blocking the caller.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn project_url_allowed(url: &str) -> bool {
    let inside = url == SOURCE_URL
        || url
            .strip_prefix(SOURCE_URL)
            .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('#'));
    inside
        && url.len() <= 300
        && url
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/:._-#".contains(&b))
        && !url.contains("..")
}

/// Outcome of an update check that did not hit a problem with the answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateCheck {
    /// A newer release is published.
    Available(UpdateOffer),
    /// The newest published release is not newer, or nothing is published yet.
    UpToDate,
    /// GitHub could not be reached or declined the request (for example its
    /// limit of anonymous requests). An expected, quiet state.
    Unreachable,
}

/// Short sentence for an `update_*` code returned by [`Updater::check`] or
/// [`UpdateOffer::matches_file`]. Unknown codes fall back to a generic
/// sentence, so callers never have to show a raw code.
pub fn describe(code: &str, en: bool) -> &'static str {
    let (pl, english) = match code {
        "update_busy" => (
            "Sprawdzanie aktualizacji już trwa.",
            "An update check is already running.",
        ),
        "update_file_unreadable" => (
            "Nie udało się odczytać wybranego pliku.",
            "The selected file could not be read.",
        ),
        "update_digest_unavailable" => (
            "GitHub nie podał sumy SHA256 tego pakietu, więc pliku nie można porównać.",
            "GitHub does not report a SHA256 for this package, so the file cannot be compared.",
        ),
        "update_browser_unavailable" => (
            "Nie udało się otworzyć przeglądarki. Otwórz ręcznie github.com/SurMistrz666/BazaStudio/releases.",
            "The browser could not be opened. Open github.com/SurMistrz666/BazaStudio/releases yourself.",
        ),
        _ if code.starts_with("update_") => (
            "Odpowiedź GitHuba o wydaniach jest nieprawidłowa i została pominięta.",
            "The answer of GitHub about releases is invalid and was skipped.",
        ),
        _ => (
            "Nie udało się sprawdzić aktualizacji. Spróbuj ponownie później.",
            "Updates could not be checked. Try again later.",
        ),
    };
    if en { english } else { pl }
}

/// One release as the GitHub API lists it; unknown fields are ignored.
#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    #[serde(default)]
    size: u64,
    /// `sha256:<hex>` on releases GitHub has computed a digest for.
    #[serde(default)]
    digest: Option<String>,
}

/// Name part of the package for this system, as the packaging scripts write it.
fn platform_suffix() -> Option<&'static str> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("-Windows-x64.exe")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("-macOS-arm64.dmg")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("-macOS-x86_64.dmg")
    } else {
        None
    }
}

/// The newest release above `current` in a GitHub answer, with the package
/// whose name ends in `suffix`. Drafts and tags that are not versions are
/// skipped; the release page must be a page of this repository.
fn newest_offer(body: &[u8], current: &str, suffix: Option<&str>) -> Result<Option<UpdateOffer>> {
    let releases: Vec<Release> =
        serde_json::from_slice(body).map_err(|_| anyhow!("update_response_invalid"))?;
    let current = Version::parse(current)?;
    let newest = releases
        .iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let tag = release
                .tag_name
                .strip_prefix('v')
                .unwrap_or(&release.tag_name);
            Version::parse(tag)
                .ok()
                .map(|version| (version, tag, release))
        })
        .max_by(|a, b| a.0.compare(&b.0));
    let Some((version, tag, release)) = newest else {
        return Ok(None);
    };
    if version.compare(&current) != Ordering::Greater {
        return Ok(None);
    }
    ensure!(
        release.html_url.starts_with(&format!("{RELEASES_URL}/"))
            && project_url_allowed(&release.html_url),
        "update_response_invalid"
    );
    let asset = suffix.and_then(|suffix| {
        release
            .assets
            .iter()
            .find(|asset| asset.name.starts_with("BazaStudio-") && asset.name.ends_with(suffix))
    });
    let notes: String = release
        .body
        .as_deref()
        .unwrap_or_default()
        .chars()
        .take(NOTES_LIMIT)
        .collect();
    Ok(Some(UpdateOffer {
        version: tag.to_owned(),
        notes,
        size: asset.map_or(0, |asset| asset.size),
        sha256: asset
            .and_then(|asset| asset.digest.as_deref())
            .and_then(|digest| digest.strip_prefix("sha256:"))
            .filter(|digest| hex_digest(digest))
            .unwrap_or_default()
            .to_owned(),
        filename: asset.map(|asset| asset.name.clone()).unwrap_or_default(),
        page_url: release.html_url.clone(),
    }))
}

pub struct Updater {
    http: reqwest::Client,
    operation: Mutex<()>,
}
impl Updater {
    /// Opening an updater does not contact GitHub.
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .timeout(NETWORK_TIMEOUT)
            .user_agent(concat!("BazaStudio/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| anyhow!("update_transport_unavailable"))?;
        Ok(Self {
            http,
            operation: Mutex::new(()),
        })
    }

    /// Ask GitHub for the releases. Network problems and a declined request
    /// are reported as [`UpdateCheck::Unreachable`]; `Err` is reserved for an
    /// answer that is not a list of releases (see [`describe`]).
    pub async fn check(&self) -> Result<UpdateCheck> {
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| anyhow!("update_busy"))?;
        let request = self
            .http
            .get(RELEASES_API)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send();
        let response = match tokio::time::timeout(NETWORK_TIMEOUT, request).await {
            Ok(Ok(response)) => response,
            _ => return Ok(UpdateCheck::Unreachable),
        };
        match response.status() {
            reqwest::StatusCode::OK => {}
            // The repository or its releases are not published yet.
            reqwest::StatusCode::NOT_FOUND => return Ok(UpdateCheck::UpToDate),
            // Rate limits, redirects (never followed) and server errors.
            _ => return Ok(UpdateCheck::Unreachable),
        }
        if response
            .content_length()
            .is_some_and(|length| length > RESPONSE_LIMIT as u64)
        {
            return Err(anyhow!("update_response_limit"));
        }
        let body = match tokio::time::timeout(NETWORK_TIMEOUT, read_bounded(response)).await {
            Ok(Ok(body)) => body,
            Ok(Err(error)) if error.to_string() == "update_response_limit" => return Err(error),
            _ => return Ok(UpdateCheck::Unreachable),
        };
        Ok(match newest_offer(&body, VERSION, platform_suffix())? {
            Some(offer) => UpdateCheck::Available(offer),
            None => UpdateCheck::UpToDate,
        })
    }
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("update_network_unavailable"))?
    {
        ensure!(
            body.len() + chunk.len() <= RESPONSE_LIMIT,
            "update_response_limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|v| format!("{v:02x}")).collect()
}

fn hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Semantic version with an optional pre-release part (`0.4.0-preview.1`).
struct Version {
    core: [u64; 3],
    pre: Option<Vec<String>>,
}
impl Version {
    fn parse(value: &str) -> Result<Self> {
        ensure!(
            !value.is_empty() && value.len() <= 128 && value.is_ascii(),
            "update_version_invalid"
        );
        let (core, pre) = value
            .split_once('-')
            .map_or((value, None), |(c, p)| (c, Some(p)));
        let parts: Vec<_> = core.split('.').collect();
        ensure!(parts.len() == 3, "update_version_invalid");
        let mut numbers = [0; 3];
        for (i, part) in parts.iter().enumerate() {
            ensure!(
                !part.is_empty()
                    && part.bytes().all(|b| b.is_ascii_digit())
                    && (part.len() == 1 || !part.starts_with('0')),
                "update_version_invalid"
            );
            numbers[i] = part
                .parse()
                .map_err(|_| anyhow!("update_version_invalid"))?;
        }
        let pre = if let Some(pre) = pre {
            let parts: Vec<String> = pre.split('.').map(String::from).collect();
            ensure!(
                parts.iter().all(|s| !s.is_empty()
                    && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    && (!s.bytes().all(|b| b.is_ascii_digit())
                        || s.len() == 1
                        || !s.starts_with('0'))),
                "update_version_invalid"
            );
            Some(parts)
        } else {
            None
        };
        Ok(Self { core: numbers, pre })
    }
    fn compare(&self, other: &Self) -> Ordering {
        let core = self.core.cmp(&other.core);
        if core != Ordering::Equal {
            return core;
        }
        match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => {
                for (a, b) in a.iter().zip(b) {
                    let an = a.bytes().all(|v| v.is_ascii_digit());
                    let bn = b.bytes().all(|v| v.is_ascii_digit());
                    let order = match (an, bn) {
                        (true, true) => a.len().cmp(&b.len()).then(a.cmp(b)),
                        (true, false) => Ordering::Less,
                        (false, true) => Ordering::Greater,
                        (false, false) => a.cmp(b),
                    };
                    if order != Ordering::Equal {
                        return order;
                    }
                }
                a.len().cmp(&b.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn release(tag: &str, draft: bool, assets: &str) -> String {
        format!(
            r#"{{"tag_name":"{tag}","name":"Baza Studio {tag}","body":"Notes of {tag}","draft":{draft},"prerelease":true,
            "html_url":"https://github.com/SurMistrz666/BazaStudio/releases/tag/{tag}","assets":[{assets}],"extra":1}}"#
        )
    }
    fn asset(name: &str) -> String {
        format!(
            r#"{{"name":"{name}","size":1234,"digest":"sha256:{DIGEST}","browser_download_url":"https://github.com/x/{name}"}}"#
        )
    }

    #[test]
    fn versions_are_ordered_like_semantic_versions() {
        for (a, b) in [
            ("0.3.0-preview.9", "0.3.0-preview.10"),
            ("0.3.0-preview.10", "0.3.0"),
            ("0.3.9", "0.3.10"),
            ("1.0.0-alpha.1", "1.0.0-alpha.beta"),
            ("1.0.0-alpha", "1.0.0-beta"),
        ] {
            assert_eq!(
                Version::parse(a)
                    .unwrap()
                    .compare(&Version::parse(b).unwrap()),
                Ordering::Less
            );
        }
        for v in [
            "0.03.1",
            "0.3",
            "0.3.1-preview.01",
            "0.3.1-",
            "0.3.1+x",
            "0.3.1/../../x",
        ] {
            assert!(Version::parse(v).is_err());
        }
    }

    #[test]
    fn the_newest_published_release_above_the_running_one_is_offered() {
        let body = format!(
            "[{},{},{},{}]",
            release(
                "v0.4.0-preview.1",
                false,
                &asset("BazaStudio-0.4.0-preview.1-macOS-arm64.dmg")
            ),
            release(
                "v0.5.0-preview.1",
                false,
                &format!(
                    "{},{}",
                    asset("BazaStudio-0.5.0-preview.1-Windows-x64.exe"),
                    asset("BazaStudio-0.5.0-preview.1-macOS-arm64.dmg")
                )
            ),
            release("v9.0.0", true, ""),
            release("not-a-version", false, ""),
        );
        let offer = newest_offer(body.as_bytes(), "0.4.0-preview.1", Some("-macOS-arm64.dmg"))
            .unwrap()
            .unwrap();
        assert_eq!(offer.version, "0.5.0-preview.1");
        assert_eq!(offer.filename, "BazaStudio-0.5.0-preview.1-macOS-arm64.dmg");
        assert_eq!((offer.size, offer.sha256.as_str()), (1234, DIGEST));
        assert_eq!(offer.notes, "Notes of v0.5.0-preview.1");
        assert_eq!(
            offer.page_url,
            "https://github.com/SurMistrz666/BazaStudio/releases/tag/v0.5.0-preview.1"
        );
        // The same answer for a build that is already newest, and for an empty list.
        assert!(
            newest_offer(body.as_bytes(), "0.5.0-preview.1", None)
                .unwrap()
                .is_none()
        );
        assert!(newest_offer(b"[]", "0.4.0", None).unwrap().is_none());
    }

    #[test]
    fn a_release_without_a_package_for_this_system_still_points_to_its_page() {
        let body = format!(
            "[{}]",
            release("v1.0.0", false, &asset("BazaStudio-1.0.0-Windows-x64.exe"))
        );
        let offer = newest_offer(body.as_bytes(), "0.4.0", Some("-macOS-x86_64.dmg"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (offer.filename.as_str(), offer.size, offer.sha256.as_str()),
            ("", 0, "")
        );
        assert!(offer.page_url.ends_with("/releases/tag/v1.0.0"));
    }

    #[test]
    fn answers_that_are_not_releases_of_this_repository_are_refused() {
        assert!(newest_offer(b"{\"message\":\"Not Found\"}", "0.4.0", None).is_err());
        assert!(newest_offer(b"not json", "0.4.0", None).is_err());
        let foreign =
            release("v2.0.0", false, "").replace("SurMistrz666/BazaStudio", "someone/else");
        assert!(newest_offer(format!("[{foreign}]").as_bytes(), "0.4.0", None).is_err());
    }

    #[test]
    fn only_pages_of_the_project_can_be_opened() {
        for allowed in [SOURCE_URL, RELEASES_URL, DOWNLOADS_URL, GUIDE_URL] {
            assert!(project_url_allowed(allowed), "{allowed}");
        }
        for refused in [
            "https://github.com/SurMistrz666/BazaStudioEvil",
            "https://github.com/SurMistrz666/BazaStudio/../other",
            "https://github.com/SurMistrz666/BazaStudio/releases?x=1",
            "https://github.com/SurMistrz666/BazaStudio/releases;rm -rf",
            "http://github.com/SurMistrz666/BazaStudio",
            "file:///etc/passwd",
        ] {
            assert!(!project_url_allowed(refused), "{refused}");
        }
    }

    #[test]
    fn a_downloaded_file_is_compared_by_size_and_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.dmg");
        std::fs::write(&path, b"package bytes").unwrap();
        let mut offer = UpdateOffer {
            version: "1.0.0".into(),
            notes: String::new(),
            size: 13,
            sha256: hex(&Sha256::digest(b"package bytes")),
            filename: "BazaStudio-1.0.0-macOS-arm64.dmg".into(),
            page_url: format!("{RELEASES_URL}/tag/v1.0.0"),
        };
        assert!(offer.matches_file(&path).unwrap());
        offer.sha256 = DIGEST.into();
        assert!(!offer.matches_file(&path).unwrap());
        offer.sha256.clear();
        assert_eq!(
            offer.matches_file(&path).unwrap_err().to_string(),
            "update_digest_unavailable"
        );
    }

    /// Asks the real GitHub API. Run with `--ignored` when online.
    #[tokio::test]
    #[ignore]
    async fn live_github_check_ends_in_a_known_state() {
        let result = Updater::new().unwrap().check().await;
        println!("BazaStudio: {result:?}");
        assert!(result.is_ok());
    }
}
