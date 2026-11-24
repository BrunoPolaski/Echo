//! Records every panic in Echo.log. On Windows a synchronous command runs inside a
//! WebView2 callback that a panic cannot unwind through, so Echo aborts and this record
//! is the only trace. It holds the thread, the source location and a message that is a
//! literal in the source. A formatted message is left out: it can quote the data that
//! panicked, such as transcript text.

use std::sync::mpsc;
use std::time::Duration;

const WRITER_THREAD: &str = "echo-panic-log";
const WRITE_WAIT: Duration = Duration::from_millis(500);

/// Puts the record in front of the hook already installed, which still runs afterwards.
pub fn install() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        // A panic on the writer thread came from logging itself, so it is not logged.
        if thread.name() != Some(WRITER_THREAD) {
            // Rust carries a message with no arguments as a `&'static str` payload.
            let literal = info.payload().downcast_ref::<&'static str>().copied();
            let record = record(thread.name(), info.location(), literal);
            write_within(WRITE_WAIT, move || log::error!("{record}"));
        }
        previous(info);
    }));
}

fn record(
    thread: Option<&str>,
    location: Option<&std::panic::Location<'_>>,
    literal: Option<&str>,
) -> String {
    let thread = thread.unwrap_or("unnamed");
    let location = location.map_or_else(|| "unknown".to_owned(), ToString::to_string);
    match literal {
        Some(message) => format!("panic thread={thread} location={location} message={message}"),
        None => format!("panic thread={thread} location={location}"),
    }
}

/// Runs `write` on its own thread and waits at most `wait` for it. The panicking thread
/// may hold the logger's lock, when the panic came from formatting a log record, and
/// logging from it would then hang Echo instead of letting the panic finish.
fn write_within(wait: Duration, write: impl FnOnce() + Send + 'static) {
    let (done, finished) = mpsc::channel();
    let writer = std::thread::Builder::new()
        .name(WRITER_THREAD.into())
        .spawn(move || {
            write();
            let _ = done.send(());
        });
    if writer.is_ok() {
        let _ = finished.recv_timeout(wait);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::clear_child_environment;

    // Keep: a panic leaves its thread and source location in Echo.log before the process
    // ends. A literal message is recorded; a formatted one, which can quote transcript
    // text, never is.
    #[test]
    fn a_panic_is_recorded_in_the_log_without_formatted_text() {
        const NAME: &str =
            "panic_log::tests::a_panic_is_recorded_in_the_log_without_formatted_text";
        const CHILD: &str = "ECHO_TEST_PANIC_LOG_CHILD";
        const KIND: &str = "ECHO_TEST_PANIC_LOG_KIND";
        const SPOKEN: &str = "dictated words that must stay out of the log";
        if let Some(directory) = std::env::var_os(CHILD) {
            crate::bounded_log::dispatch(directory.into())
                .unwrap()
                .apply()
                .unwrap();
            install();
            if std::env::var(KIND).unwrap() == "literal" {
                panic!("a literal message");
            }
            let spoken = SPOKEN.to_owned();
            panic!("slice failed inside `{spoken}`");
        }
        for (kind, expected_message) in
            [("formatted", None), ("literal", Some("a literal message"))]
        {
            let directory =
                std::env::temp_dir().join(format!("echo-panic-log-{}", uuid::Uuid::new_v4()));
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            clear_child_environment(&mut child)
                .env(CHILD, &directory)
                .env(KIND, kind)
                .args(["--exact", NAME])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            assert!(!child.status().unwrap().success(), "the child must panic");
            let log = std::fs::read_to_string(directory.join("Echo.log")).unwrap();
            std::fs::remove_dir_all(&directory).unwrap();
            let records: Vec<&str> = log.lines().collect();
            assert_eq!(records.len(), 1, "{log}");
            let (location, message) = match records[0].split_once(" message=") {
                Some((location, message)) => (location, Some(message)),
                None => (records[0], None),
            };
            assert!(
                location.starts_with(&format!("panic thread={NAME} location=")),
                "{log}"
            );
            assert!(location.contains("panic_log.rs:"), "{log}");
            assert_eq!(message, expected_message, "{log}");
            assert!(!log.contains(SPOKEN), "{log}");
        }
    }

    // Keep: a logger locked by the panicking thread delays the panic by the wait, not forever.
    #[test]
    fn a_blocked_log_write_does_not_hold_the_panic() {
        let (release, blocked) = mpsc::channel::<()>();
        let started = std::time::Instant::now();
        write_within(Duration::from_millis(50), move || {
            let _ = blocked.recv();
        });
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(release);
    }
}
