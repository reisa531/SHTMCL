//! File-backed partial transfers and content-addressed cache storage.
//!
//! Artifact identifiers never become path components. Both partial files and
//! cache objects use validated hashes, and all reads/writes are streamed.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use sha1::Sha1;
use sha2::{Digest, Sha256};

use super::state::ArtifactHash;

const BUFFER_SIZE: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("partial"))?;
        fs::create_dir_all(root.join("objects"))?;
        Ok(Self { root })
    }

    fn partial_path(&self, artifact_id: &str) -> PathBuf {
        self.root.join("partial").join(format!(
            "{}.part",
            hex(&Sha256::digest(artifact_id.as_bytes()))
        ))
    }

    fn object_path(&self, hash: &ArtifactHash) -> io::Result<PathBuf> {
        ArtifactHash::new(&hash.algorithm, &hash.digest)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid artifact hash"))?;
        Ok(self
            .root
            .join("objects")
            .join(&hash.algorithm)
            .join(&hash.digest[..2])
            .join(&hash.digest))
    }

    pub fn open_partial(&self, artifact_id: &str, offset: u64) -> io::Result<File> {
        let path = self.partial_path(artifact_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(file)
    }

    pub fn reset_partial(&self, artifact_id: &str) -> io::Result<File> {
        let mut file = self.open_partial(artifact_id, 0)?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        Ok(file)
    }

    pub fn commit_partial(&self, artifact_id: &str) -> io::Result<()> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.partial_path(artifact_id))?
            .sync_all()
    }

    pub fn hash_partial(&self, artifact_id: &str, algorithm: &str) -> io::Result<String> {
        let mut file = File::open(self.partial_path(artifact_id))?;
        let mut buffer = [0u8; BUFFER_SIZE];
        match algorithm.to_ascii_lowercase().as_str() {
            "sha1" => {
                let mut digest = Sha1::new();
                loop {
                    let count = file.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    digest.update(&buffer[..count]);
                }
                Ok(hex(&digest.finalize()))
            }
            "sha256" => {
                let mut digest = Sha256::new();
                loop {
                    let count = file.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    digest.update(&buffer[..count]);
                }
                Ok(hex(&digest.finalize()))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported hash algorithm",
            )),
        }
    }

    pub fn publish(&self, hash: &ArtifactHash, artifact_id: &str) -> io::Result<u64> {
        let partial = self.partial_path(artifact_id);
        let size = fs::metadata(&partial)?.len();
        let object = self.object_path(hash)?;
        fs::create_dir_all(object.parent().expect("object path has parent"))?;

        if object.exists() {
            if self.hash_file(&object, &hash.algorithm)? == hash.digest {
                remove_if_exists(&partial)?;
                return Ok(fs::metadata(object)?.len());
            }
            // A cache object may have been modified outside the launcher. Remove
            // it only after the new partial has already passed its digest check.
            fs::remove_file(&object)?;
        }
        fs::rename(partial, &object)?;
        Ok(size)
    }

    pub fn materialize(&self, hash: &ArtifactHash, artifact_id: &str) -> io::Result<Option<u64>> {
        let object = self.object_path(hash)?;
        if !object.is_file() {
            return Ok(None);
        }
        if self.hash_file(&object, &hash.algorithm)? != hash.digest {
            return Ok(None);
        }
        let partial = self.partial_path(artifact_id);
        if let Some(parent) = partial.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = partial.with_extension("part.tmp");
        let mut source = File::open(&object)?;
        let mut target = File::create(&temp)?;
        io::copy(&mut source, &mut target)?;
        target.sync_all()?;
        let size = target.metadata()?.len();
        drop(target);
        replace_file(&temp, &partial)?;
        Ok(Some(size))
    }

    /// Verify an existing object without copying it into partial storage.
    /// Missing objects return `None`; present but corrupt objects remain in
    /// place and return `InvalidData` so callers can distinguish both cases.
    /// Linked and nonregular paths return `PermissionDenied`, never a cache miss.
    pub fn verify_cached(&self, hash: &ArtifactHash) -> io::Result<Option<u64>> {
        let hash = ArtifactHash::new(&hash.algorithm, &hash.digest)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid artifact hash"))?;
        let object = self.object_path(&hash)?;
        let ancestors = object.ancestors().collect::<Vec<_>>();
        for path in ancestors
            .into_iter()
            .rev()
            .filter(|p| !p.as_os_str().is_empty())
        {
            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
            reject_cache_link(&metadata)?;
            if path != object && !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cache ancestor is not a directory",
                ));
            }
            if path == object && !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cache object is not a regular file",
                ));
            }
        }

        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Open the final reparse point itself rather than following it.
            options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        let mut file = match options.open(&object) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        reject_cache_link(&metadata)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cache object is not a regular file",
            ));
        }
        let mut buffer = [0u8; BUFFER_SIZE];
        let mut size = 0u64;
        let mut sha1 = Sha1::new();
        let mut sha256 = Sha256::new();
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            size += count as u64;
            if hash.algorithm == "sha1" {
                sha1.update(&buffer[..count]);
            } else {
                sha256.update(&buffer[..count]);
            }
        }
        let actual = if hash.algorithm == "sha1" {
            hex(&sha1.finalize())
        } else {
            hex(&sha256.finalize())
        };
        if actual != hash.digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cache object digest does not match",
            ));
        }
        Ok(Some(size))
    }

    pub fn remove_cached(&self, hash: &ArtifactHash) -> io::Result<()> {
        remove_if_exists(&self.object_path(hash)?)
    }

    pub fn discard(&self, artifact_id: &str) -> io::Result<()> {
        remove_if_exists(&self.partial_path(artifact_id))
    }

    fn hash_file(&self, path: &Path, algorithm: &str) -> io::Result<String> {
        let mut file = File::open(path)?;
        let mut buffer = [0u8; BUFFER_SIZE];
        match algorithm.to_ascii_lowercase().as_str() {
            "sha1" => {
                let mut digest = Sha1::new();
                loop {
                    let count = file.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    digest.update(&buffer[..count]);
                }
                Ok(hex(&digest.finalize()))
            }
            "sha256" => {
                let mut digest = Sha256::new();
                loop {
                    let count = file.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    digest.update(&buffer[..count]);
                }
                Ok(hex(&digest.finalize()))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported hash algorithm",
            )),
        }
    }
}

fn reject_cache_link(metadata: &fs::Metadata) -> io::Result<()> {
    #[cfg(windows)]
    let reparse_point = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse_point = false;
    if metadata.file_type().is_symlink() || reparse_point {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cache path contains a link",
        ))
    } else {
        Ok(())
    }
}

pub struct FileDisk {
    root: PathBuf,
}

impl FileDisk {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl super::ports::DiskPort for FileDisk {
    fn available(&self) -> Result<u64, String> {
        free_bytes(&self.root).map_err(|error| format!("downloads.disk.query:{error}"))
    }
}

#[cfg(windows)]
fn free_bytes(path: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            directory_name: *const u16,
            free_bytes_available: *mut u64,
            total_bytes: *mut u64,
            total_free_bytes: *mut u64,
        ) -> i32;
    }

    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let mut available = 0;
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, null_mut(), null_mut()) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(available)
    }
}

#[cfg(unix)]
fn free_bytes(path: &Path) -> io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let stats = unsafe { stats.assume_init() };
    Ok((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

#[cfg(not(any(windows, unix)))]
fn free_bytes(_path: &Path) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "free-space adapter is not implemented for this platform",
    ))
}

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(source, destination)
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("shtmcl-download-store-{nonce}"))
    }

    #[test]
    fn artifact_id_is_not_used_as_a_path() {
        let root = temp_root();
        let store = FileStore::new(&root).unwrap();
        store
            .open_partial("../../outside", 0)
            .unwrap()
            .write_all(b"data")
            .unwrap();
        store.commit_partial("../../outside").unwrap();
        assert!(store.partial_path("../../outside").starts_with(&root));
        store.discard("../../outside").unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hash_and_content_addressed_publish_are_streamed() {
        let root = temp_root();
        let store = FileStore::new(&root).unwrap();
        store
            .open_partial("resource", 0)
            .unwrap()
            .write_all(b"hello")
            .unwrap();
        store.commit_partial("resource").unwrap();
        let hash = ArtifactHash::new("sha1", "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        assert_eq!(store.hash_partial("resource", "sha1").unwrap(), hash.digest);
        assert_eq!(store.publish(&hash, "resource").unwrap(), 5);
        assert_eq!(
            store.materialize(&hash, "another-resource").unwrap(),
            Some(5)
        );
        store.discard("another-resource").unwrap();
        store.remove_cached(&hash).unwrap();
        store.remove_cached(&hash).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_verification_checks_both_hashes_without_materializing() {
        let root = temp_root();
        let store = FileStore::new(&root).unwrap();
        for bytes in [Vec::new(), vec![42; BUFFER_SIZE * 2 + 17]] {
            for algorithm in ["sha1", "sha256"] {
                let digest = if algorithm == "sha1" {
                    hex(&Sha1::digest(&bytes))
                } else {
                    hex(&Sha256::digest(&bytes))
                };
                let hash = ArtifactHash::new(algorithm, &digest).unwrap();
                assert_eq!(store.verify_cached(&hash).unwrap(), None);
                store
                    .open_partial("resource", 0)
                    .unwrap()
                    .write_all(&bytes)
                    .unwrap();
                store.publish(&hash, "resource").unwrap();
                assert_eq!(
                    store.verify_cached(&hash).unwrap(),
                    Some(bytes.len() as u64)
                );
                assert_eq!(fs::read_dir(root.join("partial")).unwrap().count(), 0);
                store.remove_cached(&hash).unwrap();
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_verification_rejects_corruption_invalid_hash_and_nonregular_object() {
        let root = temp_root();
        let store = FileStore::new(&root).unwrap();
        let hash = ArtifactHash::new("sha1", &hex(&Sha1::digest(b"hello"))).unwrap();
        store
            .open_partial("resource", 0)
            .unwrap()
            .write_all(b"hello")
            .unwrap();
        store.publish(&hash, "resource").unwrap();
        let object = store.object_path(&hash).unwrap();
        fs::write(&object, b"other").unwrap();
        assert_eq!(
            store.verify_cached(&hash).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(&object).unwrap(), b"other");
        fs::remove_file(&object).unwrap();
        fs::create_dir(&object).unwrap();
        assert_eq!(
            store.verify_cached(&hash).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let invalid = ArtifactHash {
            algorithm: "sha1".into(),
            digest: "../bad".into(),
        };
        assert_eq!(
            store.verify_cached(&invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn cached_verification_rejects_linked_file_directory_and_root() {
        let root = temp_root();
        let store = FileStore::new(&root).unwrap();
        let hash = ArtifactHash::new("sha1", &hex(&Sha1::digest(b"hello"))).unwrap();
        let object = store.object_path(&hash).unwrap();
        fs::create_dir_all(object.parent().unwrap()).unwrap();
        let target = root.join("target");
        fs::write(&target, b"hello").unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&target, &object);
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&target, &object);
        if let Err(error) = linked {
            #[cfg(windows)]
            if error.raw_os_error() == Some(1314) {
                eprintln!("symlink test skipped: Windows symlink privilege unavailable");
                fs::remove_dir_all(root).unwrap();
                return;
            }
            panic!("create test symlink: {error}");
        }
        assert_eq!(
            store.verify_cached(&hash).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::remove_file(&object).unwrap();
        // A missing object below a directory link must also be rejected.
        let directory = object.parent().unwrap();
        fs::remove_dir(directory).unwrap();
        let target_directory = root.join("target-directory");
        fs::create_dir(&target_directory).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target_directory, directory).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target_directory, directory).unwrap();
        assert_eq!(
            store.verify_cached(&hash).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        #[cfg(unix)]
        fs::remove_file(directory).unwrap();
        #[cfg(windows)]
        fs::remove_dir(directory).unwrap();

        let alias = root.with_extension("linked-root");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&root, &alias).unwrap();
        let linked_store = FileStore {
            root: alias.clone(),
        };
        assert_eq!(
            linked_store.verify_cached(&hash).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        #[cfg(unix)]
        fs::remove_file(alias).unwrap();
        #[cfg(windows)]
        fs::remove_dir(alias).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
