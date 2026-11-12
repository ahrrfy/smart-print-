//! Enterprise Multi-Printer Edition — Dual-Mode Upstream & Custom Update Channel.
//!
//! Ensures PdfCraft continuously receives updates from the official upstream channel
//! (`storytold/pdfcraft`) while preserving all Enterprise Multi-Printer custom features:
//! 1. **Cloud Merged Channel (`custom_repo`)**: Downloads SHA-256-verified portable builds produced
//!    automatically by `.github/workflows/upstream-sync-release.yml` (which merges official
//!    `storytold/pdfcraft` releases with the multi-printer engine every 6 hours).
//! 2. **Local Source Sync & Build (`source_workspace`)**: When running on a workstation with the
//!    local git repository (`scripts/sync-upstream.ps1`), clicking "Update and Restart" on a new
//!    official `storytold/pdfcraft` release automatically fetches & merges the upstream tag,
//!    compiles the combined release binary, stages the portable archive, swaps the executable on
//!    exit, and restarts seamlessly — without requiring a commercial Windows Authenticode EV cert
//!    and without ever overwriting custom features with a vanilla upstream binary.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use craft_update::dialog::{StartupPrompt, StartupPromptResponse};
use craft_update::http::{Fetched, Http, Stop};
use craft_update::paths::Paths;
use craft_update::platform::{PackageKind, Platform, pick_asset};
use craft_update::release::{Release, parse_github_release_list, parse_sha256sums};
use craft_update::translate::Note;
use craft_update::updater::{Action, Status};

/// CLI flag passed to the detached Windows helper copy that swaps the portable binary after exit.
pub const CUSTOM_FINISH_FLAG: &str = "--finish-custom-update";

/// Configuration file name read from `PdfCraftData/update_channel.json` or beside `pdfcraft.exe`.
pub const CONFIG_FILE_NAME: &str = "update_channel.json";

/// Default official PdfCraft upstream repository.
pub const DEFAULT_UPSTREAM_REPO: &str = "storytold/pdfcraft";

/// Configuration governing where updates are checked and how merged binaries are obtained.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpdateChannelConfig {
    /// Official upstream repository (`storytold/pdfcraft`) monitored for new releases.
    #[serde(default = "default_upstream_repo")]
    pub upstream_repo: String,
    /// Optional GitHub fork (`owner/repo`) publishing pre-merged portable ZIPs + `SHA256SUMS.txt`.
    #[serde(default)]
    pub custom_repo: String,
    /// Optional local workspace path containing `scripts/sync-upstream.ps1` for automatic local
    /// upstream merge + rebuild when `custom_repo` is not configured.
    #[serde(default)]
    pub source_workspace: String,
}

fn default_upstream_repo() -> String {
    DEFAULT_UPSTREAM_REPO.to_owned()
}

impl Default for UpdateChannelConfig {
    fn default() -> Self {
        Self {
            upstream_repo: default_upstream_repo(),
            custom_repo: String::new(),
            source_workspace: String::new(),
        }
    }
}

/// Validates a GitHub `owner/repo` slug against path traversal and shell metacharacters.
pub fn is_valid_github_repo(repo: &str) -> bool {
    let trimmed = repo.trim();
    let mut parts = trimmed.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let valid_seg = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    valid_seg(owner) && valid_seg(name)
}

impl UpdateChannelConfig {
    /// Loads `update_channel.json` from `<portable data dir>/update_channel.json`,
    /// `<exe dir>/update_channel.json`, and environment overrides (`PDFCRAFT_CUSTOM_REPO`,
    /// `PDFCRAFT_SOURCE_WORKSPACE`).
    pub fn load() -> Self {
        let mut candidates = Vec::new();
        if let Some(data_dir) = crate::portable::data_dir() {
            candidates.push(data_dir.join(CONFIG_FILE_NAME));
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(exe_dir) = exe.parent()
        {
            candidates.push(exe_dir.join(CONFIG_FILE_NAME));
        }
        let mut cfg = Self::load_from_paths(&candidates);
        if let Ok(env_repo) = std::env::var("PDFCRAFT_CUSTOM_REPO")
            && !env_repo.trim().is_empty()
        {
            cfg.custom_repo = env_repo.trim().to_owned();
        }
        if let Ok(env_ws) = std::env::var("PDFCRAFT_SOURCE_WORKSPACE")
            && !env_ws.trim().is_empty()
        {
            cfg.source_workspace = env_ws.trim().to_owned();
        }
        if !is_valid_github_repo(&cfg.upstream_repo) {
            cfg.upstream_repo = default_upstream_repo();
        }
        if !cfg.custom_repo.is_empty() && !is_valid_github_repo(&cfg.custom_repo) {
            log::warn!(
                "ignoring invalid custom_repo '{}' in {}",
                cfg.custom_repo,
                CONFIG_FILE_NAME
            );
            cfg.custom_repo.clear();
        }
        cfg
    }

    /// Loads the first valid JSON config found in `paths`, falling back to [`Self::default`].
    pub fn load_from_paths(paths: &[PathBuf]) -> Self {
        for path in paths {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            match serde_json::from_str::<Self>(&text) {
                Ok(parsed) => return parsed,
                Err(e) => {
                    log::warn!("malformed {}: {e}", path.display());
                }
            }
        }
        Self::default()
    }

    /// Path to `scripts/sync-upstream.ps1` when a valid local source workspace is configured or
    /// discoverable relative to the executable.
    pub fn sync_script_path(&self) -> Option<PathBuf> {
        if !self.source_workspace.trim().is_empty() {
            let script = Path::new(self.source_workspace.trim())
                .join("scripts")
                .join("sync-upstream.ps1");
            if script.is_file() {
                return Some(script);
            }
        }
        // Fallback: if running from `<repo>/dist/PdfCraft-Portable/pdfcraft.exe`, check `../../scripts/sync-upstream.ps1`.
        if let Ok(exe) = std::env::current_exe() {
            for ancestor in exe.ancestors().skip(1).take(4) {
                let candidate = ancestor.join("scripts").join("sync-upstream.ps1");
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// Whether this installation can self-install a merged update (either via `custom_repo` or via
    /// local `sync-upstream.ps1` source merge & build).
    pub fn can_install_merged_update(&self) -> bool {
        is_valid_github_repo(&self.custom_repo) || (cfg!(windows) && self.sync_script_path().is_some())
    }
}

/// Discovers which channel offered a newer release.
#[derive(Clone, Debug)]
struct DiscoveredRelease {
    release: Release,
    from_custom_repo: bool,
}

type CheckOutcome = Result<Option<DiscoveredRelease>, String>;

struct PendingRestart {
    spawn: Box<dyn FnOnce() -> Result<(), String> + Send>,
}

impl PendingRestart {
    fn start(self) -> Result<(), String> {
        (self.spawn)()
    }
}

/// Coordinates non-blocking upstream/custom release checks, merged update staging, and clean
/// restart hand-off while preserving unsaved-document guards.
pub(crate) struct EnterpriseUpdater {
    version: String,
    status: Status,
    requested: bool,
    started: bool,
    check_rx: Option<Receiver<CheckOutcome>>,
    install_rx: Option<(Receiver<Result<PendingRestart, String>>, Release)>,
    prompt: Option<StartupPrompt>,
    from_custom_repo: bool,
    pending_restart: Option<PendingRestart>,
    installed: bool,
    close_wanted: bool,
    exiting: Arc<Mutex<bool>>,
}

impl EnterpriseUpdater {
    pub(crate) fn new(pkg_version: &str) -> Self {
        let effective = craft_update::self_update::current_version(&crate::UPDATE_APP, pkg_version)
            .map_or_else(|| pkg_version.to_owned(), |v| v.to_string());
        Self {
            version: effective,
            status: Status::Idle,
            requested: false,
            started: false,
            check_rx: None,
            install_rx: None,
            prompt: None,
            from_custom_repo: false,
            pending_restart: None,
            installed: false,
            close_wanted: false,
            exiting: Arc::new(Mutex::new(false)),
        }
    }

    pub(crate) fn prompt_open(&self) -> bool {
        self.prompt.is_some()
    }

    pub(crate) fn status(&self) -> &Status {
        &self.status
    }

    pub(crate) fn dismiss_status(&mut self) {
        self.requested = false;
        self.status = Status::Idle;
    }

    pub(crate) fn restart_to_update(&mut self) {
        if self.installed && self.pending_restart.is_some() {
            self.close_wanted = true;
        }
    }

    pub(crate) fn check_at_start(&mut self, ctx: &egui::Context) {
        if !std::mem::replace(&mut self.started, true) {
            self.begin_check(ctx, false);
        }
    }

    pub(crate) fn check_now(&mut self, ctx: &egui::Context) {
        self.started = true;
        self.begin_check(ctx, true);
    }

    fn begin_check(&mut self, ctx: &egui::Context, requested: bool) {
        if self.install_rx.is_some() {
            if requested {
                self.status = Status::Installing;
            }
            return;
        }
        if self.installed {
            if requested {
                self.status = Status::Ready;
            }
            return;
        }
        if requested {
            self.requested = true;
            self.status = Status::Checking;
        }
        if self.check_rx.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let running = self.version.clone();
        let ctx_clone = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("enterprise-update-check".into())
            .spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    query_channels(&running)
                }))
                .unwrap_or_else(|_| Err("the update check failed unexpectedly".into()));
                let _ = tx.send(res);
                ctx_clone.request_repaint();
            });
        match spawned {
            Ok(_) => self.check_rx = Some(rx),
            Err(e) => self.finish_check(Err(format!("couldn't start the update check ({e})"))),
        }
    }

    fn finish_check(&mut self, outcome: CheckOutcome) {
        let requested = std::mem::take(&mut self.requested);
        if self.install_rx.is_some() {
            if requested {
                self.status = Status::Installing;
            }
            return;
        }
        if self.installed {
            if requested {
                self.status = Status::Ready;
            }
            return;
        }
        match outcome {
            Ok(Some(found)) => {
                let cfg = UpdateChannelConfig::load();
                let mut prompt =
                    StartupPrompt::new(&crate::UPDATE_APP, found.release, self.version.clone());
                prompt.is_self_updating = cfg.can_install_merged_update();
                if !found.from_custom_repo && cfg.sync_script_path().is_some() {
                    prompt.set_message(
                        "Official upstream update detected — Update and Restart will merge it with your custom Multi-Printer features automatically.".into(),
                    );
                } else if !prompt.is_self_updating {
                    prompt.set_message(
                        "Official upstream update available. Run scripts\\sync-upstream.ps1 or configure custom_repo in update_channel.json so custom Multi-Printer features are preserved.".into(),
                    );
                }
                self.from_custom_repo = found.from_custom_repo;
                self.prompt = Some(prompt);
                self.status = Status::Idle;
            }
            Ok(None) => {
                if requested {
                    self.status = Status::UpToDate;
                }
            }
            Err(e) => {
                log::warn!("PdfCraft enterprise update check failed: {e}");
                if requested {
                    self.status = Status::Failed(e);
                }
            }
        }
    }

    pub(crate) fn tick(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.check_rx {
            match rx.try_recv() {
                Ok(ans) => {
                    self.check_rx = None;
                    self.finish_check(ans);
                }
                Err(TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                Err(TryRecvError::Disconnected) => {
                    self.check_rx = None;
                    self.finish_check(Err("the update check stopped unexpectedly".into()));
                }
            }
        }

        if let Some((rx, _)) = &self.install_rx {
            let outcome = match rx.try_recv() {
                Ok(done) => Some(done),
                Err(TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(150));
                    None
                }
                Err(TryRecvError::Disconnected) => {
                    Some(Err("the update worker stopped unexpectedly".into()))
                }
            };
            if let Some(outcome) = outcome {
                let release = self.install_rx.take().map(|(_, r)| r);
                match outcome {
                    Ok(restart) => {
                        self.installed = true;
                        self.pending_restart = Some(restart);
                        self.close_wanted = true;
                        if let Some(prompt) = &mut self.prompt {
                            prompt.is_installing = false;
                            prompt.is_ready = true;
                            prompt.set_note(Note::new(
                                craft_update::updater::READY_TEXT,
                                &[("app", crate::UPDATE_APP.name)],
                            ));
                        }
                        if matches!(self.status, Status::Installing) {
                            self.status = Status::Ready;
                        }
                    }
                    Err(err) => {
                        log::warn!("PdfCraft enterprise self-update failed: {err}");
                        if matches!(self.status, Status::Installing) {
                            self.status = Status::Idle;
                        }
                        if self.prompt.is_none()
                            && let Some(rel) = release
                        {
                            let mut p = StartupPrompt::new(
                                &crate::UPDATE_APP,
                                rel,
                                self.version.clone(),
                            );
                            p.is_self_updating =
                                UpdateChannelConfig::load().can_install_merged_update();
                            self.prompt = Some(p);
                        }
                        if let Some(prompt) = &mut self.prompt {
                            prompt.is_installing = false;
                            prompt.set_message(err);
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn show_if(&mut self, ctx: &egui::Context, can_start: bool) -> Action {
        if can_start && self.pending_restart.is_some() && std::mem::take(&mut self.close_wanted) {
            return Action::Close;
        }
        let Some(prompt) = &mut self.prompt else {
            return Action::None;
        };
        match prompt.show(ctx) {
            StartupPromptResponse::Pending => Action::None,
            StartupPromptResponse::UpdateAndRestart => {
                if can_start {
                    self.start_install(ctx);
                } else if let Some(p) = &mut self.prompt {
                    p.set_note(Note::new(
                        "Save or close your open work first; the update restarts the app.",
                        &[],
                    ));
                }
                Action::None
            }
            StartupPromptResponse::Download => {
                let url = if craft_update::release::safe_web_link(&prompt.release.html_url) {
                    prompt.release.html_url.clone()
                } else {
                    crate::UPDATE_APP.releases_url()
                };
                self.prompt = None;
                Action::OpenUrl(url)
            }
            StartupPromptResponse::Later => {
                self.prompt = None;
                Action::None
            }
        }
    }

    fn start_install(&mut self, ctx: &egui::Context) {
        if self.installed {
            if self.pending_restart.is_some() {
                self.close_wanted = true;
            }
            return;
        }
        let cfg = UpdateChannelConfig::load();
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        if !cfg.can_install_merged_update() {
            prompt.set_message(
                "Run scripts\\sync-upstream.ps1 or set custom_repo in update_channel.json to install merged updates without losing Multi-Printer features.".into(),
            );
            prompt.is_installing = false;
            return;
        }
        prompt.is_installing = true;
        if !self.from_custom_repo && cfg.sync_script_path().is_some() {
            prompt.set_message(
                "Syncing official upstream release & compiling custom Multi-Printer features (this may take 1–2 minutes)…".into(),
            );
        } else {
            prompt.clear_message();
        }

        let release = prompt.release.clone();
        let worker_release = release.clone();
        let from_custom = self.from_custom_repo;
        let ctx_clone = ctx.clone();
        let (tx, rx) = channel();
        let exiting = Arc::clone(&self.exiting);

        let spawned = std::thread::Builder::new()
            .name("enterprise-self-update".into())
            .spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    prepare_merged_update(&cfg, &worker_release, from_custom)
                }))
                .unwrap_or_else(|_| Err("the update failed unexpectedly".into()));

                let guard = exiting.lock().unwrap_or_else(PoisonError::into_inner);
                if *guard {
                    drop(guard);
                    if let Ok(restart) = res
                        && let Err(e) = restart.start()
                    {
                        log::warn!("PdfCraft helper launch on exit failed: {e}");
                    }
                } else {
                    let _ = tx.send(res);
                    drop(guard);
                    ctx_clone.request_repaint();
                }
            });

        match spawned {
            Ok(_) => self.install_rx = Some((rx, release)),
            Err(e) => {
                prompt.is_installing = false;
                prompt.set_message(format!("Couldn't start the update ({e})"));
            }
        }
    }

    pub(crate) fn start_pending_update(&mut self) {
        let mut guard = self.exiting.lock().unwrap_or_else(PoisonError::into_inner);
        *guard = true;
        drop(guard);

        if let Some((rx, _)) = self.install_rx.take()
            && let Ok(Ok(restart)) = rx.try_recv()
        {
            self.pending_restart = Some(restart);
            self.installed = true;
        }
        if let Some(restart) = self.pending_restart.take()
            && let Err(e) = restart.start()
        {
            log::warn!("PdfCraft enterprise update helper failed to start: {e}");
        }
    }
}

impl Drop for EnterpriseUpdater {
    fn drop(&mut self) {
        self.start_pending_update();
    }
}

/// Checks `custom_repo` first (if configured), then checks the official upstream channel
/// (`storytold/pdfcraft`) so upstream releases are always received promptly.
fn query_channels(running_version: &str) -> CheckOutcome {
    let cfg = UpdateChannelConfig::load();
    let http = Http::new(&format!("{}/Update", crate::UPDATE_APP.name))?;
    let running_parsed = craft_update::parse_version(running_version);

    if is_valid_github_repo(&cfg.custom_repo) {
        match check_repo_releases(&http, &cfg.custom_repo, running_parsed.as_ref()) {
            Ok(Some(release)) => {
                return Ok(Some(DiscoveredRelease {
                    release,
                    from_custom_repo: true,
                }));
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!(
                    "custom_repo ({}) check failed, falling back to official upstream: {e}",
                    cfg.custom_repo
                );
            }
        }
    }

    let upstream_repo = if is_valid_github_repo(&cfg.upstream_repo) {
        cfg.upstream_repo.as_str()
    } else {
        DEFAULT_UPSTREAM_REPO
    };
    let upstream = check_repo_releases(&http, upstream_repo, running_parsed.as_ref())?;
    Ok(upstream.map(|release| DiscoveredRelease {
        release,
        from_custom_repo: false,
    }))
}

fn check_repo_releases(
    http: &Http,
    repo: &str,
    running: Option<&craft_update::Version>,
) -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=30");
    match http.get_api(&url, None)? {
        Fetched::Body { text, .. } => {
            let latest = parse_github_release_list(&text, repo, false)?;
            Ok(latest.filter(|rel| {
                rel.version
                    .as_ref()
                    .zip(running)
                    .is_some_and(|(rel_v, cur_v)| craft_update::version::offers_update(rel_v, cur_v))
            }))
        }
        Fetched::RateLimited { .. } => Err("GitHub's request limit was reached".into()),
        Fetched::NotModified | Fetched::NotFound => Ok(None),
    }
}

/// Prepares a merged Portable ZIP (either downloaded & SHA-256 verified from `custom_repo`, or
/// merged & compiled locally from `storytold/pdfcraft` via `scripts/sync-upstream.ps1`), then
/// stages the detached Windows update helper.
fn prepare_merged_update(
    cfg: &UpdateChannelConfig,
    release: &Release,
    from_custom_repo: bool,
) -> Result<PendingRestart, String> {
    let exe = std::env::current_exe().map_err(|e| format!("couldn't locate current executable ({e})"))?;
    let paths = Paths::for_app(&crate::UPDATE_APP)?;
    std::fs::create_dir_all(&paths.cache).map_err(|e| format!("couldn't create update cache ({e})"))?;

    let staged_zip = if from_custom_repo && is_valid_github_repo(&cfg.custom_repo) {
        download_verified_custom_zip(&paths, release)?
    } else if let Some(script) = cfg.sync_script_path() {
        build_merged_upstream_zip(&paths, &script, &release.tag)?
    } else if is_valid_github_repo(&cfg.custom_repo) {
        // An upstream release was announced before `custom_repo` finished building it; check if
        // `custom_repo` now has the matching release asset ready.
        let http = Http::new(&format!("{}/Update", crate::UPDATE_APP.name))?;
        let running = craft_update::parse_version("0.0.0");
        if let Ok(Some(custom_rel)) = check_repo_releases(&http, &cfg.custom_repo, running.as_ref())
            && custom_rel.tag == release.tag
        {
            download_verified_custom_zip(&paths, &custom_rel)?
        } else {
            return Err(format!(
                "Upstream {} is new; your custom release pipeline ({}) has not published the merged build yet. Try again shortly or run scripts\\sync-upstream.ps1.",
                release.tag, cfg.custom_repo
            ));
        }
    } else {
        return Err(
            "Configure custom_repo in update_channel.json or run scripts\\sync-upstream.ps1 to install merged updates.".into(),
        );
    };

    schedule_windows_portable_swap(&paths, &exe, &staged_zip)
}

fn download_verified_custom_zip(paths: &Paths, release: &Release) -> Result<PathBuf, String> {
    let platform = Platform::current().ok_or("unsupported platform for portable self-update")?;
    let (asset, _) = pick_asset(
        release,
        &[crate::UPDATE_APP.id],
        platform,
        &[PackageKind::PortableZip],
    )
    .ok_or_else(|| {
        format!(
            "release {} has no portable ZIP asset for {}",
            release.version_label(),
            platform.label()
        )
    })?;

    let http = Http::new(&format!("{}/Update", crate::UPDATE_APP.name))?;
    let sha = match &asset.sha256 {
        Some(digest) => digest.clone(),
        None => {
            let sums_asset = release.sums_asset().ok_or_else(|| {
                format!(
                    "release {} has no SHA256SUMS.txt to verify {}",
                    release.tag, asset.name
                )
            })?;
            let sums_text = http
                .get_text(&sums_asset.url)?
                .ok_or("SHA256SUMS.txt was not found on the release")?;
            parse_sha256sums(&sums_text)
                .into_iter()
                .find(|(_, name)| name == &asset.name)
                .map(|(hash, _)| hash)
                .ok_or_else(|| format!("SHA256SUMS.txt does not list {}", asset.name))?
        }
    };

    let downloads_dir = paths.downloads();
    std::fs::create_dir_all(&downloads_dir)
        .map_err(|e| format!("couldn't create downloads folder ({e})"))?;
    let dest = downloads_dir.join(&asset.name);
    http.download(&asset.url, &dest, asset.size, &sha, &Stop::default(), &mut |_| {})?;
    Ok(dest)
}

fn build_merged_upstream_zip(
    paths: &Paths,
    script_path: &Path,
    upstream_tag: &str,
) -> Result<PathBuf, String> {
    if !craft_update::release::valid_tag(upstream_tag) {
        return Err(format!("refusing invalid upstream tag '{upstream_tag}'"));
    }
    let staged_zip = paths.cache.join(format!("pdfcraft-{upstream_tag}-merged-portable.zip"));
    let _ = std::fs::remove_file(&staged_zip);

    let mut cmd = std::process::Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ])
    .arg(script_path)
    .arg("-TargetRef")
    .arg(upstream_tag)
    .arg("-StageZipPath")
    .arg(&staged_zip)
    .arg("-SkipTests");

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let output = cmd
        .output()
        .map_err(|e| format!("couldn't launch sync-upstream.ps1 ({e})"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        return Err(format!("upstream sync & build failed: {detail}"));
    }
    if !staged_zip.is_file() {
        return Err("sync-upstream.ps1 completed without producing the staged portable ZIP".into());
    }
    Ok(staged_zip)
}

fn schedule_windows_portable_swap(
    paths: &Paths,
    exe: &Path,
    staged_zip: &Path,
) -> Result<PendingRestart, String> {
    #[cfg(windows)]
    {
        let helper_dir = paths.cache.join("custom-update-helper");
        let _ = std::fs::remove_dir_all(&helper_dir);
        std::fs::create_dir_all(&helper_dir)
            .map_err(|e| format!("couldn't prepare update helper folder ({e})"))?;
        let exe_name = exe.file_name().ok_or("executable has no file name")?;
        let helper_exe = helper_dir.join(exe_name);
        std::fs::copy(exe, &helper_exe)
            .map_err(|e| format!("couldn't stage update helper ({e})"))?;

        let staging_dir = paths.cache.join("custom-portable-staging");
        let pid = std::process::id();
        let mut cmd = craft_update::process::command(&helper_exe);
        cmd.args(build_helper_args(pid, exe, staged_zip, &staging_dir));
        Ok(PendingRestart {
            spawn: Box::new(move || {
                craft_update::process::spawn_detached(cmd)
                    .map_err(|e| format!("couldn't start custom update helper ({e})"))
            }),
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (paths, exe, staged_zip);
        Err("automatic custom portable swap is supported on Windows; run scripts/sync-upstream.ps1 to rebuild.".into())
    }
}

/// Builds the argument list passed to the detached helper process.
pub fn build_helper_args(
    pid: u32,
    exe: &Path,
    zip: &Path,
    staging: &Path,
) -> Vec<OsString> {
    vec![
        OsString::from(CUSTOM_FINISH_FLAG),
        OsString::from(pid.to_string()),
        exe.as_os_str().to_os_string(),
        zip.as_os_str().to_os_string(),
        staging.as_os_str().to_os_string(),
    ]
}

/// Parses `--finish-custom-update <pid> <exe> <zip> <staging>`.
pub fn parse_helper_args(args: &[OsString]) -> Option<(u32, PathBuf, PathBuf, PathBuf)> {
    let [flag, pid, exe, zip, staging] = args else {
        return None;
    };
    if flag != CUSTOM_FINISH_FLAG {
        return None;
    }
    let parsed_pid = pid.to_str()?.parse::<u32>().ok()?;
    Some((
        parsed_pid,
        PathBuf::from(exe),
        PathBuf::from(zip),
        PathBuf::from(staging),
    ))
}

/// Entrypoint called at the very start of `main()` on Windows when `pdfcraft.exe` is invoked as
/// the detached post-exit portable swap helper (`--finish-custom-update`).
#[cfg(windows)]
pub fn finish_custom_update(args: &[OsString]) -> Option<Result<(), String>> {
    if args.first().is_none_or(|a| a != CUSTOM_FINISH_FLAG) {
        return None;
    }
    let Some((pid, exe, zip, staging)) = parse_helper_args(args) else {
        return Some(Err("incomplete custom update helper arguments".into()));
    };
    Some(run_windows_finish(pid, &exe, &zip, &staging))
}

#[cfg(windows)]
fn run_windows_finish(
    pid: u32,
    exe: &Path,
    zip: &Path,
    staging: &Path,
) -> Result<(), String> {
    let stem_ok = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| crate::UPDATE_APP.stems().any(|id| id.eq_ignore_ascii_case(s)));
    if !stem_ok {
        return Err("custom update helper refuses non-PdfCraft target executable".into());
    }
    let (Some(folder), Some(exe_name)) = (exe.parent(), exe.file_name()) else {
        return Err("target executable has no parent directory".into());
    };
    if !craft_update::process::wait_for_exit(pid, Duration::from_secs(60)) {
        let _ = std::fs::remove_file(zip);
        return Err("PdfCraft was still running after 60 seconds; portable update aborted".into());
    }

    let unpack_res =
        craft_update::portable::unpack_over(zip, staging, folder, exe_name, &|_| Ok(()));
    let _ = std::fs::remove_file(zip);
    unpack_res?;

    craft_update::process::spawn_detached(std::process::Command::new(exe))
        .map_err(|e| format!("updated PdfCraft, but couldn't relaunch {} ({e})", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_repo_validation_rejects_traversal_and_metacharacters() {
        assert!(is_valid_github_repo("storytold/pdfcraft"));
        assert!(is_valid_github_repo("my-org_1/pdfcraft.enterprise"));
        assert!(!is_valid_github_repo(""));
        assert!(!is_valid_github_repo("storytold"));
        assert!(!is_valid_github_repo("storytold/pdfcraft/extra"));
        assert!(!is_valid_github_repo("../pdfcraft"));
        assert!(!is_valid_github_repo("storytold/.."));
        assert!(!is_valid_github_repo("storytold/pdfcraft;calc"));
    }

    #[test]
    fn helper_args_roundtrip_preserves_spaces_and_unicode_paths() {
        let exe = Path::new(r"D:\المساعد الذكي\pdfcraft\dist\PdfCraft-Portable\pdfcraft.exe");
        let zip = Path::new(r"D:\المساعد الذكي\pdfcraft\dist\PdfCraft-Portable\PdfCraftData\cache\update.zip");
        let staging = Path::new(r"D:\المساعد الذكي\pdfcraft\dist\PdfCraft-Portable\PdfCraftData\cache\staging");
        let built = build_helper_args(9876, exe, zip, staging);
        let parsed = parse_helper_args(&built);
        assert_eq!(
            parsed,
            Some((
                9876,
                exe.to_path_buf(),
                zip.to_path_buf(),
                staging.to_path_buf()
            ))
        );
        assert_eq!(parse_helper_args(&[]), None);
        assert_eq!(parse_helper_args(&[OsString::from("--other")]), None);
        assert_eq!(
            parse_helper_args(&[OsString::from(CUSTOM_FINISH_FLAG), OsString::from("123")]),
            None
        );
    }

    #[test]
    fn config_loads_from_json_and_falls_back_gracefully() {
        let dir = std::env::temp_dir().join(format!("pdfcraft-custom-upd-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_file = dir.join(CONFIG_FILE_NAME);
        std::fs::write(
            &cfg_file,
            r#"{"upstream_repo":"storytold/pdfcraft","custom_repo":"acme-corp/pdfcraft-enterprise","source_workspace":"D:\\repo"}"#,
        )
        .unwrap();
        let loaded = UpdateChannelConfig::load_from_paths(&[cfg_file.clone()]);
        assert_eq!(loaded.upstream_repo, "storytold/pdfcraft");
        assert_eq!(loaded.custom_repo, "acme-corp/pdfcraft-enterprise");
        assert_eq!(loaded.source_workspace, r"D:\repo");
        assert!(loaded.can_install_merged_update());

        std::fs::write(&cfg_file, "{ malformed json").unwrap();
        let fallback = UpdateChannelConfig::load_from_paths(&[cfg_file]);
        assert_eq!(fallback, UpdateChannelConfig::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
