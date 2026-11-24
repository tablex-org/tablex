//! The main window could not open (no usable graphics backend): say so where the user can see it, then exit
//! instead of panicking with nothing on screen.

use crate::error_log::{Area, LogStore, Report, Severity, StoredEntry};

/// The text shown to the user (stderr, the log and the native box).
pub(super) fn message(error: &dyn std::fmt::Display) -> String {
    t!("startup.window_failed", error = error.to_string()).into_owned()
}

/// Logs the failure, prints it, shows a native box where one is cheap, and exits with code 1.
pub(super) fn report_and_exit(error: &dyn std::fmt::Display) -> ! {
    let message = message(error);
    eprintln!("{message}");
    if let Some(dir) = crate::error_log::default_dir() {
        let report = Report::new(Severity::Error, Area::App, message.clone());
        let entry = StoredEntry {
            at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis() as i64),
            severity: report.severity,
            area: report.area,
            message: report.message,
            repeat: 1,
            ..StoredEntry::default()
        };
        // Best effort: a log that cannot be written must not hide the message.
        let _ = LogStore::new(dir).append(&entry.to_line());
    }
    show_native_box(&message);
    std::process::exit(1)
}

#[cfg(target_os = "windows")]
fn show_native_box(message: &str) {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(window: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    const MB_OK_ICONERROR: u32 = 0x10;
    let wide = |text: &str| -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let (text, caption) = (wide(message), wide("tablex"));
    // SAFETY: both buffers are NUL-terminated and outlive the call; no owner window.
    unsafe { MessageBoxW(0, text.as_ptr(), caption.as_ptr(), MB_OK_ICONERROR) };
}

/// zenity (GNOME) or kdialog (KDE) when installed; the message is on stderr either way.
#[cfg(target_os = "linux")]
fn show_native_box(message: &str) {
    use std::process::{Command, Stdio};
    let boxes: [(&str, &[&str]); 2] = [
        ("zenity", &["--error", "--no-markup", "--title", "tablex", "--text"]),
        ("kdialog", &["--error"]),
    ];
    for (program, args) in boxes {
        let shown = Command::new(program)
            .args(args)
            .arg(message)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        if shown {
            return;
        }
    }
}

// macOS always has Metal: stderr and the log are enough.
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn show_native_box(_message: &str) {}

#[cfg(test)]
mod tests {
    #[test]
    fn message_names_the_requirements_and_the_error() {
        let message = super::message(&"Failed to create surface");
        assert!(message.starts_with("tablex could not open its window"));
        assert!(message.contains("Vulkan or OpenGL on Linux"));
        assert!(message.ends_with("Details: Failed to create surface"));
    }
}
