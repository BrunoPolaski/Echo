//! Opens a target, such as a settings URI or a web page, through the Windows shell.

/// ShellExecuteW returns a value above 32 on success; anything else, null included, is
/// its error code.
fn open_result(result: isize) -> Result<(), isize> {
    if result > 32 {
        Ok(())
    } else {
        Err(result)
    }
}

#[cfg(target_os = "windows")]
pub(super) use native::{shell_open, wide};

#[cfg(target_os = "windows")]
mod native {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    // A narrow declaration avoids adding Windows features to the manifest.
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            window: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> *mut c_void;
    }

    const SW_SHOWNORMAL: i32 = 1;

    /// Runs the shell's "open" verb on `target`. The error is ShellExecuteW's result.
    pub(crate) fn shell_open(target: &str) -> Result<(), isize> {
        let operation = wide("open");
        let target = wide(target);
        // SAFETY: both strings are NUL-terminated and live until ShellExecuteW returns;
        // optional window, parameters and directory are null. The result is a status, not an owned handle.
        let result = unsafe {
            ShellExecuteW(
                null_mut(),
                operation.as_ptr(),
                target.as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL,
            )
        };
        super::open_result(result as isize)
    }

    pub(crate) fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Keep: ShellExecute's values through 32 are failures, including a null result.
    #[test]
    fn shell_open_propagates_shell_execute_failure() {
        for result in [0, 2, 5, 31, 32] {
            assert_eq!(open_result(result), Err(result), "{result}");
        }
        for result in [33, 42, isize::MAX] {
            assert!(open_result(result).is_ok(), "{result}");
        }
    }
}
