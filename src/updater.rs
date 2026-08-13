//! Signed, non-blocking Reyn Studio update discovery and verified downloads.
//!
//! The pinned release key authenticates metadata and archive hashes. It does
//! not replace Developer ID/notarization or Authenticode; installation remains
//! an explicit operating-system handoff.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const UPDATE_FEED_SCHEMA: &str = "com.reyn.studio.update-feed/1";
pub const UPDATE_SIGNATURE_SCHEMA: &str = "com.reyn.studio.update-signature/1";
pub const UPDATE_FEED_URL: &str = "https://reynflow.com/updates/studio/v1/latest.json";
pub const UPDATE_SIGNATURE_URL: &str = "https://reynflow.com/updates/studio/v1/latest.sig";
pub const UPDATE_KEY_ID: &str = "studio-update-v1-838583cea462bb23";
pub const UPDATE_PUBLIC_KEY_BASE64: &str = "70GFBW8MxpG7o01/Yg8iiQRBuIzbcc69emTW/HmG+x8=";

const MANIFEST_LIMIT_BYTES: u64 = 256 * 1024;
const SIGNATURE_LIMIT_BYTES: u64 = 16 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateArtifact {
    pub platform: String,
    pub architecture: String,
    pub minimum_os: String,
    pub url: String,
    pub archive_name: String,
    pub bytes: u64,
    pub sha256: String,
    pub developer_id_signed: bool,
    pub notarized: bool,
    pub authenticode_signed: bool,
}

impl UpdateArtifact {
    pub fn trust_summary(&self) -> String {
        match self.platform.as_str() {
            "macos-arm64" if self.developer_id_signed && self.notarized => {
                "Developer ID signed and Apple-notarized".into()
            }
            "macos-arm64" => "Unsigned and not notarized — manual installation".into(),
            "windows-x64" if self.authenticode_signed => {
                "Authenticode signed — manual installation".into()
            }
            "windows-x64" => "Not Authenticode signed — manual installation".into(),
            _ => "Platform signing status unavailable".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateFeed {
    pub schema: String,
    pub version: String,
    pub release_sequence: u64,
    pub published: u64,
    pub expires: u64,
    pub minimum_updater_version: String,
    pub channel: String,
    pub changelog_url: String,
    pub key_id: String,
    pub artifacts: Vec<UpdateArtifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct UpdateSignatureDocument {
    schema: String,
    key_id: String,
    algorithm: String,
    signature: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct AcceptedFeedState {
    highest_release_sequence: u64,
    version: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub version: String,
    pub release_sequence: u64,
    pub changelog_url: String,
    pub artifact: UpdateArtifact,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Verified,
    Error,
}

#[derive(Clone, Debug)]
pub struct UpdateSnapshot {
    pub phase: UpdatePhase,
    pub current_version: String,
    pub available: Option<AvailableUpdate>,
    pub downloaded_path: Option<PathBuf>,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub last_checked_utc_unix: Option<u64>,
    pub message: String,
}

impl Default for UpdateSnapshot {
    fn default() -> Self {
        Self {
            phase: UpdatePhase::Idle,
            current_version: env!("CARGO_PKG_VERSION").into(),
            available: None,
            downloaded_path: None,
            downloaded_bytes: 0,
            total_bytes: 0,
            last_checked_utc_unix: None,
            message: "Updates have not been checked yet.".into(),
        }
    }
}

struct UpdaterInner {
    snapshot: Mutex<UpdateSnapshot>,
    cancel: AtomicBool,
    repaint: egui::Context,
    feed_url: String,
    signature_url: String,
}

#[derive(Clone)]
pub struct Updater {
    inner: Arc<UpdaterInner>,
}

impl Updater {
    pub fn new(repaint: egui::Context) -> Self {
        Self {
            inner: Arc::new(UpdaterInner {
                snapshot: Mutex::new(UpdateSnapshot::default()),
                cancel: AtomicBool::new(false),
                repaint,
                feed_url: UPDATE_FEED_URL.into(),
                signature_url: UPDATE_SIGNATURE_URL.into(),
            }),
        }
    }

    pub fn snapshot(&self) -> UpdateSnapshot {
        self.inner
            .snapshot
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default()
    }

    pub fn check(&self) {
        let mut state = match self.inner.snapshot.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        if matches!(
            state.phase,
            UpdatePhase::Checking | UpdatePhase::Downloading
        ) {
            return;
        }
        state.phase = UpdatePhase::Checking;
        state.message = "Checking Reyn's signed update feed…".into();
        state.downloaded_path = None;
        drop(state);

        let inner = Arc::clone(&self.inner);
        let _ = std::thread::Builder::new()
            .name("reyn-update-check".into())
            .spawn(move || {
                let result = fetch_signed_feed(
                    &inner.feed_url,
                    &inner.signature_url,
                    env!("CARGO_PKG_VERSION"),
                    unix_now().unwrap_or(0),
                    &accepted_feed_state_path(),
                );
                if let Ok(mut state) = inner.snapshot.lock() {
                    state.last_checked_utc_unix = unix_now().ok();
                    match result {
                        Ok(Some(available)) => {
                            state.total_bytes = available.artifact.bytes;
                            state.message = format!(
                                "Reyn Studio {} is available. {}",
                                available.version,
                                available.artifact.trust_summary()
                            );
                            state.available = Some(available);
                            state.phase = UpdatePhase::Available;
                        }
                        Ok(None) => {
                            state.available = None;
                            state.message = format!(
                                "Reyn Studio {} is the newest verified release.",
                                env!("CARGO_PKG_VERSION")
                            );
                            state.phase = UpdatePhase::UpToDate;
                        }
                        Err(error) => {
                            state.message = error;
                            state.phase = UpdatePhase::Error;
                        }
                    }
                }
                inner.repaint.request_repaint();
            });
    }

    pub fn download(&self) {
        let mut state = match self.inner.snapshot.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        let Some(available) = state.available.clone() else {
            state.message = "Check for an available update before downloading.".into();
            state.phase = UpdatePhase::Error;
            return;
        };
        if state.phase == UpdatePhase::Downloading {
            return;
        }
        state.phase = UpdatePhase::Downloading;
        state.downloaded_bytes = 0;
        state.total_bytes = available.artifact.bytes;
        state.downloaded_path = None;
        state.message = format!("Downloading {}…", available.artifact.archive_name);
        self.inner.cancel.store(false, Ordering::Release);
        drop(state);

        let inner = Arc::clone(&self.inner);
        let _ = std::thread::Builder::new()
            .name("reyn-update-download".into())
            .spawn(move || {
                let destination = default_download_directory();
                let result = download_verified_artifact(&inner, &available.artifact, &destination);
                if let Ok(mut state) = inner.snapshot.lock() {
                    match result {
                        Ok(path) => {
                            state.downloaded_bytes = available.artifact.bytes;
                            state.downloaded_path = Some(path);
                            state.message = format!(
                                "Download verified. {} Reyn Studio will not open or install it automatically.",
                                available.artifact.trust_summary()
                            );
                            state.phase = UpdatePhase::Verified;
                        }
                        Err(error) => {
                            state.message = error;
                            state.phase = if state.available.is_some() {
                                UpdatePhase::Available
                            } else {
                                UpdatePhase::Error
                            };
                        }
                    }
                }
                inner.repaint.request_repaint();
            });
    }

    pub fn cancel_download(&self) {
        self.inner.cancel.store(true, Ordering::Release);
    }
}

pub fn show_compact_banner(ui: &mut egui::Ui, updater: &Updater) {
    let snapshot = updater.snapshot();
    if !matches!(
        snapshot.phase,
        UpdatePhase::Available | UpdatePhase::Downloading | UpdatePhase::Verified
    ) {
        return;
    }
    egui::Frame::NONE
        .fill(egui::Color32::from_rgb(37, 31, 27))
        .inner_margin(egui::Margin::symmetric(18, 8))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(&snapshot.message)
                        .color(egui::Color32::from_rgb(241, 229, 215)),
                );
                if let Some(available) = &snapshot.available {
                    ui.hyperlink_to("Release notes", &available.changelog_url);
                }
                match snapshot.phase {
                    UpdatePhase::Available => {
                        if ui.button("Download verified package").clicked() {
                            updater.download();
                        }
                    }
                    UpdatePhase::Downloading => {
                        let progress = if snapshot.total_bytes == 0 {
                            0.0
                        } else {
                            snapshot.downloaded_bytes as f32 / snapshot.total_bytes as f32
                        };
                        ui.add(
                            egui::ProgressBar::new(progress.clamp(0.0, 1.0))
                                .desired_width(150.0)
                                .show_percentage(),
                        );
                        if ui.button("Cancel").clicked() {
                            updater.cancel_download();
                        }
                    }
                    UpdatePhase::Verified => {
                        if let Some(path) = snapshot.downloaded_path.as_deref() {
                            if ui.button("Show package").clicked() {
                                let _ = reveal_in_file_manager(path);
                            }
                        }
                    }
                    _ => {}
                }
            });
        });
}

pub fn show_settings(ui: &mut egui::Ui, updater: Option<&Updater>, automatic_checks: &mut bool) {
    ui.checkbox(
        automatic_checks,
        "Automatically check the signed release feed at startup",
    );
    ui.label(
        egui::RichText::new(
            "Checks send no credentials, project data, geometry, or telemetry. Downloads are verified before they are published to disk.",
        )
        .small()
        .color(egui::Color32::from_rgb(147, 137, 129)),
    );
    ui.add_space(10.0);
    let Some(updater) = updater else {
        ui.label("The updater is unavailable in this application session.");
        return;
    };
    let snapshot = updater.snapshot();
    egui::Grid::new("settings.update-facts")
        .num_columns(2)
        .spacing([18.0, 7.0])
        .show(ui, |ui| {
            ui.label("Current version");
            ui.monospace(&snapshot.current_version);
            ui.end_row();
            ui.label("Latest verified");
            ui.monospace(
                snapshot
                    .available
                    .as_ref()
                    .map(|update| update.version.as_str())
                    .unwrap_or("No newer release"),
            );
            ui.end_row();
            ui.label("Last check");
            ui.label(
                snapshot
                    .last_checked_utc_unix
                    .map(crate::app::format_utc)
                    .unwrap_or_else(|| "Not checked".into()),
            );
            ui.end_row();
            if let Some(available) = &snapshot.available {
                ui.label("Package trust");
                ui.label(available.artifact.trust_summary());
                ui.end_row();
            }
        });
    ui.add_space(8.0);
    ui.label(&snapshot.message);
    if snapshot.phase == UpdatePhase::Downloading {
        let progress = if snapshot.total_bytes == 0 {
            0.0
        } else {
            snapshot.downloaded_bytes as f32 / snapshot.total_bytes as f32
        };
        ui.add(
            egui::ProgressBar::new(progress.clamp(0.0, 1.0))
                .desired_width(ui.available_width())
                .show_percentage(),
        );
    }
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                !matches!(
                    snapshot.phase,
                    UpdatePhase::Checking | UpdatePhase::Downloading
                ),
                egui::Button::new("Check for updates"),
            )
            .clicked()
        {
            updater.check();
        }
        if snapshot.phase == UpdatePhase::Available
            && ui.button("Download verified package").clicked()
        {
            updater.download();
        }
        if snapshot.phase == UpdatePhase::Downloading && ui.button("Cancel download").clicked() {
            updater.cancel_download();
        }
        if snapshot.phase == UpdatePhase::Verified {
            if let Some(path) = snapshot.downloaded_path.as_deref() {
                if ui.button("Show in Finder / Explorer").clicked() {
                    let _ = reveal_in_file_manager(path);
                }
            }
        }
        if let Some(available) = &snapshot.available {
            ui.hyperlink_to("Read release notes", &available.changelog_url);
        }
    });
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(
            "Installation is manual. Reyn Studio never silently replaces or executes the downloaded archive.",
        )
        .strong(),
    );
}

pub fn automatic_checks_enabled_from_disk() -> bool {
    let Some(path) = crate::settings::config_path() else {
        return true;
    };
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.get("automatic_update_checks")?.as_bool())
        .unwrap_or(true)
}

pub fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let status = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .status();
    #[cfg(target_os = "windows")]
    let status = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .status();
    #[cfg(target_os = "linux")]
    let status = reveal_in_linux_file_manager(path);
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let status = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "unsupported platform",
    ));

    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("File manager exited with status {status}.")),
        Err(error) => Err(format!("Could not reveal the update package: {error}")),
    }
}

#[cfg(target_os = "linux")]
fn reveal_in_linux_file_manager(path: &Path) -> std::io::Result<std::process::ExitStatus> {
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let uri = format!("file://{}", absolute.display());
    let dbus = std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.freedesktop.FileManager1",
            "--type=method_call",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1.ShowItems",
            &format!("array:string:{uri}"),
            "string:",
        ])
        .status();
    if matches!(&dbus, Ok(status) if status.success()) {
        return dbus;
    }
    let parent = absolute.parent().unwrap_or(absolute.as_path());
    std::process::Command::new("xdg-open").arg(parent).status()
}

fn fetch_signed_feed(
    feed_url: &str,
    signature_url: &str,
    current_version: &str,
    now_utc_unix: u64,
    state_path: &Path,
) -> Result<Option<AvailableUpdate>, String> {
    if feed_url != UPDATE_FEED_URL || signature_url != UPDATE_SIGNATURE_URL {
        return Err("The update metadata endpoint is not approved.".into());
    }
    let agent = update_agent(REQUEST_TIMEOUT);
    let feed_bytes = get_bounded(&agent, feed_url, MANIFEST_LIMIT_BYTES)?;
    let signature_bytes = get_bounded(&agent, signature_url, SIGNATURE_LIMIT_BYTES)?;
    let accepted = read_accepted_state(state_path)?;
    let feed = verify_and_validate_feed(
        &feed_bytes,
        &signature_bytes,
        UPDATE_KEY_ID,
        UPDATE_PUBLIC_KEY_BASE64,
        current_version,
        now_utc_unix,
        accepted.as_ref(),
    )?;
    persist_accepted_state(
        state_path,
        &AcceptedFeedState {
            highest_release_sequence: feed.release_sequence,
            version: feed.version.clone(),
        },
    )?;
    let current = Version::parse(current_version)
        .map_err(|_| "The running application version is invalid.".to_string())?;
    let latest =
        Version::parse(&feed.version).map_err(|_| "The update version is invalid.".to_string())?;
    if latest <= current {
        return Ok(None);
    }
    let artifact = select_artifact(&feed)?.clone();
    Ok(Some(AvailableUpdate {
        version: feed.version,
        release_sequence: feed.release_sequence,
        changelog_url: feed.changelog_url,
        artifact,
    }))
}

fn verify_and_validate_feed(
    feed_bytes: &[u8],
    signature_bytes: &[u8],
    expected_key_id: &str,
    public_key_base64: &str,
    current_version: &str,
    now_utc_unix: u64,
    accepted: Option<&AcceptedFeedState>,
) -> Result<UpdateFeed, String> {
    let signature: UpdateSignatureDocument = serde_json::from_slice(signature_bytes)
        .map_err(|_| "The update signature document is malformed.".to_string())?;
    if signature.schema != UPDATE_SIGNATURE_SCHEMA
        || signature.key_id != expected_key_id
        || signature.algorithm != "Ed25519"
    {
        return Err("The update signature uses an untrusted schema, key, or algorithm.".into());
    }
    let public_key = BASE64
        .decode(public_key_base64)
        .map_err(|_| "The pinned update key is invalid.".to_string())?;
    let public_key: [u8; 32] = public_key
        .try_into()
        .map_err(|_| "The pinned update key has the wrong length.".to_string())?;
    let verifying_key = VerifyingKey::from_bytes(&public_key)
        .map_err(|_| "The pinned update key is invalid.".to_string())?;
    let signature = BASE64
        .decode(signature.signature)
        .map_err(|_| "The update signature is not valid base64.".to_string())?;
    let signature =
        Signature::from_slice(&signature).map_err(|_| "The update signature is malformed.")?;
    verifying_key
        .verify(feed_bytes, &signature)
        .map_err(|_| "The update feed signature could not be verified.".to_string())?;

    let feed: UpdateFeed = serde_json::from_slice(feed_bytes)
        .map_err(|_| "The signed update feed is malformed.".to_string())?;
    validate_feed(
        &feed,
        expected_key_id,
        current_version,
        now_utc_unix,
        accepted,
    )?;
    Ok(feed)
}

fn validate_feed(
    feed: &UpdateFeed,
    expected_key_id: &str,
    current_version: &str,
    now_utc_unix: u64,
    accepted: Option<&AcceptedFeedState>,
) -> Result<(), String> {
    if feed.schema != UPDATE_FEED_SCHEMA
        || feed.key_id != expected_key_id
        || feed.channel != "stable"
    {
        return Err("The signed update feed has an unsupported schema, key, or channel.".into());
    }
    let latest = Version::parse(&feed.version)
        .map_err(|_| "The signed update feed has an invalid version.".to_string())?;
    let minimum = Version::parse(&feed.minimum_updater_version)
        .map_err(|_| "The signed update feed has an invalid updater floor.".to_string())?;
    let current = Version::parse(current_version)
        .map_err(|_| "The running application version is invalid.".to_string())?;
    if current < minimum {
        return Err(format!(
            "This update requires updater {} or newer. Download the release from reynflow.com.",
            feed.minimum_updater_version
        ));
    }
    if feed.release_sequence == 0
        || feed.published == 0
        || feed.expires <= feed.published
        || now_utc_unix > feed.expires
    {
        return Err("The signed update feed is expired or has invalid release timing.".into());
    }
    if feed.expires.saturating_sub(feed.published) > 90 * 24 * 60 * 60 {
        return Err("The signed update feed expiry window is too long.".into());
    }
    if let Some(accepted) = accepted {
        if feed.release_sequence < accepted.highest_release_sequence {
            return Err("The update feed was rejected as a rollback.".into());
        }
        if feed.release_sequence == accepted.highest_release_sequence
            && feed.version != accepted.version
        {
            return Err("The update feed reused a release sequence for another version.".into());
        }
    }
    if feed.changelog_url != "https://reynflow.com/docs/changelog" {
        return Err("The update feed changelog URL is not approved.".into());
    }
    if feed.artifacts.len() != 2 {
        return Err("The update feed must contain exactly two supported artifacts.".into());
    }
    let mut saw_macos = false;
    let mut saw_windows = false;
    for artifact in &feed.artifacts {
        if artifact.bytes == 0 || artifact.bytes > MAX_ARTIFACT_BYTES {
            return Err("An update artifact has an invalid byte count.".into());
        }
        if !is_sha256(&artifact.sha256) {
            return Err("An update artifact has an invalid SHA-256 digest.".into());
        }
        if artifact.minimum_os.trim().is_empty() || artifact.minimum_os.len() > 64 {
            return Err("An update artifact has an invalid minimum OS.".into());
        }
        let expected_name = match (artifact.platform.as_str(), artifact.architecture.as_str()) {
            ("macos-arm64", "arm64") => {
                saw_macos = true;
                let prefix = format!("Reyn-Studio-{}-build.", latest);
                let suffix = "-arm64.app.zip";
                if !artifact.archive_name.starts_with(&prefix)
                    || !artifact.archive_name.ends_with(suffix)
                    || artifact.archive_name
                        [prefix.len()..artifact.archive_name.len() - suffix.len()]
                        .parse::<u64>()
                        .is_err()
                {
                    return Err("The macOS update archive name is invalid.".into());
                }
                artifact.archive_name.clone()
            }
            ("windows-x64", "x64") => {
                saw_windows = true;
                format!("Reyn-Studio-{}-windows-x64.zip", latest)
            }
            _ => return Err("The update feed contains an unsupported platform target.".into()),
        };
        if artifact.archive_name != expected_name {
            return Err("An update artifact has an unexpected archive name.".into());
        }
        validate_artifact_url(&artifact.url, &feed.version, &artifact.archive_name)?;
    }
    if !saw_macos || !saw_windows {
        return Err("The update feed is missing a required platform artifact.".into());
    }
    Ok(())
}

fn validate_artifact_url(url: &str, version: &str, archive_name: &str) -> Result<(), String> {
    let github = format!(
        "https://github.com/videochataiai/reyn-studio/releases/download/v{version}/{archive_name}"
    );
    let reyn = format!("https://reynflow.com/releases/studio/v{version}/{archive_name}");
    if url != github && url != reyn {
        return Err("An update artifact URL is not an approved immutable release URL.".into());
    }
    Ok(())
}

fn select_artifact(feed: &UpdateFeed) -> Result<&UpdateArtifact, String> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    let target = ("macos-arm64", "arm64");
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    let target = ("windows-x64", "x64");
    #[cfg(not(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "windows", target_arch = "x86_64")
    )))]
    return Err("In-app updates are supported only on Apple-silicon macOS and Windows x64.".into());

    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "windows", target_arch = "x86_64")
    ))]
    feed.artifacts
        .iter()
        .find(|artifact| artifact.platform == target.0 && artifact.architecture == target.1)
        .ok_or_else(|| "The update feed has no package for this computer.".into())
}

fn update_agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(3)
        .max_redirects_will_error(true)
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .user_agent(format!(
            "ReynStudio/{} updater ({}; {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .build(),
        )
        .build();
    ureq::Agent::new_with_config(config)
}

fn get_bounded(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>, String> {
    let mut response = agent
        .get(url)
        .header("Accept", "application/json")
        .call()
        .map_err(|error| format!("The update service could not be reached: {error}"))?;
    if response.status().as_u16() != 200 {
        return Err(format!(
            "The update service returned HTTP {}.",
            response.status().as_u16()
        ));
    }
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|error| format!("The update response was invalid or too large: {error}"))
}

fn download_verified_artifact(
    inner: &Arc<UpdaterInner>,
    artifact: &UpdateArtifact,
    destination: &Path,
) -> Result<PathBuf, String> {
    fs::create_dir_all(destination)
        .map_err(|error| format!("Could not create the update download folder: {error}"))?;
    let final_path = destination.join(&artifact.archive_name);
    if final_path.is_file() && file_matches(&final_path, artifact)? {
        return Ok(final_path);
    }
    let part_path = destination.join(format!(
        ".{}.{}.part",
        artifact.archive_name,
        std::process::id()
    ));
    let result = (|| {
        let agent = update_agent(DOWNLOAD_TIMEOUT);
        let mut response = agent
            .get(&artifact.url)
            .header("Accept", "application/octet-stream")
            .call()
            .map_err(|error| format!("The update package could not be downloaded: {error}"))?;
        if response.status().as_u16() != 200 {
            return Err(format!(
                "The update download returned HTTP {}.",
                response.status().as_u16()
            ));
        }
        if let Some(content_length) = response
            .headers()
            .get("Content-Length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            if content_length != artifact.bytes {
                return Err("The update download length did not match signed metadata.".into());
            }
        }
        let mut reader = response.body_mut().as_reader();
        let mut file = File::create(&part_path)
            .map_err(|error| format!("Could not create the partial update file: {error}"))?;
        let mut hasher = Sha256::new();
        let mut downloaded = 0_u64;
        let mut buffer = vec![0_u8; 128 * 1024];
        loop {
            if inner.cancel.load(Ordering::Acquire) {
                return Err("Update download cancelled.".into());
            }
            let read = reader
                .read(&mut buffer)
                .map_err(|error| format!("The update download was interrupted: {error}"))?;
            if read == 0 {
                break;
            }
            downloaded = downloaded.saturating_add(read as u64);
            if downloaded > artifact.bytes {
                return Err("The update package exceeded its signed byte count.".into());
            }
            file.write_all(&buffer[..read])
                .map_err(|error| format!("Could not write the update package: {error}"))?;
            hasher.update(&buffer[..read]);
            if let Ok(mut state) = inner.snapshot.lock() {
                state.downloaded_bytes = downloaded;
            }
            inner.repaint.request_repaint();
        }
        if downloaded != artifact.bytes {
            return Err("The update package was truncated.".into());
        }
        let digest = format!("{:x}", hasher.finalize());
        if digest != artifact.sha256 {
            return Err("The update package failed SHA-256 verification.".into());
        }
        file.sync_all()
            .map_err(|error| format!("Could not flush the verified update package: {error}"))?;
        drop(file);
        if final_path.exists() {
            fs::remove_file(&final_path).map_err(|error| {
                format!("Could not replace the previous update package: {error}")
            })?;
        }
        fs::rename(&part_path, &final_path)
            .map_err(|error| format!("Could not publish the verified update package: {error}"))?;
        sync_parent(destination)?;
        Ok(final_path.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&part_path);
    }
    result
}

fn file_matches(path: &Path, artifact: &UpdateArtifact) -> Result<bool, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Could not inspect downloaded update: {error}"))?;
    if metadata.len() != artifact.bytes {
        return Ok(false);
    }
    let mut file =
        File::open(path).map_err(|error| format!("Could not open downloaded update: {error}"))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|error| format!("Could not verify downloaded update: {error}"))?;
    Ok(format!("{:x}", hasher.finalize()) == artifact.sha256)
}

fn read_accepted_state(path: &Path) -> Result<Option<AcceptedFeedState>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| "The local update rollback state is malformed.".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not read the local update state: {error}")),
    }
}

fn persist_accepted_state(path: &Path, state: &AcceptedFeedState) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "The update state path has no parent directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create the update state folder: {error}"))?;
    let temporary = parent.join(format!(".update-state.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec(state)
        .map_err(|error| format!("Could not encode the update state: {error}"))?;
    {
        let mut file = File::create(&temporary)
            .map_err(|error| format!("Could not create the update state: {error}"))?;
        file.write_all(&bytes)
            .map_err(|error| format!("Could not write the update state: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Could not flush the update state: {error}"))?;
    }
    fs::rename(&temporary, path)
        .map_err(|error| format!("Could not publish the update state: {error}"))?;
    sync_parent(parent)
}

fn sync_parent(parent: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("Could not flush the update directory: {error}"))
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

fn accepted_feed_state_path() -> PathBuf {
    crate::settings::config_path()
        .and_then(|path| path.parent().map(|parent| parent.join("update-state.json")))
        .unwrap_or_else(|| std::env::temp_dir().join("reyn-studio-update-state.json"))
}

fn default_download_directory() -> PathBuf {
    #[cfg(target_os = "windows")]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(target_os = "windows"))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from)
        .map(|path| path.join("Downloads").join("Reyn Studio Updates"))
        .unwrap_or_else(|| std::env::temp_dir().join("Reyn Studio Updates"))
}

fn unix_now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "The system clock is before the Unix epoch.".into())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn test_feed() -> UpdateFeed {
        UpdateFeed {
            schema: UPDATE_FEED_SCHEMA.into(),
            version: "0.3.0".into(),
            release_sequence: 3,
            published: 1_800_000_000,
            expires: 1_805_000_000,
            minimum_updater_version: "0.3.0".into(),
            channel: "stable".into(),
            changelog_url: "https://reynflow.com/docs/changelog".into(),
            key_id: "test-key".into(),
            artifacts: vec![
                UpdateArtifact {
                    platform: "macos-arm64".into(),
                    architecture: "arm64".into(),
                    minimum_os: "13.0".into(),
                    url: "https://github.com/videochataiai/reyn-studio/releases/download/v0.3.0/Reyn-Studio-0.3.0-build.3-arm64.app.zip".into(),
                    archive_name: "Reyn-Studio-0.3.0-build.3-arm64.app.zip".into(),
                    bytes: 128,
                    sha256: "a".repeat(64),
                    developer_id_signed: false,
                    notarized: false,
                    authenticode_signed: false,
                },
                UpdateArtifact {
                    platform: "windows-x64".into(),
                    architecture: "x64".into(),
                    minimum_os: "10 22H2".into(),
                    url: "https://github.com/videochataiai/reyn-studio/releases/download/v0.3.0/Reyn-Studio-0.3.0-windows-x64.zip".into(),
                    archive_name: "Reyn-Studio-0.3.0-windows-x64.zip".into(),
                    bytes: 256,
                    sha256: "b".repeat(64),
                    developer_id_signed: false,
                    notarized: false,
                    authenticode_signed: false,
                },
            ],
        }
    }

    fn signed(feed: &UpdateFeed) -> (Vec<u8>, Vec<u8>, String) {
        let signing = SigningKey::from_bytes(&[7_u8; 32]);
        let feed_bytes = serde_json::to_vec(feed).unwrap();
        let signature = signing.sign(&feed_bytes);
        let signature_document = UpdateSignatureDocument {
            schema: UPDATE_SIGNATURE_SCHEMA.into(),
            key_id: "test-key".into(),
            algorithm: "Ed25519".into(),
            signature: BASE64.encode(signature.to_bytes()),
        };
        (
            feed_bytes,
            serde_json::to_vec(&signature_document).unwrap(),
            BASE64.encode(signing.verifying_key().to_bytes()),
        )
    }

    #[test]
    fn signed_feed_accepts_exact_supported_targets() {
        let feed = test_feed();
        let (bytes, signature, public_key) = signed(&feed);
        let verified = verify_and_validate_feed(
            &bytes,
            &signature,
            "test-key",
            &public_key,
            "0.3.0",
            1_800_000_100,
            None,
        )
        .unwrap();
        assert_eq!(verified, feed);
    }

    #[test]
    fn signed_feed_rejects_tampering_unknown_fields_and_expiry() {
        let feed = test_feed();
        let (mut bytes, signature, public_key) = signed(&feed);
        let index = bytes.iter().position(|byte| *byte == b'3').unwrap();
        bytes[index] = b'4';
        assert!(verify_and_validate_feed(
            &bytes,
            &signature,
            "test-key",
            &public_key,
            "0.3.0",
            1_800_000_100,
            None,
        )
        .unwrap_err()
        .contains("signature"));

        let (bytes, signature, public_key) = signed(&feed);
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["unexpected"] = serde_json::json!(true);
        let unknown = serde_json::to_vec(&value).unwrap();
        let signing = SigningKey::from_bytes(&[7_u8; 32]);
        let document = UpdateSignatureDocument {
            schema: UPDATE_SIGNATURE_SCHEMA.into(),
            key_id: "test-key".into(),
            algorithm: "Ed25519".into(),
            signature: BASE64.encode(signing.sign(&unknown).to_bytes()),
        };
        assert!(verify_and_validate_feed(
            &unknown,
            &serde_json::to_vec(&document).unwrap(),
            "test-key",
            &public_key,
            "0.3.0",
            1_800_000_100,
            None,
        )
        .unwrap_err()
        .contains("malformed"));
        assert!(verify_and_validate_feed(
            &bytes,
            &signature,
            "test-key",
            &public_key,
            "0.3.0",
            feed.expires + 1,
            None,
        )
        .unwrap_err()
        .contains("expired"));
    }

    #[test]
    fn signed_feed_rejects_rollback_and_unapproved_urls() {
        let mut feed = test_feed();
        let accepted = AcceptedFeedState {
            highest_release_sequence: feed.release_sequence + 1,
            version: "0.4.0".into(),
        };
        let (bytes, signature, public_key) = signed(&feed);
        assert!(verify_and_validate_feed(
            &bytes,
            &signature,
            "test-key",
            &public_key,
            "0.3.0",
            1_800_000_100,
            Some(&accepted),
        )
        .unwrap_err()
        .contains("rollback"));

        feed.release_sequence += 2;
        feed.artifacts[0].url = "https://example.com/update.zip".into();
        let (bytes, signature, public_key) = signed(&feed);
        assert!(verify_and_validate_feed(
            &bytes,
            &signature,
            "test-key",
            &public_key,
            "0.3.0",
            1_800_000_100,
            None,
        )
        .unwrap_err()
        .contains("approved"));
    }

    #[test]
    fn downloaded_file_requires_signed_size_and_hash() {
        let directory =
            std::env::temp_dir().join(format!("reyn-updater-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("package.zip");
        fs::write(&path, b"verified package bytes").unwrap();
        let artifact = UpdateArtifact {
            bytes: fs::metadata(&path).unwrap().len(),
            sha256: format!("{:x}", Sha256::digest(b"verified package bytes")),
            ..test_feed().artifacts[0].clone()
        };
        assert!(file_matches(&path, &artifact).unwrap());
        let wrong_hash = UpdateArtifact {
            sha256: "0".repeat(64),
            ..artifact.clone()
        };
        assert!(!file_matches(&path, &wrong_hash).unwrap());
        let wrong_size = UpdateArtifact {
            bytes: artifact.bytes + 1,
            ..artifact
        };
        assert!(!file_matches(&path, &wrong_size).unwrap());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn accepted_release_sequence_persists_atomically() {
        let directory =
            std::env::temp_dir().join(format!("reyn-updater-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("update-state.json");
        let accepted = AcceptedFeedState {
            highest_release_sequence: 300,
            version: "0.3.0".into(),
        };
        persist_accepted_state(&path, &accepted).unwrap();
        assert_eq!(read_accepted_state(&path).unwrap(), Some(accepted));
        assert!(directory.read_dir().unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
        fs::remove_dir_all(directory).unwrap();
    }
}
