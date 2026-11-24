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

    const CHILD: &str = "ECHO_TEST_PANIC_LOG_CHILD";
    const KIND: &str = "ECHO_TEST_PANIC_LOG_KIND";

    /// In a child process, installs the real log writer and the hook, and returns the
    /// kind of panic the parent asked for.
    fn child_kind() -> Option<String> {
        let directory = std::env::var_os(CHILD)?;
        crate::bounded_log::dispatch(directory.into())
            .unwrap()
            .apply()
            .unwrap();
        install();
        Some(std::env::var(KIND).unwrap())
    }

    /// Runs one test of this binary again as a child that logs under a new directory.
    fn child(test: &str, kind: &str) -> (std::process::Child, std::path::PathBuf) {
        let directory =
            std::env::temp_dir().join(format!("echo-panic-log-{}", uuid::Uuid::new_v4()));
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        clear_child_environment(&mut command)
            .env(CHILD, &directory)
            .env(KIND, kind)
            .args(["--exact", test])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        (command.spawn().unwrap(), directory)
    }

    // Keep: a panic leaves its thread and source location in Echo.log before the process
    // ends. A literal message is recorded; a formatted one, which can quote transcript
    // text, never is.
    #[test]
    fn a_panic_is_recorded_in_the_log_without_formatted_text() {
        const NAME: &str =
            "panic_log::tests::a_panic_is_recorded_in_the_log_without_formatted_text";
        const SPOKEN: &str = "dictated words that must stay out of the log";
        if let Some(kind) = child_kind() {
            if kind == "literal" {
                panic!("a literal message");
            }
            let spoken = SPOKEN.to_owned();
            panic!("slice failed inside `{spoken}`");
        }
        for (kind, expected_message) in
            [("formatted", None), ("literal", Some("a literal message"))]
        {
            let (mut child, directory) = child(NAME, kind);
            assert!(!child.wait().unwrap().success(), "the child must panic");
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

    // Keep: a panic raised while its own thread holds the logger's lock, here a value
    // that panics as the log writer formats it, still ends the process. Logging from the
    // panicking thread would wait on that lock forever.
    #[test]
    fn a_panic_inside_a_log_record_does_not_hang() {
        const NAME: &str = "panic_log::tests::a_panic_inside_a_log_record_does_not_hang";
        struct PanicsWhenLogged;
        impl std::fmt::Display for PanicsWhenLogged {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                panic!("formatting failed")
            }
        }
        if child_kind().is_some() {
            log::info!("{PanicsWhenLogged}");
            return;
        }
        let (mut child, directory) = child(NAME, "inside-log-record");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("the child hung on the logger's lock");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let _ = std::fs::remove_dir_all(&directory);
        assert!(!status.success(), "the child must panic");
    }
}
