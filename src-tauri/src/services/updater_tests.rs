use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
#[cfg(echo_update_install)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(echo_update_install)]
use std::time::Instant;
use std::time::{Duration, SystemTime};

use tauri::test::MockRuntime;
use tauri::Manager;
#[cfg(echo_update_install)]
use tauri_plugin_updater::Update;

#[cfg(echo_update_install)]
use super::install::*;
use super::*;
#[cfg(echo_update_install)]
use crate::audio::tests::fixtures::{owner, spawn_coordinator, FakeBackend};
#[cfg(echo_update_install)]
use crate::audio::{
    AudioCapturePolicy, CancelCapture, CaptureErrorCode, CaptureEvent, CaptureIntent, CapturePhase,
    CapturePurpose, DesktopAudioCapture,
};
#[cfg(echo_update_install)]
use crate::services::runtime::{Activity, EchoErrorCode, EchoRuntime};

const ARTIFACT: &[u8] = include_bytes!("updater_fixtures/artifact.bin");
const ARTIFACT_SIGNATURE: &str = include_str!("updater_fixtures/artifact.bin.sig");
#[cfg(echo_update_install)]
const TRUSTED_KEY: &str = include_str!("updater_fixtures/trusted.pub");
#[cfg(echo_update_install)]
const OTHER_KEY: &str = include_str!("updater_fixtures/other.pub");

type Published = Arc<Mutex<Vec<UpdateEnvelope>>>;

fn service<P: Clone>() -> (UpdaterService<P>, Published) {
    let published: Published = Arc::default();
    let sink = Arc::clone(&published);
    let service = UpdaterService::new(move |envelope| sink.lock().unwrap().push(envelope.clone()));
    (service, published)
}

fn available(version: &str) -> UpdateStatus {
    UpdateStatus::Available {
        version: version.to_owned(),
    }
}

fn downloading(downloaded_bytes: u64, total_bytes: Option<u64>) -> UpdateStatus {
    UpdateStatus::Downloading {
        version: "0.5.1".to_owned(),
        downloaded_bytes,
        total_bytes,
    }
}

fn version_state(state: &str) -> UpdateStatus {
    let version = "0.5.1".to_owned();
    match state {
        "waiting_for_idle" => UpdateStatus::WaitingForIdle { version },
        "installing" => UpdateStatus::Installing { version },
        "failed" => UpdateStatus::Failed { version },
        _ => unreachable!("{state}"),
    }
}

#[derive(Default)]
struct FakeSource {
    checks: Mutex<VecDeque<CheckOutcome<&'static str>>>,
    check_calls: AtomicUsize,
    #[cfg(echo_update_install)]
    chunks: Vec<usize>,
    #[cfg(echo_update_install)]
    total: Option<u64>,
    #[cfg(echo_update_install)]
    download_errors: Mutex<VecDeque<String>>,
    #[cfg(echo_update_install)]
    download_calls: AtomicUsize,
    #[cfg(echo_update_install)]
    install_error: Option<String>,
    #[cfg(echo_update_install)]
    installs: Mutex<Vec<(&'static str, Vec<u8>)>>,
}

impl FakeSource {
    #[cfg(echo_update_install)]
    fn offering(version: &str) -> Self {
        let source = Self {
            chunks: vec![1000; 4],
            total: Some(4000),
            ..Self::default()
        };
        source.queue_check(CheckOutcome::Available {
            version: version.to_owned(),
            pending: "release",
        });
        source
    }

    fn queue_check(&self, outcome: CheckOutcome<&'static str>) {
        self.checks.lock().unwrap().push_back(outcome);
    }
}

impl UpdateSource for FakeSource {
    type Pending = &'static str;

    fn check(&self) -> CheckOutcome<&'static str> {
        self.check_calls.fetch_add(1, Ordering::SeqCst);
        self.checks
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(CheckOutcome::None)
    }

    #[cfg(echo_update_install)]
    fn download(
        &self,
        _pending: &&'static str,
        on_chunk: &mut dyn FnMut(usize, Option<u64>),
    ) -> Result<Vec<u8>, String> {
        self.download_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.download_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        for &chunk in &self.chunks {
            on_chunk(chunk, self.total);
        }
        Ok(vec![7; self.chunks.iter().sum()])
    }

    #[cfg(echo_update_install)]
    fn install(&self, pending: &&'static str, bytes: &[u8]) -> Result<(), String> {
        self.installs
            .lock()
            .unwrap()
            .push((pending, bytes.to_vec()));
        self.install_error.clone().map_or(Ok(()), Err)
    }
}

#[cfg(echo_update_install)]
/// Refuses `refusals` times, then closes. `hold` keeps it refusing until released.
#[derive(Default)]
struct FakeGate {
    refusals: AtomicUsize,
    hold: AtomicBool,
    attempts: AtomicUsize,
    closed: AtomicBool,
    reopened: AtomicUsize,
}

#[cfg(echo_update_install)]
impl InstallGate for FakeGate {
    fn try_close(&self) -> bool {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.hold.load(Ordering::SeqCst) {
            return false;
        }
        let refused = self
            .refusals
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        self.closed.store(!refused, Ordering::SeqCst);
        !refused
    }

    fn reopen(&self) {
        self.reopened.fetch_add(1, Ordering::SeqCst);
        self.closed.store(false, Ordering::SeqCst);
    }
}

#[cfg(echo_update_install)]
fn install<S: UpdateSource>(service: &UpdaterService<S::Pending>, source: &S, gate: &FakeGate) {
    let (version, pending) = service.admit_install().expect("install admitted");
    run_install(
        service,
        source,
        gate,
        Duration::from_millis(1),
        version,
        pending,
    );
}

fn statuses(published: &Published) -> Vec<UpdateStatus> {
    published
        .lock()
        .unwrap()
        .iter()
        .map(|envelope| envelope.status.clone())
        .collect()
}

fn assert_sequences_rise_by_one(published: &Published) {
    let sequences: Vec<u64> = published
        .lock()
        .unwrap()
        .iter()
        .map(|envelope| envelope.sequence)
        .collect();
    let expected: Vec<u64> = (1..=sequences.len() as u64).collect();
    assert_eq!(sequences, expected);
}

// Keep: contract pin for the dashboard's command responses and echo-update-status payload.
#[test]
fn status_envelope_matches_the_dashboard_contract() {
    let cases = [
        (UpdateStatus::None, serde_json::json!({"state": "none"})),
        (
            available("0.5.1"),
            serde_json::json!({"state": "available", "version": "0.5.1"}),
        ),
        (
            downloading(1024, Some(4096)),
            serde_json::json!({"state": "downloading", "version": "0.5.1",
                "downloadedBytes": 1024, "totalBytes": 4096}),
        ),
        (
            downloading(0, None),
            serde_json::json!({"state": "downloading", "version": "0.5.1",
                "downloadedBytes": 0, "totalBytes": null}),
        ),
        (
            version_state("waiting_for_idle"),
            serde_json::json!({"state": "waiting_for_idle", "version": "0.5.1"}),
        ),
        (
            version_state("installing"),
            serde_json::json!({"state": "installing", "version": "0.5.1"}),
        ),
        (
            version_state("failed"),
            serde_json::json!({"state": "failed", "version": "0.5.1"}),
        ),
    ];
    for (status, expected) in cases {
        let envelope = UpdateEnvelope {
            sequence: 3,
            status,
        };
        assert_eq!(
            serde_json::to_value(envelope).unwrap(),
            serde_json::json!({"sequence": 3, "status": expected})
        );
    }
}

// Keep: none and failed checks show nothing, and a failed check keeps an existing offer.
#[cfg(echo_update_install)]
#[test]
fn checks_offer_a_release_and_failures_keep_the_status_exactly() {
    let (service, published) = service();
    let source = FakeSource::default();
    for outcome in [CheckOutcome::None, CheckOutcome::Failed("timeout".into())] {
        source.queue_check(outcome);
        check_once(&service, &source);
        assert_eq!(service.status().status, UpdateStatus::None);
        assert_eq!(service.tray_offer(), TrayOffer::Hidden);
    }
    assert!(published.lock().unwrap().is_empty(), "nothing to show");

    source.queue_check(CheckOutcome::Available {
        version: "0.5.1".into(),
        pending: "release",
    });
    check_once(&service, &source);
    assert_eq!(service.status().status, available("0.5.1"));
    assert_eq!(service.tray_offer(), TrayOffer::Enabled);

    source.queue_check(CheckOutcome::Failed("malformed".into()));
    check_once(&service, &source);
    assert_eq!(
        service.status(),
        UpdateEnvelope {
            sequence: 1,
            status: available("0.5.1")
        }
    );

    source.queue_check(CheckOutcome::None);
    check_once(&service, &source);
    assert_eq!(
        statuses(&published),
        [available("0.5.1"), UpdateStatus::None]
    );
    assert_sequences_rise_by_one(&published);
}

// Keep: an admitted install reports rising progress, then installing, and installs once.
#[cfg(echo_update_install)]
#[test]
fn install_reports_rising_progress_then_installs_once() {
    for (chunks, total, expected) in [
        (
            vec![1000; 4],
            Some(4000),
            vec![
                downloading(0, None),
                downloading(1000, Some(4000)),
                downloading(2000, Some(4000)),
                downloading(3000, Some(4000)),
                downloading(4000, Some(4000)),
            ],
        ),
        // Without a length, one change per MiB rather than per chunk.
        (
            vec![512 * 1024; 5],
            None,
            vec![
                downloading(0, None),
                downloading(512 * 1024, None),
                downloading(1024 * 1024, None),
                downloading(2 * 1024 * 1024, None),
            ],
        ),
    ] {
        let (service, published) = service();
        let source = FakeSource {
            chunks,
            total,
            ..FakeSource::offering("0.5.1")
        };
        check_once(&service, &source);
        published.lock().unwrap().clear();
        let gate = FakeGate::default();

        install(&service, &source, &gate);

        let mut expected = expected;
        expected.push(version_state("installing"));
        assert_eq!(statuses(&published), expected);
        let installs = source.installs.lock().unwrap();
        assert_eq!(installs.len(), 1);
        assert_eq!(installs[0].0, "release");
        assert_eq!(installs[0].1.len(), source.chunks.iter().sum::<usize>());
        assert!(gate.closed.load(Ordering::SeqCst));
    }
}

// Keep: a download or verification failure installs nothing, and a retry downloads again.
#[cfg(echo_update_install)]
#[test]
fn failed_download_installs_nothing_and_a_retry_downloads_again() {
    let (service, _) = service();
    let source = FakeSource::offering("0.5.1");
    source
        .download_errors
        .lock()
        .unwrap()
        .push_back("signature mismatch".into());
    check_once(&service, &source);
    let gate = FakeGate::default();

    install(&service, &source, &gate);
    assert_eq!(service.status().status, version_state("failed"));
    assert_eq!(service.tray_offer(), TrayOffer::Enabled);
    assert!(source.installs.lock().unwrap().is_empty());
    assert_eq!(gate.attempts.load(Ordering::SeqCst), 0);

    install(&service, &source, &gate);
    assert_eq!(source.download_calls.load(Ordering::SeqCst), 2);
    assert_eq!(source.installs.lock().unwrap().len(), 1);
}

// Keep: while one install runs, further requests and scheduled checks change nothing.
#[cfg(echo_update_install)]
#[test]
fn repeated_requests_during_an_install_start_one_download() {
    let (service, published) = service();
    let source = Arc::new(FakeSource::offering("0.5.1"));
    check_once(&service, source.as_ref());
    let gate = Arc::new(FakeGate {
        hold: AtomicBool::new(true),
        ..FakeGate::default()
    });
    let (version, pending) = service.admit_install().unwrap();
    assert_eq!(service.tray_offer(), TrayOffer::Disabled);
    assert!(service.admit_install().is_none(), "downloading");

    let worker = {
        let (service, source, gate) = (service.clone(), Arc::clone(&source), Arc::clone(&gate));
        std::thread::spawn(move || {
            run_install(
                &service,
                source.as_ref(),
                gate.as_ref(),
                Duration::from_millis(1),
                version,
                pending,
            )
        })
    };
    while service.status().status != version_state("waiting_for_idle") {
        std::thread::yield_now();
    }
    let waiting = service.status();
    assert!(service.admit_install().is_none(), "waiting for idle");
    source.queue_check(CheckOutcome::None);
    check_once(&service, source.as_ref());
    assert_eq!(
        source.check_calls.load(Ordering::SeqCst),
        1,
        "no check while busy"
    );
    assert_eq!(service.status(), waiting);
    assert_eq!(service.tray_offer(), TrayOffer::Disabled);

    gate.hold.store(false, Ordering::SeqCst);
    worker.join().unwrap();
    assert_eq!(service.status().status, version_state("installing"));
    assert_eq!(service.tray_offer(), TrayOffer::Disabled);
    assert!(service.admit_install().is_none(), "installing");
    assert_eq!(source.download_calls.load(Ordering::SeqCst), 1);
    assert_eq!(source.installs.lock().unwrap().len(), 1);
    assert_sequences_rise_by_one(&published);
}

// Keep: a busy Echo gets waiting_for_idle and no install until the gate closes; an install
// failure reopens capture admission and leaves the offer retryable.
#[cfg(echo_update_install)]
#[test]
fn install_waits_for_idle_and_a_failed_install_reopens_capture() {
    let (service, published) = service();
    let source = FakeSource {
        install_error: Some("disk full".into()),
        ..FakeSource::offering("0.5.1")
    };
    check_once(&service, &source);
    let gate = FakeGate {
        refusals: AtomicUsize::new(3),
        ..FakeGate::default()
    };

    install(&service, &source, &gate);

    assert_eq!(gate.attempts.load(Ordering::SeqCst), 4);
    let statuses = statuses(&published);
    let waiting = statuses
        .iter()
        .position(|status| *status == version_state("waiting_for_idle"))
        .expect("waiting_for_idle published");
    assert_eq!(statuses[waiting + 1], version_state("installing"));
    assert_eq!(statuses.last(), Some(&version_state("failed")));
    assert_eq!(source.installs.lock().unwrap().len(), 1);
    assert_eq!(gate.reopened.load(Ordering::SeqCst), 1);
    assert!(!gate.closed.load(Ordering::SeqCst));
    assert!(service.admit_install().is_some(), "failed is retryable");
}

// Keep: idle needs no reservation, no running work or delivery, and ten quiet seconds
// after the last of them, so a paste delivered 5 s after its job still lands first.
#[cfg(echo_update_install)]
#[test]
fn idle_requires_no_work_and_ten_quiet_seconds() {
    let job_done = Instant::now();
    let pasted = job_done + Duration::from_secs(6);
    let quiet = Activity::Quiet;
    let cases = [
        (
            "fresh launch",
            false,
            quiet(None),
            quiet(None),
            job_done,
            true,
        ),
        (
            "reservation",
            true,
            quiet(None),
            quiet(None),
            job_done,
            false,
        ),
        (
            "capture or job",
            false,
            Activity::Running,
            quiet(None),
            job_done,
            false,
        ),
        (
            "delivery",
            false,
            quiet(None),
            Activity::Running,
            job_done,
            false,
        ),
        (
            "9 s after a job",
            false,
            quiet(Some(job_done)),
            quiet(None),
            job_done + Duration::from_secs(9),
            false,
        ),
        (
            "10 s after a job",
            false,
            quiet(Some(job_done)),
            quiet(None),
            job_done + Duration::from_secs(10),
            true,
        ),
        (
            "renderer delivering 5 s after the job",
            false,
            quiet(Some(job_done)),
            Activity::Running,
            job_done + Duration::from_secs(5),
            false,
        ),
        (
            "12 s after the job, 6 s after its paste",
            false,
            quiet(Some(job_done)),
            quiet(Some(pasted)),
            job_done + Duration::from_secs(12),
            false,
        ),
        (
            "10 s after the paste",
            false,
            quiet(Some(job_done)),
            quiet(Some(pasted)),
            pasted + Duration::from_secs(10),
            true,
        ),
    ];
    for (case, reserved, work, deliveries, now, expected) in cases {
        assert_eq!(is_idle(reserved, work, deliveries, now), expected, "{case}");
    }
}

#[cfg(echo_update_install)]
fn quiet_deliveries() -> Activity {
    Activity::Quiet(None)
}

#[cfg(echo_update_install)]
fn idle_gate(capture: &DesktopAudioCapture, runtime: &EchoRuntime) -> IdleGate {
    IdleGate {
        capture: capture.clone(),
        runtime: runtime.clone(),
        deliveries: quiet_deliveries,
    }
}

#[cfg(echo_update_install)]
fn reserve(capture: &DesktopAudioCapture, intent_id: &str) -> Result<(), CaptureErrorCode> {
    capture
        .reserve(
            owner("overlay"),
            CaptureIntent {
                intent_id: intent_id.to_owned(),
                purpose: CapturePurpose::PushToTalk,
            },
        )
        .map(|_| ())
        .map_err(|error| error.code)
}

#[cfg(echo_update_install)]
fn cancel(capture: &DesktopAudioCapture, intent_id: &str) {
    capture
        .cancel(CancelCapture {
            owner: owner("overlay"),
            intent_id: intent_id.to_owned(),
            session_id: None,
            reason: "user_cancelled",
        })
        .unwrap();
}

#[cfg(echo_update_install)]
fn capture_event(phase: CapturePhase) -> CaptureEvent {
    CaptureEvent {
        elapsed_ms: 0,
        limit_ms: 0,
        sequence: 1,
        phase,
        purpose: Some(CapturePurpose::PushToTalk),
        intent_id: Some("intent".into()),
        session_id: None,
        artifact_id: None,
        outcome: "test",
        error: None,
        has_observed_callback: false,
        callback_count: 0,
        sample_count: 0,
        last_error: None,
        artifact_count: 0,
        artifact_bytes: 0,
    }
}

// Keep: the gate reads the real reservation and runtime state and waits out the quiet
// period after a capture or job ends.
#[cfg(echo_update_install)]
#[test]
fn idle_gate_waits_for_reservations_captures_jobs_and_the_quiet_period() {
    let (backend, _, _, _) = FakeBackend::blocked();
    let capture = spawn_coordinator(backend, AudioCapturePolicy::default());
    let runtime = EchoRuntime::default();
    let gate = idle_gate(&capture, &runtime);
    let later = || Instant::now() + Duration::from_secs(11);

    reserve(&capture, "held").unwrap();
    assert!(!gate.try_close_at(later()), "reservation");
    cancel(&capture, "held");

    runtime
        .apply_capture_event(&capture_event(CapturePhase::Recording))
        .unwrap();
    assert!(!gate.try_close_at(later()), "recording");
    runtime
        .apply_capture_event(&capture_event(CapturePhase::Idle))
        .unwrap();
    assert!(!gate.try_close_at(Instant::now()), "capture just ended");

    runtime.job_started_for_capture(1, "intent").unwrap();
    assert!(!gate.try_close_at(later()), "job");
    runtime
        .job_failed(1, EchoErrorCode::ProviderFailed)
        .unwrap();
    assert!(!gate.try_close_at(Instant::now()), "job just settled");

    assert!(gate.try_close_at(later()), "quiet period over");
    gate.reopen();
}

// Keep: a closed barrier refuses capture before the microphone opens; reopening it after a
// failed install lets the next capture start normally.
#[cfg(echo_update_install)]
#[test]
fn closed_barrier_refuses_capture_without_opening_the_microphone() {
    let (backend, open_started, release_open, _) = FakeBackend::blocked();
    let capture = spawn_coordinator(backend, AudioCapturePolicy::default());
    let runtime = EchoRuntime::default();
    let gate = idle_gate(&capture, &runtime);
    assert!(gate.try_close_at(Instant::now()));

    let refused = capture
        .reserve(
            owner("overlay"),
            CaptureIntent {
                intent_id: "during-install".into(),
                purpose: CapturePurpose::PushToTalk,
            },
        )
        .unwrap_err();
    assert_eq!(refused.code, CaptureErrorCode::UpdateInstalling);
    assert!(refused.retryable);
    assert_eq!(
        serde_json::to_value(refused.code).unwrap(),
        "update_installing"
    );
    assert!(!capture.has_reservation());
    assert!(open_started.try_recv().is_err(), "microphone opened");

    gate.reopen();
    release_open.send(()).unwrap();
    let session = crate::audio::tests::fixtures::begin(&capture, owner("overlay"), "after-failure");
    assert_eq!(session.phase, CapturePhase::Recording);
    open_started
        .try_recv()
        .expect("microphone opened after reopening");
}

#[cfg(echo_update_install)]
static INTERLEAVED_CAPTURE: Mutex<Option<DesktopAudioCapture>> = Mutex::new(None);

/// Reserves on the shared capture during the gate's first idle read, as a capture start
/// landing between that read and the barrier would.
#[cfg(echo_update_install)]
fn deliveries_while_a_capture_starts() -> Activity {
    if let Some(capture) = INTERLEAVED_CAPTURE.lock().unwrap().take() {
        reserve(&capture, "interleaved").unwrap();
    }
    Activity::Quiet(None)
}

// Keep: the barrier race, the order closed_barrier_refuses_capture does not cover. A
// capture reserved after the gate's first idle read but before the barrier is seen by
// the read after the barrier, so the install keeps waiting and the capture runs.
#[cfg(echo_update_install)]
#[test]
fn capture_reserved_before_the_barrier_keeps_the_install_waiting() {
    let (backend, _, _, _) = FakeBackend::blocked();
    let capture = spawn_coordinator(backend, AudioCapturePolicy::default());
    *INTERLEAVED_CAPTURE.lock().unwrap() = Some(capture.clone());
    let gate = IdleGate {
        capture: capture.clone(),
        runtime: EchoRuntime::default(),
        deliveries: deliveries_while_a_capture_starts,
    };

    assert!(!gate.try_close_at(Instant::now()));
    assert!(capture.has_reservation(), "the capture proceeds");
    cancel(&capture, "interleaved");
    assert_eq!(reserve(&capture, "next"), Ok(()), "barrier left set");
}

// Keep: checks run at launch and on the first 5-minute tick at which wall time is a day
// past the last check, including after system sleep and a clock moved back.
#[test]
fn schedule_checks_at_launch_and_once_wall_time_passes_a_day() {
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let minutes = |value: u64| Duration::from_secs(value * 60);
    const HOUR: i64 = 3600;
    // Wall seconds a tick adds beyond its own five minutes, keyed by tick number, and the
    // expected check times in seconds from launch.
    let cases = [
        ("awake", Vec::<(usize, i64)>::new(), 300, vec![0, 24 * HOUR]),
        (
            "system asleep for 30 h during the tenth tick",
            vec![(10, 30 * HOUR)],
            12,
            vec![0, 50 * 60 + 30 * HOUR],
        ),
        (
            "clock moved back a day",
            vec![(2, -24 * HOUR)],
            4,
            vec![0, 10 * 60 - 24 * HOUR],
        ),
    ];
    for (case, jumps, ticks, expected) in cases {
        let (service, _) = service();
        let source = FakeSource::default();
        let clock = Mutex::new((start, 0usize));
        let checked = Mutex::new(Vec::new());
        let tracking = TrackingSource {
            inner: &source,
            on_check: || checked.lock().unwrap().push(clock.lock().unwrap().0),
        };
        run_schedule(
            &service,
            &tracking,
            || clock.lock().unwrap().0,
            |tick| {
                assert_eq!(tick, minutes(5));
                let mut clock = clock.lock().unwrap();
                clock.1 += 1;
                let extra = jumps
                    .iter()
                    .find(|(at, _)| *at == clock.1)
                    .map_or(0, |(_, seconds)| *seconds);
                let advanced = clock.0 + tick;
                clock.0 = if extra >= 0 {
                    advanced + Duration::from_secs(extra as u64)
                } else {
                    advanced - Duration::from_secs(extra.unsigned_abs())
                };
                clock.1 < ticks
            },
        );
        let offsets: Vec<i64> = checked
            .lock()
            .unwrap()
            .iter()
            .map(|time| match time.duration_since(start) {
                Ok(after) => after.as_secs() as i64,
                Err(before) => -(before.duration().as_secs() as i64),
            })
            .collect();
        assert_eq!(offsets, expected, "{case}");
    }
}

struct TrackingSource<'a, F: Fn()> {
    inner: &'a FakeSource,
    on_check: F,
}

impl<F: Fn()> UpdateSource for TrackingSource<'_, F> {
    type Pending = &'static str;

    fn check(&self) -> CheckOutcome<&'static str> {
        (self.on_check)();
        self.inner.check()
    }

    #[cfg(echo_update_install)]
    fn download(
        &self,
        pending: &&'static str,
        on_chunk: &mut dyn FnMut(usize, Option<u64>),
    ) -> Result<Vec<u8>, String> {
        self.inner.download(pending, on_chunk)
    }

    #[cfg(echo_update_install)]
    fn install(&self, pending: &&'static str, bytes: &[u8]) -> Result<(), String> {
        self.inner.install(pending, bytes)
    }
}

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn body(body: impl Into<Vec<u8>>) -> Self {
        Self {
            body: body.into(),
            ..Self::status(200)
        }
    }

    fn redirect(location: String) -> Self {
        Self {
            headers: vec![("Location", location)],
            ..Self::status(302)
        }
    }
}

/// A plain HTTP server on 127.0.0.1 that records each request path. Needs permission
/// to bind a loopback listener.
struct Loopback {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Loopback {
    fn start(route: impl Fn(&str, &str) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let (served_base, recorded) = (base.clone(), Arc::clone(&requests));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut header = String::new();
                while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
                    header.clear();
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_owned();
                recorded.lock().unwrap().push(path.clone());
                let reply = route(&served_base, &path);
                let mut head = format!(
                    "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (name, value) in reply.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&reply.body);
            }
        });
        Self { base, requests }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn manifest(version: &str, artifact_url: &str) -> Vec<u8> {
    #[cfg(echo_update_install)]
    let target = tauri_plugin_updater::target().unwrap();
    // A notify build reads only `version`; the entry keeps the published manifest's shape.
    #[cfg(not(echo_update_install))]
    let target = "windows-x86_64";
    serde_json::json!({
        "version": version,
        "platforms": {
            target: {"url": artifact_url, "signature": ARTIFACT_SIGNATURE.trim()}
        }
    })
    .to_string()
    .into_bytes()
}

#[cfg(echo_update_install)]
/// A mock app running the real updater plugin as an installed 0.5.0 would.
fn plugin_app(endpoints: &[String], pubkey: &str, insecure: bool) -> tauri::App<MockRuntime> {
    let mut context = tauri::test::mock_context(tauri::test::noop_assets());
    context.package_info_mut().version = "0.5.0".parse().unwrap();
    let mut updater = serde_json::json!({"pubkey": pubkey.trim(), "endpoints": endpoints});
    if insecure {
        updater["dangerousInsecureTransportProtocol"] = true.into();
    }
    context
        .config_mut()
        .plugins
        .0
        .insert("updater".into(), updater);
    tauri::test::mock_builder()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .build(context)
        .unwrap()
}

#[cfg(echo_update_install)]
fn plugin_check(endpoints: &[String], insecure: bool) -> CheckOutcome<Update> {
    let app = plugin_app(endpoints, TRUSTED_KEY, insecure);
    PluginSource::new(app.handle().clone()).check()
}

fn offered_version<P>(outcome: &CheckOutcome<P>) -> Option<&str> {
    match outcome {
        CheckOutcome::Available { version, .. } => Some(version),
        _ => None,
    }
}

// Keep: 404s from both endpoints read as no update, checked in configured order, and the
// second endpoint is used when the first has no manifest.
#[cfg(echo_update_install)]
#[test]
fn missing_manifests_read_as_no_update() {
    let server = Loopback::start(|base, path| match path {
        "/fallback/latest.json" => Reply::body(manifest("0.5.1", &format!("{base}/artifact"))),
        _ => Reply::status(404),
    });
    let endpoints = [
        server.url("/first/latest.json"),
        server.url("/second/latest.json"),
    ];
    assert!(matches!(plugin_check(&endpoints, true), CheckOutcome::None));
    assert_eq!(
        server.requests(),
        ["/first/latest.json", "/second/latest.json"]
    );

    let fallback = [
        server.url("/first/latest.json"),
        server.url("/fallback/latest.json"),
    ];
    assert_eq!(
        offered_version(&plugin_check(&fallback, true)),
        Some("0.5.1")
    );
}

// Keep: the real comparator offers only higher stable versions, never a downgrade,
// the same version or a prerelease.
#[cfg(echo_update_install)]
#[test]
fn only_a_higher_stable_release_is_offered() {
    let server = Loopback::start(|base, path| {
        let version = path
            .trim_start_matches('/')
            .trim_end_matches("/latest.json");
        Reply::body(manifest(version, &format!("{base}/artifact")))
    });
    for (version, offered) in [
        ("0.5.0", None),
        ("0.4.9", None),
        ("0.5.1-beta.1", None),
        ("0.5.1", Some("0.5.1")),
        ("1.0.0", Some("1.0.0")),
    ] {
        let endpoint = [server.url(&format!("/{version}/latest.json"))];
        assert_eq!(
            offered_version(&plugin_check(&endpoint, true)),
            offered,
            "{version}"
        );
    }
}

/// Serves `/redirect/<n>` (n more redirects, then a 0.5.1 offer) and `/malformed`; every
/// other path is 404.
fn release_server() -> Loopback {
    Loopback::start(|base, path| {
        if let Some(hops) = path.strip_prefix("/redirect/") {
            let hops: u32 = hops.parse().unwrap();
            return match hops {
                0 => Reply::body(manifest("0.5.1", &format!("{base}/artifact"))),
                _ => Reply::redirect(format!("{base}/redirect/{}", hops - 1)),
            };
        }
        match path {
            "/malformed" => Reply::body("{\"version\": "),
            _ => Reply::status(404),
        }
    })
}

/// An https:// endpoint whose server answers each TLS ClientHello with a fatal
/// handshake_failure alert, so the TCP connection succeeds and the TLS handshake fails.
/// The counter records the ClientHellos it received.
fn tls_refusing_endpoint() -> (String, Arc<AtomicUsize>) {
    const HANDSHAKE_RECORD: u8 = 22;
    // Content type alert (21), TLS 1.2, length 2, level fatal (2), handshake_failure (40).
    const FATAL_HANDSHAKE_FAILURE: [u8; 7] = [21, 3, 3, 0, 2, 2, 40];
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
    let endpoint = format!("https://{}/latest.json", listener.local_addr().unwrap());
    let hellos = Arc::new(AtomicUsize::new(0));
    let received = Arc::clone(&hellos);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut header = [0u8; 5];
            if stream.read_exact(&mut header).is_err() || header[0] != HANDSHAKE_RECORD {
                continue;
            }
            // Read the whole record first: closing with unread bytes resets the connection
            // before the client reads the alert.
            let mut hello = vec![0u8; usize::from(u16::from_be_bytes([header[3], header[4]]))];
            if stream.read_exact(&mut hello).is_err() {
                continue;
            }
            received.fetch_add(1, Ordering::SeqCst);
            let _ = stream.write_all(&FATAL_HANDSHAKE_FAILURE);
            let _ = stream.shutdown(std::net::Shutdown::Write);
            let _ = stream.read_to_end(&mut Vec::new());
        }
    });
    (endpoint, hellos)
}

/// The check timeout the failure cases use, so a timeout costs this instead of 30 s.
const SHORT_CHECK_TIMEOUT: Duration = Duration::from_millis(250);

struct FailingCheck {
    case: &'static str,
    endpoint: String,
    insecure: bool,
}

/// One check failure of each kind a check must survive without changing the status.
/// `release` is a [`release_server`]. The timeout case is a server that offers 0.5.1 only
/// after 3 s, so a check that ignored its timeout would read the offer instead of failing.
fn failing_checks(release: &Loopback) -> (Vec<FailingCheck>, Arc<AtomicUsize>) {
    let slow = Loopback::start(|base, _| {
        std::thread::sleep(Duration::from_secs(3));
        Reply::body(manifest("0.5.1", &format!("{base}/artifact")))
    });
    let (tls_endpoint, tls_hellos) = tls_refusing_endpoint();
    let closed_port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/latest.json", listener.local_addr().unwrap())
    };
    let case = |case, endpoint, insecure| FailingCheck {
        case,
        endpoint,
        insecure,
    };
    let cases = vec![
        case("timeout", slow.url("/latest.json"), true),
        case("TLS handshake failure", tls_endpoint, false),
        case("malformed JSON", release.url("/malformed"), true),
        case("six redirects", release.url("/redirect/6"), true),
        case(
            "http under the shipped config",
            release.url("/redirect/0"),
            false,
        ),
        case("connection refused", closed_port, true),
    ];
    (cases, tls_hellos)
}

#[cfg(echo_update_install)]
/// The real plugin source for `failing`, with the short check timeout.
fn failing_source(failing: &FailingCheck) -> (tauri::App<MockRuntime>, PluginSource<MockRuntime>) {
    let app = plugin_app(
        std::slice::from_ref(&failing.endpoint),
        TRUSTED_KEY,
        failing.insecure,
    );
    let source = PluginSource {
        check_timeout: SHORT_CHECK_TIMEOUT,
        ..PluginSource::new(app.handle().clone())
    };
    (app, source)
}

// Keep: a timeout, a TLS handshake failure, malformed JSON, a sixth redirect, an http:// hop
// under the shipped config and a refused connection each fail the check through the real
// plugin and leave the envelope unchanged, so nothing is published: none stays none, and an
// existing offer stays available with its version. Five redirects still reach the manifest.
#[cfg(echo_update_install)]
#[test]
fn check_failures_keep_the_status_and_redirects_stop_after_five() {
    let release = release_server();
    assert_eq!(
        offered_version(&plugin_check(&[release.url("/redirect/5")], true)),
        Some("0.5.1")
    );
    let offering = plugin_app(&[release.url("/redirect/0")], TRUSTED_KEY, true);
    let (cases, tls_hellos) = failing_checks(&release);
    for failing in &cases {
        for offered in [false, true] {
            let (service, _) = service::<Update>();
            if offered {
                check_once(&service, &PluginSource::new(offering.handle().clone()));
                assert_eq!(service.status().status, available("0.5.1"));
            }
            let before = service.status();
            let requests_before = release.requests().len();

            let (_app, source) = failing_source(failing);
            check_once(&service, &source);

            let case = failing.case;
            assert_eq!(service.status(), before, "{case}, offered: {offered}");
            if case == "http under the shipped config" {
                assert_eq!(
                    release.requests().len(),
                    requests_before,
                    "{case} sent a request"
                );
            }
        }
    }
    assert_eq!(
        tls_hellos.load(Ordering::SeqCst),
        2,
        "TLS handshakes attempted"
    );
}

// Keep: a shipped check gives up after 30 s; the failure cases above shorten it.
#[cfg(echo_update_install)]
#[test]
fn shipped_checks_time_out_after_30_seconds() {
    let app = plugin_app(&[], TRUSTED_KEY, false);
    assert_eq!(
        PluginSource::new(app.handle().clone()).check_timeout,
        Duration::from_secs(30)
    );
}

#[cfg(echo_update_install)]
/// Delegates to the real plugin but never installs, so a test cannot replace anything.
struct NeverInstalls<S> {
    inner: S,
    installs: AtomicUsize,
}

#[cfg(echo_update_install)]
impl<S: UpdateSource> UpdateSource for NeverInstalls<S> {
    type Pending = S::Pending;

    fn check(&self) -> CheckOutcome<S::Pending> {
        self.inner.check()
    }

    fn download(
        &self,
        pending: &S::Pending,
        on_chunk: &mut dyn FnMut(usize, Option<u64>),
    ) -> Result<Vec<u8>, String> {
        self.inner.download(pending, on_chunk)
    }

    fn install(&self, _pending: &S::Pending, _bytes: &[u8]) -> Result<(), String> {
        self.installs.fetch_add(1, Ordering::SeqCst);
        Err("tests never install".into())
    }
}

// Keep: tampered bytes, a foreign signing key or an http:// artifact under the shipped
// config fail the install at download, with nothing installed; the untampered artifact
// verifies against the configured key.
#[cfg(echo_update_install)]
#[test]
fn tampered_or_foreign_artifacts_fail_without_installing() {
    let server = Loopback::start(|base, path| match path {
        "/latest.json" => Reply::body(manifest("0.5.1", &format!("{base}/artifact"))),
        "/tampered/latest.json" => Reply::body(manifest("0.5.1", &format!("{base}/tampered"))),
        "/artifact" => Reply::body(ARTIFACT),
        "/tampered" => {
            let mut bytes = ARTIFACT.to_vec();
            bytes[0] ^= 1;
            Reply::body(bytes)
        }
        _ => Reply::status(404),
    });
    let check = |path: &str, key: &str| {
        let app = plugin_app(&[server.url(path)], key, true);
        let source = PluginSource::new(app.handle().clone());
        let CheckOutcome::Available { pending, .. } = source.check() else {
            panic!("{path} offers 0.5.1");
        };
        (app, source, pending)
    };

    let (_app, source, pending) = check("/latest.json", TRUSTED_KEY);
    assert_eq!(source.download(&pending, &mut |_, _| {}).unwrap(), ARTIFACT);

    for (case, path, key) in [
        ("tampered bytes", "/tampered/latest.json", TRUSTED_KEY),
        ("foreign signing key", "/latest.json", OTHER_KEY),
    ] {
        let (_app, source, _) = check(path, key);
        let guarded = NeverInstalls {
            inner: source,
            installs: AtomicUsize::new(0),
        };
        let (service, _) = service::<Update>();
        check_once(&service, &guarded);
        let gate = FakeGate::default();
        install(&service, &guarded, &gate);
        assert_eq!(service.status().status, version_state("failed"), "{case}");
        assert_eq!(guarded.installs.load(Ordering::SeqCst), 0, "{case}");
        assert_eq!(gate.attempts.load(Ordering::SeqCst), 0, "{case}");
    }

    // An http:// artifact under the shipped config: restore_update makes no request, so the
    // offer exists without a TLS loopback, and the download must refuse the hop.
    let shipped = plugin_app(&[server.url("/latest.json")], TRUSTED_KEY, false);
    let source = PluginSource::new(shipped.handle().clone());
    let offer = serde_json::from_slice(&manifest("0.5.1", &server.url("/artifact"))).unwrap();
    let update = source
        .updater()
        .unwrap()
        .restore_update(offer)
        .unwrap()
        .expect("0.5.1 offered");
    let artifact_requests = || {
        server
            .requests()
            .iter()
            .filter(|p| *p == "/artifact")
            .count()
    };
    let before = artifact_requests();
    assert!(source.download(&update, &mut |_, _| {}).is_err());
    assert_eq!(artifact_requests(), before, "http artifact requested");
}

// Keep: a debug build, as `bun run tauri:dev` runs, never contacts an endpoint.
#[cfg(echo_update_install)]
#[test]
fn debug_builds_never_contact_the_endpoints() {
    let server = Loopback::start(|_, _| Reply::status(404));
    let app = plugin_app(&[server.url("/latest.json")], TRUSTED_KEY, true);
    app.manage(UpdaterService::<Update>::new(|_| {}));

    start(app.handle());
    std::thread::sleep(Duration::from_millis(500));

    assert!(server.requests().is_empty());
    assert_eq!(
        app.state::<UpdaterService>().status().status,
        UpdateStatus::None
    );
}

// Keep: the log lines Alex reviews. The log facade accepts one logger per process, so
// this runs in a child process.
#[cfg(echo_update_install)]
#[test]
fn checks_log_none_for_missing_manifests_and_failed_for_errors() {
    const CHILD: &str = "ECHO_TEST_UPDATE_CHECK_LOG_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        let status = crate::test_support::clear_child_environment(&mut command)
            .env(CHILD, "1")
            .args([
                "--exact",
                "services::updater::tests::checks_log_none_for_missing_manifests_and_failed_for_errors",
                "--nocapture",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    struct Records(Mutex<Vec<String>>);
    static RECORDS: Records = Records(Mutex::new(Vec::new()));
    impl log::Log for Records {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            let line = record.args().to_string();
            if line.starts_with("update_check") {
                self.0.lock().unwrap().push(line);
            }
        }
        fn flush(&self) {}
    }
    log::set_logger(&RECORDS).unwrap();
    log::set_max_level(log::LevelFilter::Info);

    let release = release_server();
    let missing = plugin_app(&[release.url("/latest.json")], TRUSTED_KEY, true);
    check_once(
        &service::<Update>().0,
        &PluginSource::new(missing.handle().clone()),
    );
    let (cases, _) = failing_checks(&release);
    for failing in &cases {
        let (_app, source) = failing_source(failing);
        check_once(&service::<Update>().0, &source);
    }
    let records = RECORDS.0.lock().unwrap().clone();
    assert_eq!(records.len(), 1 + cases.len(), "{records:?}");
    assert_eq!(records[0], "update_check outcome=none");
    for (failing, record) in cases.iter().zip(&records[1..]) {
        assert!(
            record.starts_with("update_check outcome=failed error="),
            "{}: {record}",
            failing.case
        );
    }
}

// Keep: the native half of the loop across layers. A dashboard reads none over real IPC, a
// check against a loopback release offers 0.5.1, the install downloads and verifies the
// signed artifact with progress, waits while a capture is reserved, then installs; the
// stand-in installer fails, so capture admission reopens and the offer reads failed.
#[cfg(echo_update_install)]
#[test]
fn update_loop_walks_from_offer_to_install_over_loopback() {
    let server = Loopback::start(|base, path| match path {
        "/latest.json" => Reply::body(manifest("0.5.1", &format!("{base}/artifact"))),
        "/artifact" => Reply::body(ARTIFACT),
        _ => Reply::status(404),
    });
    let mut context = tauri::generate_context!(test = true);
    context.package_info_mut().version = "0.5.0".parse().unwrap();
    context.config_mut().plugins.0.insert(
        "updater".into(),
        serde_json::json!({
            "pubkey": TRUSTED_KEY.trim(),
            "endpoints": [server.url("/latest.json")],
            "dangerousInsecureTransportProtocol": true
        }),
    );
    let (service, published) = service::<Update>();
    let app = tauri::test::mock_builder()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(service.clone())
        .invoke_handler(tauri::generate_handler![
            crate::commands::update::get_update_status,
            crate::commands::update::install_update
        ])
        .build(context)
        .unwrap();
    let read_status = |label: &str| {
        let window = app.get_webview_window(label).unwrap_or_else(|| {
            tauri::WebviewWindowBuilder::new(&app, label, Default::default())
                .build()
                .unwrap()
        });
        tauri::test::get_ipc_response(
            &window,
            tauri::webview::InvokeRequest {
                cmd: "get_update_status".into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: if cfg!(windows) {
                    "http://tauri.localhost"
                } else {
                    "tauri://localhost"
                }
                .parse()
                .unwrap(),
                body: Default::default(),
                headers: Default::default(),
                invoke_key: tauri::test::INVOKE_KEY.to_string(),
            },
        )
        .map(|body| body.deserialize::<serde_json::Value>().unwrap())
    };
    println!(
        "e2e: launch, dashboard reads {}",
        read_status("dashboard").unwrap()
    );
    assert!(read_status("overlay").is_err(), "overlay denied");
    println!("e2e: overlay get_update_status denied by the capability layer");

    let source = NeverInstalls {
        inner: PluginSource::new(app.handle().clone()),
        installs: AtomicUsize::new(0),
    };
    check_once(&service, &source);
    println!(
        "e2e: check, dashboard reads {}",
        read_status("dashboard").unwrap()
    );
    assert_eq!(service.tray_offer(), TrayOffer::Enabled);
    println!("e2e: tray offers Update Echo");

    let (backend, _, release_open, _) = FakeBackend::blocked();
    let capture = spawn_coordinator(backend, AudioCapturePolicy::default());
    reserve(&capture, "dictating").unwrap();
    println!("e2e: a dictation holds the microphone reservation");
    let gate = IdleGate {
        capture: capture.clone(),
        runtime: EchoRuntime::default(),
        deliveries: quiet_deliveries,
    };
    let (version, pending) = service.admit_install().expect("install admitted");
    assert!(service.admit_install().is_none(), "second request ignored");
    let worker = {
        let service = service.clone();
        std::thread::spawn(move || {
            run_install(
                &service,
                &source,
                &gate,
                Duration::from_millis(5),
                version,
                pending,
            );
            source.installs.load(Ordering::SeqCst)
        })
    };
    while service.status().status != version_state("waiting_for_idle") {
        std::thread::sleep(Duration::from_millis(5));
    }
    println!(
        "e2e: downloaded and verified, dashboard reads {}",
        read_status("dashboard").unwrap()
    );
    cancel(&capture, "dictating");
    println!("e2e: the dictation ends");
    assert_eq!(worker.join().unwrap(), 1, "one install");
    println!(
        "e2e: stand-in installer ran once and failed, dashboard reads {}",
        read_status("dashboard").unwrap()
    );
    release_open.send(()).unwrap();
    crate::audio::tests::fixtures::begin(&capture, owner("overlay"), "after-update");
    println!("e2e: capture starts again after the failed install");
    for envelope in published.lock().unwrap().iter() {
        println!("e2e: event {}", serde_json::to_string(envelope).unwrap());
    }
    assert_sequences_rise_by_one(&published);
    let statuses = statuses(&published);
    assert_eq!(statuses.first(), Some(&available("0.5.1")));
    assert!(statuses.contains(&downloading(
        ARTIFACT.len() as u64,
        Some(ARTIFACT.len() as u64)
    )));
    assert_eq!(
        &statuses[statuses.len() - 3..],
        [
            version_state("waiting_for_idle"),
            version_state("installing"),
            version_state("failed")
        ]
    );
    assert!(
        server
            .requests()
            .iter()
            .filter(|p| *p == "/artifact")
            .count()
            == 1
    );
}

/// Runs the named test of this module again in a child process and returns false; in that
/// child it returns true, and the test may install the process logger. The log facade
/// accepts one logger per process.
fn in_log_child(marker: &str, test: &str) -> bool {
    if std::env::var_os(marker).is_some() {
        return true;
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    let status = crate::test_support::clear_child_environment(&mut command)
        .env(marker, "1")
        .args([
            "--exact",
            &format!("services::updater::tests::{test}"),
            "--nocapture",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "{test} failed in its child process");
    false
}

/// Records the `update_` log lines Alex reviews.
struct UpdateLog(Mutex<Vec<String>>);

impl log::Log for UpdateLog {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        let line = record.args().to_string();
        if line.starts_with("update_") {
            self.0.lock().unwrap().push(line);
        }
    }
    fn flush(&self) {}
}

static UPDATE_LOG: UpdateLog = UpdateLog(Mutex::new(Vec::new()));

fn record_update_log() -> &'static UpdateLog {
    log::set_logger(&UPDATE_LOG).unwrap();
    log::set_max_level(log::LevelFilter::Info);
    &UPDATE_LOG
}

fn logged(log: &UpdateLog) -> Vec<String> {
    log.0.lock().unwrap().clone()
}

/// A mock app configured as an installed 0.5.0 notify build: the updater configuration is
/// present and the updater plugin is not registered.
fn manifest_app(endpoints: &[String], insecure: bool) -> tauri::App<MockRuntime> {
    let mut context = tauri::test::mock_context(tauri::test::noop_assets());
    context.package_info_mut().version = "0.5.0".parse().unwrap();
    let mut updater = serde_json::json!({"endpoints": endpoints});
    if insecure {
        updater["dangerousInsecureTransportProtocol"] = true.into();
    }
    context
        .config_mut()
        .plugins
        .0
        .insert("updater".into(), updater);
    tauri::test::mock_builder().build(context).unwrap()
}

fn manifest_check(endpoints: &[String], insecure: bool) -> CheckOutcome<()> {
    let app = manifest_app(endpoints, insecure);
    ManifestSource::new(app.handle().clone()).check()
}

/// The manifest source for `failing`, with the short check timeout.
fn failing_manifest_source(
    failing: &FailingCheck,
) -> (tauri::App<MockRuntime>, ManifestSource<MockRuntime>) {
    let app = manifest_app(std::slice::from_ref(&failing.endpoint), failing.insecure);
    let source = ManifestSource {
        check_timeout: SHORT_CHECK_TIMEOUT,
        ..ManifestSource::new(app.handle().clone())
    };
    (app, source)
}

/// A manifest of exactly `length` bytes offering 0.5.1, padded with release notes.
fn padded_manifest(length: usize) -> Vec<u8> {
    let empty = serde_json::json!({"version": "0.5.1", "notes": ""}).to_string();
    let notes = "n".repeat(length - empty.len());
    let body = serde_json::json!({"version": "0.5.1", "notes": notes})
        .to_string()
        .into_bytes();
    assert_eq!(body.len(), length);
    body
}

/// Failures only the manifest source reads: a version that is not major.minor.patch, a
/// manifest without a version and a body over 64 KiB.
fn manifest_failures(release: &Loopback) -> Vec<FailingCheck> {
    ["/bad-version", "/no-version", "/oversized"]
        .into_iter()
        .map(|path| FailingCheck {
            case: path,
            endpoint: release.url(path),
            insecure: true,
        })
        .collect()
}

/// Serves the manifest-only failures and a body of exactly 64 KiB at `/limit`.
fn manifest_release_server() -> Loopback {
    Loopback::start(|_, path| match path {
        "/bad-version" => Reply::body(r#"{"version": "0.5"}"#),
        "/no-version" => Reply::body(r#"{"name": "Echo"}"#),
        "/oversized" => Reply::body(padded_manifest(MANIFEST_LIMIT + 1)),
        "/limit" => Reply::body(padded_manifest(MANIFEST_LIMIT)),
        _ => Reply::status(404),
    })
}

// Keep: the notify check reads no release from 404s on every endpoint, in configured order,
// falls back to the second endpoint, and stops at a 204 as the plugin does.
#[test]
fn manifest_missing_manifests_and_no_content_read_as_no_update() {
    let server = Loopback::start(|base, path| match path {
        "/fallback/latest.json" => Reply::body(manifest("0.5.1", &format!("{base}/artifact"))),
        "/empty/latest.json" => Reply::status(204),
        _ => Reply::status(404),
    });
    let endpoints = [
        server.url("/first/latest.json"),
        server.url("/second/latest.json"),
    ];
    assert!(matches!(
        manifest_check(&endpoints, true),
        CheckOutcome::None
    ));
    assert_eq!(
        server.requests(),
        ["/first/latest.json", "/second/latest.json"]
    );

    let fallback = [
        server.url("/first/latest.json"),
        server.url("/fallback/latest.json"),
    ];
    assert_eq!(
        offered_version(&manifest_check(&fallback, true)),
        Some("0.5.1")
    );

    let before = server.requests().len();
    let no_content = [
        server.url("/empty/latest.json"),
        server.url("/fallback/latest.json"),
    ];
    assert!(matches!(
        manifest_check(&no_content, true),
        CheckOutcome::None
    ));
    assert_eq!(&server.requests()[before..], ["/empty/latest.json"]);
}

// Keep: the notify comparator offers only higher stable versions, without a leading `v`,
// never a downgrade, the same version or a prerelease.
#[test]
fn manifest_offers_only_a_higher_stable_release() {
    let server = Loopback::start(|base, path| {
        let version = path
            .trim_start_matches('/')
            .trim_end_matches("/latest.json");
        Reply::body(manifest(version, &format!("{base}/artifact")))
    });
    for (version, offered) in [
        ("0.5.0", None),
        ("0.4.9", None),
        ("v0.5.0", None),
        ("0.5.1-beta.1", None),
        ("0.5.1", Some("0.5.1")),
        ("v0.5.1", Some("0.5.1")),
        ("0.5.1+build.7", Some("0.5.1")),
        ("1.0.0", Some("1.0.0")),
    ] {
        let endpoint = [server.url(&format!("/{version}/latest.json"))];
        assert_eq!(
            offered_version(&manifest_check(&endpoint, true)),
            offered,
            "{version}"
        );
    }
}

// Keep: the plugin's failures, plus a malformed version, a manifest without a version and a
// body over 64 KiB, each fail the notify check and leave the envelope unchanged: none stays
// none and an offer keeps its version. Five redirects and a 64 KiB body still read the offer.
#[test]
fn manifest_check_failures_keep_the_status_and_redirects_stop_after_five() {
    let release = release_server();
    assert_eq!(
        offered_version(&manifest_check(&[release.url("/redirect/5")], true)),
        Some("0.5.1")
    );
    let extra = manifest_release_server();
    assert_eq!(
        offered_version(&manifest_check(&[extra.url("/limit")], true)),
        Some("0.5.1")
    );
    let offering = manifest_app(&[release.url("/redirect/0")], true);
    let (mut cases, tls_hellos) = failing_checks(&release);
    cases.extend(manifest_failures(&extra));
    for failing in &cases {
        for offered in [false, true] {
            let (service, _) = service::<()>();
            if offered {
                check_once(&service, &ManifestSource::new(offering.handle().clone()));
                assert_eq!(service.status().status, offer("0.5.1".into()));
            }
            let before = service.status();
            let requests_before = release.requests().len();

            let (_app, source) = failing_manifest_source(failing);
            check_once(&service, &source);

            let case = failing.case;
            assert_eq!(service.status(), before, "{case}, offered: {offered}");
            if case == "http under the shipped config" {
                assert_eq!(
                    release.requests().len(),
                    requests_before,
                    "{case} sent a request"
                );
            }
        }
    }
    assert_eq!(
        tls_hellos.load(Ordering::SeqCst),
        2,
        "TLS handshakes attempted"
    );
}

// Keep: a shipped notify check gives up after 30 s; the failure cases above shorten it.
#[test]
fn manifest_shipped_checks_time_out_after_30_seconds() {
    let app = manifest_app(&[], false);
    assert_eq!(
        ManifestSource::new(app.handle().clone()).check_timeout,
        Duration::from_secs(30)
    );
}

// Keep: the shipped endpoints carry no URL template variables, so the notify check requests
// them as written, without the plugin's {{target}} substitution.
#[test]
fn shipped_endpoints_need_no_template_substitution() {
    let config: serde_json::Value =
        serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
    let endpoints = config["plugins"]["updater"]["endpoints"]
        .as_array()
        .unwrap();
    assert_eq!(endpoints.len(), 2);
    for endpoint in endpoints {
        let endpoint = endpoint.as_str().unwrap();
        assert!(endpoint.starts_with("https://"), "{endpoint}");
        assert!(!endpoint.contains("{{"), "{endpoint}");
    }
}

// Keep: the notify check's log lines for none, an offer and every failure kind.
#[test]
fn manifest_checks_log_none_available_and_failed() {
    if !in_log_child(
        "ECHO_TEST_MANIFEST_CHECK_LOG_CHILD",
        "manifest_checks_log_none_available_and_failed",
    ) {
        return;
    }
    let log = record_update_log();
    let release = release_server();
    let extra = manifest_release_server();
    for path in ["/latest.json", "/redirect/0"] {
        let app = manifest_app(&[release.url(path)], true);
        check_once(
            &service::<()>().0,
            &ManifestSource::new(app.handle().clone()),
        );
    }
    let (mut cases, _) = failing_checks(&release);
    cases.extend(manifest_failures(&extra));
    for failing in &cases {
        let (_app, source) = failing_manifest_source(failing);
        check_once(&service::<()>().0, &source);
    }
    let records = logged(log);
    assert_eq!(records.len(), 2 + cases.len(), "{records:?}");
    assert_eq!(records[0], "update_check outcome=none");
    assert_eq!(records[1], "update_check outcome=available version=0.5.1");
    for (failing, record) in cases.iter().zip(&records[2..]) {
        assert!(
            record.starts_with("update_check outcome=failed error="),
            "{}: {record}",
            failing.case
        );
    }
}

// Keep: the comparator rules without semver: one leading v, build metadata ignored, any
// prerelease not offered, and anything but three plain numbers an error.
#[test]
fn newer_stable_compares_three_numbers_and_skips_prereleases() {
    let current = (0, 5, 0);
    let long_build = format!("0.5.1+{}", "a".repeat(200));
    let offered = |version: &str| Ok((version.to_owned(), true));
    let not_offered = |version: &str| Ok((version.to_owned(), false));
    for (release, expected) in [
        ("0.5.1", offered("0.5.1")),
        ("0.6.0", offered("0.6.0")),
        ("1.0.0", offered("1.0.0")),
        ("0.5.10", offered("0.5.10")),
        ("v0.5.1", offered("0.5.1")),
        ("0.5.1+build.7", offered("0.5.1")),
        ("0.5.1+build-7.x-y", offered("0.5.1")),
        ("0.5.0", not_offered("0.5.0")),
        ("v0.5.0", not_offered("0.5.0")),
        ("0.5.0+build.7", not_offered("0.5.0")),
        ("0.4.99", not_offered("0.4.99")),
        ("0.5.1-beta.1", not_offered("0.5.1")),
        ("0.5.1-beta-2", not_offered("0.5.1")),
        ("1.0.0-rc.1+build.7", not_offered("1.0.0")),
        ("0.5", Err(())),
        ("0.5.1.2", Err(())),
        ("vv0.5.1", Err(())),
        ("0.05.1", Err(())),
        ("0.5.-1", Err(())),
        ("0.5.1-", Err(())),
        ("0.5.1+", Err(())),
        ("0.5.1+a..b", Err(())),
        ("0.5.1-a..b", Err(())),
        ("0.5.1-.a", Err(())),
        ("0.5.1+a.", Err(())),
        (
            "0.5.1+ URGENT: get the fixed build at evil.example instead",
            Err(()),
        ),
        (
            "0.5.1+build\nupdate_check outcome=available version=9.9.9",
            Err(()),
        ),
        ("0.5.1-<script>", Err(())),
        ("0.5.1+b\u{e9}ta", Err(())),
        ("0.5.\u{0661}", Err(())),
        (long_build.as_str(), Err(())),
        ("0.5.x", Err(())),
        (" 0.5.1", Err(())),
        ("", Err(())),
        ("99999999999999999999.0.0", Err(())),
    ] {
        assert_eq!(
            newer_stable(current, release).map_err(|_| ()),
            expected,
            "{release:?}"
        );
    }
}

/// Manifest versions a hostile server might send to put its own text in the button or the
/// log. Each must fail the check.
fn hostile_versions() -> Vec<String> {
    vec![
        "0.5.1+ URGENT: get the fixed build at evil.example instead".into(),
        "0.5.1+build\nupdate_check outcome=available version=9.9.9".into(),
        "0.5.1-<img src=evil.example>".into(),
        "0.5.1+\u{e9}vil".into(),
        "0.5.1+".into(),
        "0.5.1-".into(),
        "0.5.1+a..b".into(),
        format!("0.5.1+{}", "evil".repeat(16 * 1024)),
    ]
}

// Keep: hostile manifest versions fail the notify check against a loopback server and leave
// the status untouched, offered or not; build metadata is dropped from the offer; and no
// update_check log line carries server text beyond the canonical version.
#[test]
fn hostile_manifest_versions_never_reach_the_status_or_the_log() {
    if !in_log_child(
        "ECHO_TEST_HOSTILE_VERSION_LOG_CHILD",
        "hostile_manifest_versions_never_reach_the_status_or_the_log",
    ) {
        return;
    }
    let log = record_update_log();
    let hostile = hostile_versions();
    let served = hostile.clone();
    let server = Loopback::start(move |_, path| {
        let version = match path.strip_prefix("/hostile/") {
            Some(index) => served[index.parse::<usize>().unwrap()].clone(),
            None => "0.5.1+build.7".to_owned(),
        };
        Reply::body(serde_json::json!({"version": version}).to_string())
    });

    let (service, published) = service::<()>();
    let offering = manifest_app(&[server.url("/build")], true);
    let mut checks = 0;
    for offered in [false, true] {
        if offered {
            check_once(&service, &ManifestSource::new(offering.handle().clone()));
            checks += 1;
            assert_eq!(
                serde_json::to_value(service.status().status).unwrap()["version"],
                "0.5.1"
            );
        }
        for (index, version) in hostile.iter().enumerate() {
            let before = service.status();
            let app = manifest_app(&[server.url(&format!("/hostile/{index}"))], true);
            check_once(&service, &ManifestSource::new(app.handle().clone()));
            checks += 1;
            assert_eq!(service.status(), before, "{version:.60}");
        }
    }
    assert_eq!(statuses(&published), [offer("0.5.1".into())]);

    let records = logged(log);
    assert_eq!(records.len(), checks, "{records:?}");
    assert_eq!(
        records
            .iter()
            .filter(|line| *line == "update_check outcome=available version=0.5.1")
            .count(),
        1
    );
    for record in &records {
        assert!(record.len() < 200, "{record:.200}");
        assert!(
            record
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' '),
            "{record:?}"
        );
        for server_text in ["URGENT", "evil", "<img", "build", "9.9.9", "..b"] {
            assert!(!record.contains(server_text), "{record}");
        }
        if record != "update_check outcome=available version=0.5.1" {
            assert!(
                record.starts_with("update_check outcome=failed error="),
                "{record}"
            );
        }
    }
}

/// Puts the service in the notify offer, as a check does on notify builds, so every build
/// can exercise the download page.
fn offer_download_page<P: Clone>(service: &UpdaterService<P>, version: &str) {
    let changed = service.lock().change(UpdateStatus::DownloadPage {
        version: version.to_owned(),
    });
    service.publish(changed);
}

// Keep: a check offers available where Echo installs and download_page elsewhere; both notify
// states show Download Echo Update in the tray, with the dashboard's JSON shape.
#[test]
fn offer_maps_to_the_platform_state() {
    let expected = if cfg!(echo_update_install) {
        "available"
    } else {
        "download_page"
    };
    assert_eq!(
        serde_json::to_value(offer("0.5.1".into())).unwrap(),
        serde_json::json!({"state": expected, "version": "0.5.1"})
    );
    let (checked, _) = service::<&'static str>();
    let source = FakeSource::default();
    source.queue_check(CheckOutcome::Available {
        version: "0.5.1".into(),
        pending: "release",
    });
    check_once(&checked, &source);
    assert_eq!(checked.status().status, offer("0.5.1".into()));

    let (service, _) = service::<()>();

    for (status, state) in [
        (
            UpdateStatus::DownloadPage {
                version: "0.5.1".into(),
            },
            "download_page",
        ),
        (
            UpdateStatus::DownloadPageFailed {
                version: "0.5.1".into(),
            },
            "download_page_failed",
        ),
    ] {
        service.lock().change(status.clone());
        assert_eq!(service.tray_offer(), TrayOffer::DownloadPage, "{state}");
        assert_eq!(
            serde_json::to_value(UpdateEnvelope {
                sequence: 3,
                status
            })
            .unwrap(),
            serde_json::json!({"sequence": 3, "status": {"state": state, "version": "0.5.1"}})
        );
    }
}

// Keep: the download page opens exactly the fixed release URL and nothing else, only from an
// offer; a failed open reads download_page_failed and logs, a retry recovers, and a check
// that lands while the browser opens wins.
#[test]
fn download_page_opens_only_the_fixed_url() {
    if !in_log_child(
        "ECHO_TEST_DOWNLOAD_PAGE_LOG_CHILD",
        "download_page_opens_only_the_fixed_url",
    ) {
        return;
    }
    let log = record_update_log();
    let (service, published) = service::<()>();
    let opened: Mutex<Vec<String>> = Mutex::default();
    let opener = |result: Result<(), String>| {
        let opened = &opened;
        move |url: &str| {
            opened.lock().unwrap().push(url.to_owned());
            result
        }
    };

    let none = open_download_page(&service, opener(Ok(())));
    assert_eq!(none.status, UpdateStatus::None);
    assert!(opened.lock().unwrap().is_empty(), "opened from none");

    offer_download_page(&service, "0.5.1");
    let page = UpdateStatus::DownloadPage {
        version: "0.5.1".into(),
    };
    let failed = UpdateStatus::DownloadPageFailed {
        version: "0.5.1".into(),
    };
    assert_eq!(open_download_page(&service, opener(Ok(()))).status, page);
    assert_eq!(
        open_download_page(&service, opener(Err("no browser".into()))).status,
        failed
    );
    assert_eq!(service.tray_offer(), TrayOffer::DownloadPage);
    assert_eq!(open_download_page(&service, opener(Ok(()))).status, page);
    assert_eq!(
        *opened.lock().unwrap(),
        ["https://github.com/alegsm/Echo/releases/latest"; 3]
    );
    assert_eq!(
        DOWNLOAD_PAGE_URL,
        "https://github.com/alegsm/Echo/releases/latest"
    );
    assert_eq!(statuses(&published), [page.clone(), failed, page]);
    assert_sequences_rise_by_one(&published);

    // A check that finds no release while the browser opens is not overwritten.
    let cleared = open_download_page(&service, |_| {
        service.apply_check(CheckOutcome::None);
        Err("no browser".into())
    });
    assert_eq!(cleared.status, UpdateStatus::None);

    // From a state only builds that install use, nothing opens.
    let changed = service.lock().change(available("0.5.1"));
    service.publish(changed);
    let before = opened.lock().unwrap().len();
    assert_eq!(
        open_download_page(&service, opener(Ok(()))).status,
        available("0.5.1")
    );
    assert_eq!(
        opened.lock().unwrap().len(),
        before,
        "opened from available"
    );
    assert_sequences_rise_by_one(&published);

    assert_eq!(
        logged(log),
        [
            "update_open outcome=opened version=0.5.1",
            "update_open outcome=failed error=no browser",
            "update_open outcome=opened version=0.5.1",
            "update_open outcome=failed error=no browser",
        ]
    );
}

// Keep: a notify build that installs nothing keeps every install path out of reach: the
// install module and the plugin registration compile only under the build.rs switch, which
// names only macOS, and the shared update sources never name the plugin.
#[cfg(not(echo_update_install))]
#[test]
fn no_install_path_is_reachable_on_notify_builds() {
    fn preceded_by_switch(source: &str, line: &str) {
        let lines: Vec<&str> = source.lines().map(str::trim).collect();
        let at: Vec<usize> = (0..lines.len()).filter(|&i| lines[i] == line).collect();
        assert_eq!(at.len(), 1, "{line}");
        assert!(
            at[0] > 0 && lines[at[0] - 1] == "#[cfg(echo_update_install)]",
            "{line}"
        );
    }
    let updater = include_str!("updater.rs");
    let lib = include_str!("../lib.rs");
    preceded_by_switch(updater, "mod install;");
    preceded_by_switch(
        lib,
        "let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());",
    );
    assert_eq!(lib.matches("tauri_plugin_updater").count(), 1);
    // Only the install module, this test file and the cfg-gated line in lib.rs may name the
    // plugin or its reqwest.
    fn sources(directory: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, found);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    sources(&root, &mut found);
    assert!(found.len() > 20, "{found:?}");
    let mut naming = Vec::new();
    for path in &found {
        let source = std::fs::read_to_string(path).unwrap();
        if source.contains("tauri_plugin_updater") || source.contains("updater_reqwest") {
            let relative = path.strip_prefix(&root).unwrap().to_string_lossy();
            naming.push(relative.replace('\\', "/"));
        }
    }
    naming.sort();
    assert_eq!(
        naming,
        [
            "lib.rs",
            "services/updater/install.rs",
            "services/updater_tests.rs"
        ]
    );
    assert!(!lib.contains("updater_reqwest"));
    let build = include_str!("../../build.rs");
    let switch = build
        .split("fn installs_updates() -> bool {")
        .nth(1)
        .unwrap()
        .split("\n}")
        .next()
        .unwrap();
    assert!(switch.contains(r#"target_os == "macos""#), "{switch}");
    for other in ["windows", "linux", "||"] {
        assert!(!switch.contains(other), "{switch}");
    }
    assert!(build.contains(r#"println!("cargo:rustc-cfg=echo_update_install");"#));
}

// Keep: a debug notify build, as `bun run tauri:dev` runs, never contacts an endpoint.
#[cfg(not(echo_update_install))]
#[test]
fn notify_debug_builds_never_contact_the_endpoints() {
    let server = Loopback::start(|_, _| Reply::status(404));
    let app = manifest_app(&[server.url("/latest.json")], true);
    app.manage(UpdaterService::<ProductionPending>::new(|_| {}));

    start(app.handle());
    std::thread::sleep(Duration::from_millis(500));

    assert!(server.requests().is_empty());
    assert_eq!(
        app.state::<UpdaterService>().status().status,
        UpdateStatus::None
    );
}

// Keep: the notify twin of the install-path check test: none and failed checks show
// nothing, a release offers download_page, and a failed check keeps that offer.
#[cfg(not(echo_update_install))]
#[test]
fn notify_checks_offer_the_download_page_and_failures_keep_the_status() {
    let (service, published) = service();
    let source = FakeSource::default();
    for outcome in [CheckOutcome::None, CheckOutcome::Failed("timeout".into())] {
        source.queue_check(outcome);
        check_once(&service, &source);
        assert_eq!(service.status().status, UpdateStatus::None);
        assert_eq!(service.tray_offer(), TrayOffer::Hidden);
    }
    assert!(published.lock().unwrap().is_empty(), "nothing to show");

    source.queue_check(CheckOutcome::Available {
        version: "0.5.1".into(),
        pending: "release",
    });
    check_once(&service, &source);
    let page = UpdateStatus::DownloadPage {
        version: "0.5.1".into(),
    };
    assert_eq!(service.status().status, page);
    assert_eq!(service.tray_offer(), TrayOffer::DownloadPage);

    source.queue_check(CheckOutcome::Failed("malformed".into()));
    check_once(&service, &source);
    assert_eq!(
        service.status(),
        UpdateEnvelope {
            sequence: 1,
            status: page.clone()
        }
    );

    source.queue_check(CheckOutcome::None);
    check_once(&service, &source);
    assert_eq!(statuses(&published), [page, UpdateStatus::None]);
    assert_sequences_rise_by_one(&published);
}

// Keep: the notify half of the loop across layers. A dashboard reads none over real IPC, a
// check against a loopback release offers download_page 0.5.1 and the tray offers Download
// Echo Update; a failed browser open reads download_page_failed and a retry recovers. The
// server sees only the manifest request: the artifact it also serves is never fetched.
#[cfg(not(echo_update_install))]
#[test]
fn notify_loop_walks_from_offer_to_the_download_page_over_loopback() {
    let server = Loopback::start(|base, path| match path {
        "/latest.json" => Reply::body(manifest("0.5.1", &format!("{base}/artifact"))),
        "/artifact" => Reply::body(ARTIFACT),
        _ => Reply::status(404),
    });
    let mut context = tauri::generate_context!(test = true);
    context.package_info_mut().version = "0.5.0".parse().unwrap();
    context.config_mut().plugins.0.insert(
        "updater".into(),
        serde_json::json!({
            "endpoints": [server.url("/latest.json")],
            "dangerousInsecureTransportProtocol": true
        }),
    );
    let (service, published) = service::<()>();
    let app = tauri::test::mock_builder()
        .manage(service.clone())
        .invoke_handler(tauri::generate_handler![
            crate::commands::update::get_update_status,
            crate::commands::update::install_update
        ])
        .build(context)
        .unwrap();
    let read_status = |label: &str| {
        let window = app.get_webview_window(label).unwrap_or_else(|| {
            tauri::WebviewWindowBuilder::new(&app, label, Default::default())
                .build()
                .unwrap()
        });
        tauri::test::get_ipc_response(
            &window,
            tauri::webview::InvokeRequest {
                cmd: "get_update_status".into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: if cfg!(windows) {
                    "http://tauri.localhost"
                } else {
                    "tauri://localhost"
                }
                .parse()
                .unwrap(),
                body: Default::default(),
                headers: Default::default(),
                invoke_key: tauri::test::INVOKE_KEY.to_string(),
            },
        )
        .map(|body| body.deserialize::<serde_json::Value>().unwrap())
    };
    let state = |label: &str| read_status(label).unwrap()["status"].clone();
    assert_eq!(state("dashboard"), serde_json::json!({"state": "none"}));
    assert!(read_status("overlay").is_err(), "overlay denied");

    check_once(&service, &ManifestSource::new(app.handle().clone()));
    assert_eq!(
        state("dashboard"),
        serde_json::json!({"state": "download_page", "version": "0.5.1"})
    );
    assert_eq!(service.tray_offer(), TrayOffer::DownloadPage);

    let mut opened = Vec::new();
    open_download_page(&service, |url| {
        opened.push(url.to_owned());
        Err("no browser".into())
    });
    assert_eq!(
        state("dashboard"),
        serde_json::json!({"state": "download_page_failed", "version": "0.5.1"})
    );
    open_download_page(&service, |url| {
        opened.push(url.to_owned());
        Ok(())
    });
    assert_eq!(
        state("dashboard"),
        serde_json::json!({"state": "download_page", "version": "0.5.1"})
    );
    assert_eq!(opened, [DOWNLOAD_PAGE_URL; 2]);
    assert_sequences_rise_by_one(&published);
    assert_eq!(server.requests(), ["/latest.json"]);
}
