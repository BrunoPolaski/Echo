//! Re-inserts a window at the top of the topmost z-order band without activating it.
//!
//! tao calls `SetWindowPos(HWND_TOPMOST)` only when its own always-on-top flag changes, so once
//! the window has been demoted, or a later topmost window has risen above it,
//! `set_always_on_top(true)` is a no-op. Only `SetWindowPos` moves a window between z-order
//! bands; `SetWindowLongW` neither sets nor clears `WS_EX_TOPMOST`.
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SetWindowPos, HWND_TOPMOST, SET_WINDOW_POS_FLAGS, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE,
};

/// Changes z-order only: no move, no resize, no activation, and no visibility change.
/// `SWP_ASYNCWINDOWPOS` matters because callers such as the async `start_audio_capture` command
/// run on a runtime worker, not the window's thread: Windows posts the request to the window's
/// thread instead of making the worker wait for it. On the owning thread the call is synchronous.
const TOPMOST_FLAGS: SET_WINDOW_POS_FLAGS =
    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;

/// Places `window` at the top of the topmost band. It never activates or shows the window.
pub fn raise_to_topmost(window: HWND) -> Result<(), String> {
    // SAFETY: SetWindowPos validates the handle and fails for an invalid one; HWND_TOPMOST is a
    // documented insert-after sentinel and the remaining arguments are plain values.
    let placed = unsafe { SetWindowPos(window, HWND_TOPMOST, 0, 0, 0, 0, TOPMOST_FLAGS) };
    if placed == 0 {
        return Err(format!(
            "SetWindowPos(HWND_TOPMOST) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetWindow, GetWindowLongW, GWL_EXSTYLE, GW_HWNDPREV,
        SWP_HIDEWINDOW, SWP_SHOWWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    /// Concurrent z-order changes from several test threads intermittently leave a just-raised
    /// window without `WS_EX_TOPMOST` (seen with and without `SWP_ASYNCWINDOWPOS`), so every
    /// window-creating test holds this lock for its whole body.
    static WINDOW_TESTS: Mutex<()> = Mutex::new(());

    fn serialize_window_tests() -> MutexGuard<'static, ()> {
        // A panicking test poisons the lock but leaves no shared state behind; its windows were
        // destroyed during unwinding.
        WINDOW_TESTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A hidden top-level window owned by the test thread, destroyed on drop.
    struct TestWindow(HWND);

    impl TestWindow {
        fn create() -> Self {
            let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            // SAFETY: STATIC is a system window class, the class name is NUL-terminated and
            // outlives the call, and every other argument is a value or null.
            let window = unsafe {
                CreateWindowExW(
                    0,
                    class.as_ptr(),
                    std::ptr::null(),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            assert!(!window.is_null(), "test window creation failed");
            Self(window)
        }

        fn ex_style(&self) -> u32 {
            // SAFETY: the handle is a live window owned by this thread.
            unsafe { GetWindowLongW(self.0, GWL_EXSTYLE) as u32 }
        }

        /// True when this window is above `other` in z-order. Other processes keep creating
        /// and destroying windows, so a walk that loses its place retries before failing.
        fn is_above(&self, other: &TestWindow) -> bool {
            (0..5).any(|_| {
                let mut above = other.0;
                loop {
                    // SAFETY: GetWindow accepts any handle and returns null at the top or when
                    // the window it started from has been destroyed.
                    above = unsafe { GetWindow(above, GW_HWNDPREV) };
                    if above.is_null() {
                        return false;
                    }
                    if above == self.0 {
                        return true;
                    }
                }
            })
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: the handle was created on this thread and is destroyed once.
            unsafe { DestroyWindow(self.0) };
        }
    }

    #[test]
    fn restacking_never_activates_moves_resizes_or_shows_the_window() {
        assert_ne!(TOPMOST_FLAGS & SWP_NOACTIVATE, 0);
        assert_ne!(TOPMOST_FLAGS & SWP_NOMOVE, 0);
        assert_ne!(TOPMOST_FLAGS & SWP_NOSIZE, 0);
        assert_eq!(TOPMOST_FLAGS & (SWP_SHOWWINDOW | SWP_HIDEWINDOW), 0);
    }

    #[test]
    fn returns_a_demoted_window_to_the_topmost_band() {
        let _serial = serialize_window_tests();
        let overlay = TestWindow::create();
        let ordinary = TestWindow::create();
        raise_to_topmost(overlay.0).expect("make the overlay topmost");
        assert_ne!(overlay.ex_style() & WS_EX_TOPMOST, 0);
        assert!(overlay.is_above(&ordinary));
        // Inserting after an ordinary window is one of the ways Windows demotes a topmost window.
        // SAFETY: both handles are live windows owned by this thread.
        let demoted = unsafe {
            SetWindowPos(
                overlay.0,
                ordinary.0,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        };
        assert_ne!(demoted, 0);
        // Leaving the band clears the bit, so tao's flag and the window now disagree.
        assert_eq!(overlay.ex_style() & WS_EX_TOPMOST, 0);
        assert!(ordinary.is_above(&overlay));

        raise_to_topmost(overlay.0).expect("restack the overlay");

        assert_ne!(overlay.ex_style() & WS_EX_TOPMOST, 0);
        assert_eq!(ordinary.ex_style() & WS_EX_TOPMOST, 0);
        assert!(overlay.is_above(&ordinary));
    }

    #[test]
    fn an_invalid_handle_is_a_recoverable_error() {
        let error = raise_to_topmost(std::ptr::null_mut()).expect_err("null handle");
        assert!(error.starts_with("SetWindowPos(HWND_TOPMOST) failed"));
    }
}
