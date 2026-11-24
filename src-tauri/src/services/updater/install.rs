//! The install path, compiled only where build.rs sets `echo_update_install`: download
//! and verify the release through the updater plugin, wait until Echo is idle, install it
//! and restart. The shared status, schedule and check live in `services/updater.rs`.

use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_updater::{Update, Updater, UpdaterExt};

use super::{
    CheckOutcome, UpdateEnvelope, UpdateSource, UpdateStatus, UpdaterService, CHECK_TIMEOUT,
    CONNECT_TIMEOUT, MAX_REDIRECTS,
};
use crate::audio::DesktopAudioCapture;
use crate::services::runtime::{Activity, EchoRuntime};
use crate::services::transcript_delivery;

/// What an offer carries on builds that install: the plugin's release, downloaded and
/// verified on request.
pub type ProductionPending = Update;

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const IDLE_POLL: Duration = Duration::from_secs(1);
/// Covers the gap between a job completing and the renderer delivering its transcript:
/// a manual-mode transcript never calls delivery, so only a timeout can cover it.
const QUIET_PERIOD: Duration = Duration::from_secs(10);
const MIB: u64 = 1024 * 1024;

/// Admission to install, atomic with capture admission.
pub(crate) trait InstallGate {
    /// Sets the install barrier when Echo is idle; otherwise leaves it clear and returns false.
    fn try_close(&self) -> bool;
    fn reopen(&self);
}

impl<P: Clone> UpdaterService<P> {
    /// Admits one install from available or failed. Any other state ignores the request.
    pub(super) fn admit_install(&self) -> Option<(String, P)> {
        let (admitted, changed) = {
            let mut state = self.lock();
            let version = match &state.status {
                UpdateStatus::Available { version } | UpdateStatus::Failed { version }
                    if !state.busy =>
                {
                    version.clone()
                }
                _ => return None,
            };
            let pending = state.pending.clone()?;
            state.busy = true;
            let changed = state.change(UpdateStatus::Downloading {
                version: version.clone(),
                downloaded_bytes: 0,
                total_bytes: None,
            });
            ((version, pending), changed)
        };
        self.publish(changed);
        Some(admitted)
    }

    pub(super) fn fail(&self, version: String) {
        let changed = {
            let mut state = self.lock();
            state.busy = false;
            state.change(UpdateStatus::Failed { version })
        };
        self.publish(changed);
    }
}

/// `updater::request_update` lands here on builds that install. Returns as soon as the request
/// is admitted (status already downloading) or ignored (status unchanged).
pub fn request_install<R: Runtime>(app: &AppHandle<R>) -> UpdateEnvelope {
    let service = app.state::<UpdaterService>().inner().clone();
    let Some((version, pending)) = service.admit_install() else {
        return service.status();
    };
    let admitted = service.status();
    let source = PluginSource::new(app.clone());
    let gate = IdleGate {
        capture: app.state::<DesktopAudioCapture>().inner().clone(),
        runtime: app.state::<EchoRuntime>().inner().clone(),
        deliveries: transcript_delivery::activity,
    };
    let worker = service.clone();
    let failed_version = version.clone();
    let spawned = std::thread::Builder::new()
        .name("echo-update-install".to_owned())
        .spawn(move || run_install(&worker, &source, &gate, IDLE_POLL, version, pending));
    if let Err(error) = spawned {
        log::warn!("update_install outcome=failed stage=start error={error}");
        service.fail(failed_version);
    }
    admitted
}

/// Download, then the idle gate, then install. Any error leaves the status failed and
/// capture admission open.
pub(crate) fn run_install<S: UpdateSource>(
    service: &UpdaterService<S::Pending>,
    source: &S,
    gate: &impl InstallGate,
    poll: Duration,
    version: String,
    pending: S::Pending,
) {
    let mut downloaded: u64 = 0;
    let mut published_step: Option<u64> = None;
    let mut on_chunk = |chunk: usize, total: Option<u64>| {
        downloaded = downloaded.saturating_add(chunk as u64);
        // One status change per whole percent, or per MiB when the server sends no length.
        let step = match total {
            Some(total) if total > 0 => downloaded.saturating_mul(100) / total,
            _ => downloaded / MIB,
        };
        if published_step != Some(step) {
            published_step = Some(step);
            service.set_status(UpdateStatus::Downloading {
                version: version.clone(),
                downloaded_bytes: downloaded,
                total_bytes: total,
            });
        }
    };
    let bytes = match source.download(&pending, &mut on_chunk) {
        Ok(bytes) => bytes,
        Err(error) => {
            log::warn!("update_install outcome=failed stage=download error={error}");
            service.fail(version);
            return;
        }
    };
    log::info!("update_install outcome=downloaded version={version}");
    if !gate.try_close() {
        log::info!("update_install outcome=waiting_for_idle version={version}");
        service.set_status(UpdateStatus::WaitingForIdle {
            version: version.clone(),
        });
        loop {
            std::thread::sleep(poll);
            if gate.try_close() {
                break;
            }
        }
    }
    log::info!("update_install outcome=installing version={version}");
    service.set_status(UpdateStatus::Installing {
        version: version.clone(),
    });
    if let Err(error) = source.install(&pending, &bytes) {
        gate.reopen();
        log::warn!("update_install outcome=failed stage=install error={error}");
        service.fail(version);
        return;
    }
    log::info!("update_install outcome=restarting version={version}");
}

/// The one definition of idle for an install: no capture reservation or session, no
/// capture phase other than Idle, no active job, no pending delivery, and the quiet
/// period since the last of those ended.
pub(super) fn is_idle(
    capture_reserved: bool,
    work: Activity,
    deliveries: Activity,
    now: Instant,
) -> bool {
    if capture_reserved {
        return false;
    }
    let (Activity::Quiet(work_ended), Activity::Quiet(delivery_ended)) = (work, deliveries) else {
        return false;
    };
    work_ended
        .max(delivery_ended)
        .is_none_or(|ended| now.saturating_duration_since(ended) >= QUIET_PERIOD)
}

pub(crate) struct IdleGate {
    pub(super) capture: DesktopAudioCapture,
    pub(super) runtime: EchoRuntime,
    pub(super) deliveries: fn() -> Activity,
}

impl IdleGate {
    pub(super) fn is_idle_at(&self, now: Instant) -> bool {
        let work = self.runtime.activity().unwrap_or(Activity::Running);
        is_idle(
            self.capture.has_reservation(),
            work,
            (self.deliveries)(),
            now,
        )
    }

    /// The first read keeps a busy Echo from refusing captures every poll; the read after
    /// setting the barrier is the one that makes admission atomic.
    pub(super) fn try_close_at(&self, now: Instant) -> bool {
        if !self.is_idle_at(now) {
            return false;
        }
        self.capture.set_install_barrier(true);
        if self.is_idle_at(now) {
            return true;
        }
        self.capture.set_install_barrier(false);
        false
    }
}

impl InstallGate for IdleGate {
    fn try_close(&self) -> bool {
        self.try_close_at(Instant::now())
    }

    fn reopen(&self) {
        self.capture.set_install_barrier(false);
    }
}

pub(super) struct PluginSource<R: Runtime> {
    pub(super) app: AppHandle<R>,
    /// The limit on a whole check request. Tests shorten it to exercise a server that stalls.
    pub(super) check_timeout: Duration,
}

impl<R: Runtime> PluginSource<R> {
    pub(super) fn new(app: AppHandle<R>) -> Self {
        Self {
            app,
            check_timeout: CHECK_TIMEOUT,
        }
    }

    pub(super) fn updater(&self) -> tauri_plugin_updater::Result<Updater> {
        let https_only = !self.insecure_transport_configured();
        let app = self.app.clone();
        self.app
            .updater_builder()
            .no_proxy()
            .timeout(self.check_timeout)
            .configure_client(move |client| {
                client
                    .https_only(https_only)
                    .redirect(updater_reqwest::redirect::Policy::limited(MAX_REDIRECTS))
                    .connect_timeout(CONNECT_TIMEOUT)
            })
            .version_comparator(|current, release| {
                release.version > current && release.version.pre.is_empty()
            })
            .on_before_exit(move || {
                crate::app_setup::shutdown_application(&app);
                app.cleanup_before_exit();
            })
            .build()
    }

    /// Only the staging overlay sets this, to serve a loopback manifest over HTTP.
    fn insecure_transport_configured(&self) -> bool {
        self.app
            .config()
            .plugins
            .0
            .get("updater")
            .and_then(|updater| updater.get("dangerousInsecureTransportProtocol"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }
}

impl<R: Runtime> UpdateSource for PluginSource<R> {
    type Pending = Update;

    fn check(&self) -> CheckOutcome<Update> {
        let checked = self
            .updater()
            .and_then(|updater| tauri::async_runtime::block_on(updater.check()));
        match checked {
            // Every endpoint answered without a manifest, as before the first release carries one.
            Ok(None) | Err(tauri_plugin_updater::Error::ReleaseNotFound) => CheckOutcome::None,
            Ok(Some(update)) => CheckOutcome::Available {
                version: update.version.clone(),
                pending: update,
            },
            Err(error) => CheckOutcome::Failed(error.to_string()),
        }
    }

    fn download(
        &self,
        pending: &Update,
        on_chunk: &mut dyn FnMut(usize, Option<u64>),
    ) -> Result<Vec<u8>, String> {
        let mut update = pending.clone();
        update.timeout = Some(DOWNLOAD_TIMEOUT);
        tauri::async_runtime::block_on(update.download(on_chunk, || {}))
            .map_err(|error| error.to_string())
    }

    fn install(&self, pending: &Update, bytes: &[u8]) -> Result<(), String> {
        pending.install(bytes).map_err(|error| error.to_string())?;
        #[cfg(target_os = "macos")]
        self.app.request_restart();
        Ok(())
    }
}
