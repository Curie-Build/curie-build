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
