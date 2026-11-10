//! Resolve the OS folder opener without searching the portable app directory.
#[cfg(windows)]
pub(crate) fn opener() -> Result<std::path::PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetWindowsDirectoryW(buffer: *mut u16, size: u32) -> u32;
    }
    let mut buffer = vec![0u16; 32768];
    // The OS writes at most the supplied UTF-16 capacity; zero/overflow fails.
    let length = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err("system_directory_unavailable".into());
    }
    let path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]))
        .join("explorer.exe");
    if !path.is_absolute() || !path.is_file() {
        return Err("system_opener_unavailable".into());
    }
    Ok(path)
}

#[cfg(target_os = "macos")]
pub(crate) fn opener() -> Result<std::path::PathBuf, String> {
    Ok("/usr/bin/open".into())
}
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn opener() -> Result<std::path::PathBuf, String> {
    Ok("/usr/bin/xdg-open".into())
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn opener_is_an_existing_absolute_system_executable() {
        let path = super::opener().unwrap();
        assert!(path.is_absolute() && path.is_file());
        assert_eq!(path.file_name().unwrap(), "explorer.exe");
        assert_ne!(path.parent(), std::env::current_dir().ok().as_deref());
    }
}
