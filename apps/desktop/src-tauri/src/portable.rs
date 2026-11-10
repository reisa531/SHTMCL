//! Windows portable data layout and the WebView2 preflight.
//!
//! The root follows the executable rather than the process working directory.
//! Tauri also uses this root for its WebView2 profile through
//! `appDirectoriesOverride` in `run()`.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const WEBVIEW2_DOWNLOAD: &str = "https://developer.microsoft.com/microsoft-edge/webview2/";
const RUNTIME_GUID: &str = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";

#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(
        hwnd: *mut core::ffi::c_void,
        text: *const u16,
        caption: *const u16,
        kind: u32,
    ) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteW(
        hwnd: *mut core::ffi::c_void,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show: i32,
    ) -> isize;
}

const MB_OK: u32 = 0x0000;
const MB_YESNO: u32 = 0x0004;
const MB_YESNOCANCEL: u32 = 0x0003;
const MB_ICONERROR: u32 = 0x0010;
const MB_ICONWARNING: u32 = 0x0030;
const MB_ICONINFORMATION: u32 = 0x0040;
const IDYES: i32 = 6;
const IDNO: i32 = 7;

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn message(text: &str, kind: u32) -> i32 {
    let text = wide(text);
    let title = wide("SHTMCL");
    // SAFETY: both strings are NUL-terminated and remain alive for the call.
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), kind) }
}

fn is_valid_version(output: &[u8]) -> bool {
    let output = String::from_utf8_lossy(output);
    output.lines().any(|line| {
        let Some((_, value)) = line.split_once("REG_SZ") else {
            return false;
        };
        let version = value.trim();
        version
            .split('.')
            .filter_map(|part| part.parse::<u32>().ok())
            .any(|part| part > 0)
    })
}

fn webview2_runtime_present() -> bool {
    let keys = [
        format!(r"HKCU\SOFTWARE\Microsoft\EdgeUpdate\Clients\{RUNTIME_GUID}"),
        format!(r"HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{RUNTIME_GUID}"),
        format!(r"HKLM\SOFTWARE\Microsoft\EdgeUpdate\Clients\{RUNTIME_GUID}"),
    ];
    keys.iter().any(|key| {
        Command::new("reg.exe")
            .args(["query", key, "/v", "pv"])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .output()
            .is_ok_and(|output| output.status.success() && is_valid_version(&output.stdout))
    })
}

fn portable_root(executable: &Path) -> io::Result<PathBuf> {
    let parent = executable
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "executable has no parent directory",
            )
        })?;
    Ok(parent.join("SHTMCL-data"))
}

fn has_files(path: &Path) -> io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    Ok(fs::read_dir(path)?.next().is_some())
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "legacy data contains a symbolic link: {}",
                    source_path.display()
                ),
            ));
        }
        if metadata.is_dir() {
            copy_tree(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(source_path, destination_path)?;
        }
    }
    Ok(())
}

fn migrate_legacy(source: &Path, destination: &Path) -> io::Result<()> {
    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "portable data root has no parent",
        )
    })?;
    let stage = parent.join(format!(".SHTMCL-data-migration-{}", std::process::id()));
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    if let Err(error) = copy_tree(source, &stage) {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    match fs::rename(&stage, destination) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_dir_all(&stage);
            Err(error)
        }
    }
}

fn writable_probe(root: &Path) -> io::Result<()> {
    fs::create_dir_all(root)?;
    let probe = root.join(format!(".write-probe-{}", std::process::id()));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    drop(file);
    fs::remove_file(probe)
}

/// Prepare the path before Tauri creates the WebView. Returns Err after showing
/// a native message when startup should stop.
pub(super) fn prepare() -> Result<PathBuf, ()> {
    if !webview2_runtime_present() {
        let answer = message(
            "SHTMCL 需要 Microsoft Edge WebView2 Runtime 才能显示界面。\n\n选择“是”打开微软官方下载页；下载并安装后，请重新双击 SHTMCL.exe。",
            MB_YESNO | MB_ICONWARNING,
        );
        if answer == IDYES {
            let operation = wide("open");
            let target = wide(WEBVIEW2_DOWNLOAD);
            // SAFETY: all arguments are NUL-terminated and valid for the call.
            let result = unsafe {
                ShellExecuteW(
                    std::ptr::null_mut(),
                    operation.as_ptr(),
                    target.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    1,
                )
            };
            if result <= 32 {
                message(
                    "无法打开浏览器。请手动访问微软 WebView2 Runtime 下载页：\nhttps://developer.microsoft.com/microsoft-edge/webview2/",
                    MB_OK | MB_ICONERROR,
                );
            }
        }
        return Err(());
    }

    let executable = std::env::current_exe().map_err(|error| {
        message(
            &format!("无法确定 SHTMCL.exe 的位置：{error}"),
            MB_OK | MB_ICONERROR,
        );
    })?;
    let root = portable_root(&executable).map_err(|error| {
        message(
            &format!("无法确定便携数据目录：{error}"),
            MB_OK | MB_ICONERROR,
        );
    })?;

    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let (portable_data_exists, check_legacy) = match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            message(
                &format!("便携数据目录不能是符号链接：\n{}", root.display()),
                MB_OK | MB_ICONERROR,
            );
            return Err(());
        }
        Ok(metadata) if metadata.is_dir() => match has_files(&root) {
            Ok(exists) => (exists, !exists),
            Err(error) => {
                message(
                    &format!("无法检查便携数据目录：\n{}\n\n{error}", root.display()),
                    MB_OK | MB_ICONERROR,
                );
                return Err(());
            }
        },
        Ok(_) => (false, false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (false, true),
        Err(error) => {
            message(
                &format!("无法检查便携数据目录：\n{}\n\n{error}", root.display()),
                MB_OK | MB_ICONERROR,
            );
            return Err(());
        }
    };
    if check_legacy && !portable_data_exists {
        if let Some(legacy) = appdata.map(|path| path.join("dev.shtmcl.launcher")) {
            let legacy_exists = match has_files(&legacy) {
                Ok(exists) => exists,
                Err(error) => {
                    message(
                        &format!("无法检查旧版数据目录：\n{}\n\n{error}", legacy.display()),
                        MB_OK | MB_ICONERROR,
                    );
                    return Err(());
                }
            };
            if legacy_exists {
                let prompt = format!(
                    "检测到旧版数据：\n{}\n\n选择“是”将数据复制到程序旁的 SHTMCL-data；旧数据会保留作为备份。选择“否”以空白便携数据启动。选择“取消”退出。",
                    legacy.display()
                );
                match message(&prompt, MB_YESNOCANCEL | MB_ICONINFORMATION) {
                    IDYES => {
                        if root.exists() {
                            if let Err(error) = fs::remove_dir(&root) {
                                message(
                                    &format!("便携数据目录无法准备，旧数据仍保留。\n\n{error}"),
                                    MB_OK | MB_ICONERROR,
                                );
                                return Err(());
                            }
                        }
                        if let Err(error) = migrate_legacy(&legacy, &root) {
                            message(
                                &format!("旧数据复制失败，原数据仍保留。\n\n{error}"),
                                MB_OK | MB_ICONERROR,
                            );
                            return Err(());
                        }
                    }
                    IDNO => {}
                    _ => return Err(()),
                }
            }
        }
    }

    if let Err(error) = writable_probe(&root) {
        let prompt = format!(
            "无法在程序旁创建或写入 SHTMCL-data：\n{}\n\n请将 SHTMCL.exe 移到你有写入权限的位置（例如用户文档目录），然后重新启动。SHTMCL 不会请求管理员权限，也不会改写文件夹权限。\n\n错误：{error}",
            root.display()
        );
        message(&prompt, MB_OK | MB_ICONERROR);
        return Err(());
    }

    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::{copy_tree, migrate_legacy, portable_root, writable_probe};
    use std::fs;
    use std::path::Path;

    #[test]
    fn root_is_sibling_of_executable_with_spaces_and_independent_of_cwd() {
        let exe = Path::new(r"D:\Games Folder\SHTMCL.exe");
        assert_eq!(
            portable_root(exe).unwrap(),
            Path::new(r"D:\Games Folder\SHTMCL-data")
        );
    }

    #[test]
    fn data_root_can_be_moved_to_another_drive_without_path_rewriting() {
        let original = portable_root(Path::new(r"C:\Launcher\SHTMCL.exe")).unwrap();
        let moved = portable_root(Path::new(r"E:\Portable\SHTMCL.exe")).unwrap();
        assert_ne!(original, moved);
        assert_eq!(moved, Path::new(r"E:\Portable\SHTMCL-data"));
    }

    #[test]
    fn copy_and_migration_leave_legacy_data_intact_and_commit_atomically() {
        let temp =
            std::env::temp_dir().join(format!("shtmcl-portable-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        let old = temp.join("old data");
        let new = temp.join("app folder").join("SHTMCL-data");
        fs::create_dir_all(old.join("instances")).unwrap();
        fs::write(old.join("instances").join("index.json"), b"old state").unwrap();
        fs::create_dir_all(new.parent().unwrap()).unwrap();

        migrate_legacy(&old, &new).unwrap();
        assert_eq!(
            fs::read(new.join("instances").join("index.json")).unwrap(),
            b"old state"
        );
        assert_eq!(
            fs::read(old.join("instances").join("index.json")).unwrap(),
            b"old state"
        );
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn write_probe_handles_paths_with_spaces() {
        let temp = std::env::temp_dir().join(format!("shtmcl portable {}", std::process::id()));
        writable_probe(&temp).unwrap();
        assert!(temp.is_dir());
        assert_eq!(fs::read_dir(&temp).unwrap().count(), 0);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn copy_tree_copies_nested_regular_files() {
        let temp = std::env::temp_dir().join(format!("shtmcl-copy-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(temp.join("from/a")).unwrap();
        fs::write(temp.join("from/a/file.bin"), [1, 2, 3]).unwrap();
        copy_tree(&temp.join("from"), &temp.join("to")).unwrap();
        assert_eq!(fs::read(temp.join("to/a/file.bin")).unwrap(), [1, 2, 3]);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn failed_migration_cleans_staging_and_keeps_legacy_path_untouched() {
        let temp =
            std::env::temp_dir().join(format!("shtmcl-migration-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        let missing_source = temp.join("missing legacy");
        let destination = temp.join("app").join("SHTMCL-data");
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        assert!(migrate_legacy(&missing_source, &destination).is_err());
        assert!(!destination.exists());
        assert!(!temp
            .join(format!(".SHTMCL-data-migration-{}", std::process::id()))
            .exists());
        let _ = fs::remove_dir_all(temp);
    }
}
