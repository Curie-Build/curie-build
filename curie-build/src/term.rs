/// Returns `true` when stdout is a terminal and `NO_COLOR` is not set.
pub(crate) fn use_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    #[cfg(unix)]
    {
        extern "C" {
            fn isatty(fd: i32) -> i32;
        }
        unsafe { isatty(1) != 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// The width (in columns) of the controlling terminal on stdout.
///
/// Queried via the `TIOCGWINSZ` ioctl rather than the `COLUMNS` env var,
/// because `COLUMNS` is a shell-local variable that is usually *not* exported
/// to child processes — relying on it silently falls back to a default and
/// produces a too-wide value.  Returns `None` when stdout is not a terminal
/// (e.g. piped to a file) or the ioctl fails.
pub(crate) fn width() -> Option<u16> {
    #[cfg(unix)]
    {
        // struct winsize { ws_row, ws_col, ws_xpixel, ws_ypixel } — all u16.
        #[repr(C)]
        struct Winsize {
            ws_row: u16,
            ws_col: u16,
            ws_xpixel: u16,
            ws_ypixel: u16,
        }
        extern "C" {
            fn ioctl(fd: i32, request: u64, ...) -> i32;
        }
        // TIOCGWINSZ is 0x5413 on Linux.  On macOS/BSD it differs, so gate it.
        #[cfg(target_os = "linux")]
        const TIOCGWINSZ: u64 = 0x5413;
        #[cfg(not(target_os = "linux"))]
        const TIOCGWINSZ: u64 = 0x4008_7468;

        let mut ws = Winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // fd 1 = stdout.
        let rc = unsafe { ioctl(1, TIOCGWINSZ, &mut ws as *mut Winsize) };
        if rc == 0 && ws.ws_col > 0 {
            return Some(ws.ws_col);
        }
        None
    }
    #[cfg(not(unix))]
    {
        None
    }
}
