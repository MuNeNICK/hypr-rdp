//! Private, claimed mount paths. All calls, including Drop, belong to the mount service.
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const PREFIX: &str = "hypr-rdp-clipboard-";
const FS_NAME: &str = "hypr-rdp-clipboard";
static NEXT_MOUNT: AtomicU64 = AtomicU64::new(0);

pub(super) struct RemoteMount {
    session: Option<fuser::BackgroundSession>,
    claim: Claim,
}

struct Claim {
    path: PathBuf,
    _lock: File,
}

impl RemoteMount {
    pub(super) fn create<FS: fuser::Filesystem + Send + 'static>(filesystem: FS) -> Option<Self> {
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
        if !private_runtime(&runtime) {
            return None;
        }
        sweep(&runtime, unmount_owned);
        let claim = claim_directory(&runtime)
            .map_err(|error| {
                tracing::warn!(%error, "Clipboard: cannot create inbound mount directory");
            })
            .ok()?;
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            fuser::MountOption::RO,
            fuser::MountOption::FSName(FS_NAME.into()),
        ];
        match fuser::spawn_mount(filesystem, &claim.path, &config) {
            Ok(session) => Some(Self {
                session: Some(session),
                claim,
            }),
            Err(error) => {
                tracing::warn!(%error, "Clipboard: cannot mount incoming files; install fusermount3 and allow FUSE access");
                cleanup(&claim);
                None
            }
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.claim.path
    }
}

impl Drop for RemoteMount {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            if let Err(error) = session.umount_and_join() {
                tracing::warn!(%error, "Clipboard: failed to unmount incoming files");
            }
        }
        cleanup(&self.claim);
    }
}

fn private_runtime(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|m| m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0)
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    name.into()
}

fn lock(path: &Path, create: bool) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(lock_path(path))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    // The open file owns the advisory claim until cleanup is finished.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

fn claim_directory(runtime: &Path) -> std::io::Result<Claim> {
    claim_directory_with(runtime, |_| {})
}

fn claim_directory_with(
    runtime: &Path,
    mut before_visibility: impl FnMut(&Path),
) -> std::io::Result<Claim> {
    for _ in 0..32 {
        let sequence = NEXT_MOUNT.fetch_add(1, Ordering::Relaxed);
        let path = runtime.join(format!("{PREFIX}{}-{sequence}", std::process::id()));
        // Exclusive lock creation precedes directory visibility to concurrent sweeps.
        let file = match lock(&path, true) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        before_visibility(&path);
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(Claim { path, _lock: file }),
            Err(error) => {
                let _ = std::fs::remove_file(lock_path(&path));
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
        }
    }
    Err(std::io::ErrorKind::AlreadyExists.into())
}

fn cleanup(claim: &Claim) {
    // Keep a failed cleanup claim on disk for a subsequent sweep.
    if std::fs::remove_dir(&claim.path).is_ok() {
        let _ = std::fs::remove_file(lock_path(&claim.path));
    }
}

fn ours(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(pid, seq)| {
            pid.parse::<u32>().is_ok_and(|id| id > 0) && seq.parse::<u64>().is_ok()
        })
}

fn sweep(runtime: &Path, unmount: impl Fn(&Path) -> bool) {
    let Ok(entries) = std::fs::read_dir(runtime) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_str().is_some_and(ours) {
            continue;
        }
        // d_type avoids touching the contents of a disconnected FUSE mount.
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        // Missing locks and failed claims do not authorize touching a directory.
        let Ok(file) = lock(&path, false) else {
            continue;
        };
        let claim = Claim { path, _lock: file };
        if unmount(&claim.path) {
            cleanup(&claim);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum MountKind {
    Absent,
    Ours,
    Other,
}

fn mount_kind(path: &Path, table: &[u8]) -> MountKind {
    let escaped = escape_mount_point(path.as_os_str().as_encoded_bytes());
    for line in table.split(|b| *b == b'\n') {
        let fields: Vec<_> = line.split(|b| *b == b' ').collect();
        if fields.get(4).copied() != Some(escaped.as_slice()) {
            continue;
        }
        let Some(separator) = fields.iter().position(|field| *field == b"-") else {
            return MountKind::Other;
        };
        let fs = fields.get(separator + 1).copied();
        let source = fields.get(separator + 2).copied();
        return if matches!(fs, Some(b"fuse" | b"fuse.hypr-rdp-clipboard"))
            && source == Some(FS_NAME.as_bytes())
        {
            MountKind::Ours
        } else {
            MountKind::Other
        };
    }
    MountKind::Absent
}

fn escape_mount_point(path: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::new();
    for byte in path {
        match byte {
            b' ' => escaped.extend_from_slice(b"\\040"),
            b'\t' => escaped.extend_from_slice(b"\\011"),
            b'\n' => escaped.extend_from_slice(b"\\012"),
            b'\\' => escaped.extend_from_slice(b"\\134"),
            other => escaped.push(*other),
        }
    }
    escaped
}

fn unmount_owned(path: &Path) -> bool {
    let Ok(table) = std::fs::read("/proc/self/mountinfo") else {
        return false;
    };
    match mount_kind(path, &table) {
        MountKind::Absent => return true,
        MountKind::Other => return false,
        MountKind::Ours => {}
    }
    let program = std::env::var_os("FUSERMOUNT_PATH").unwrap_or_else(|| "fusermount3".into());
    let Ok(mut child) = Command::new(program)
        .args(["-u", "-q", "-z"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Runtime(PathBuf);
    impl Runtime {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hypr-rdp-mount-test-{}-{}",
                std::process::id(),
                NEXT_MOUNT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }
    }
    impl Drop for Runtime {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn inbound_mount_claim_precedes_visibility() {
        let runtime = Runtime::new();
        let claim = claim_directory_with(&runtime.0, |path| {
            assert!(
                !path.exists(),
                "directory must not precede its ownership claim"
            );
            assert!(lock_path(path).is_file());
            assert!(lock(path, false).is_err());
            sweep(&runtime.0, |_| {
                panic!("creation window must not be sweepable")
            });
        })
        .unwrap();
        assert!(lock_path(&claim.path).is_file());
        sweep(&runtime.0, |_| panic!("a creator still owns the claim"));
        assert!(claim.path.is_dir());
        assert!(lock(&claim.path, false).is_err());
        cleanup(&claim);
        assert!(!claim.path.exists());
        assert!(!lock_path(&claim.path).exists());
    }

    #[test]
    fn inbound_mount_cleanup_retains_claim() {
        let runtime = Runtime::new();
        let claim = claim_directory(&runtime.0).unwrap();
        let path = claim.path.clone();
        drop(claim); // Models process exit: kernel drops the claim, files remain.
        let called = std::cell::Cell::new(false);
        sweep(&runtime.0, |path| {
            assert!(
                lock(path, false).is_err(),
                "sweeper must retain the claim through unmount"
            );
            called.set(true);
            true
        });
        assert!(called.get());
        assert!(!path.exists());
        assert!(!lock_path(&path).exists());
    }

    #[test]
    fn inbound_sweep_preserves_unclaimed_foreign_and_symlink_paths() {
        use std::os::unix::fs::symlink;
        let runtime = Runtime::new();
        let unclaimed = runtime.0.join(format!("{PREFIX}123-9"));
        std::fs::create_dir(&unclaimed).unwrap();
        let alias = runtime.0.join(format!("{PREFIX}123-10"));
        symlink(&unclaimed, &alias).unwrap();
        let claim = claim_directory(&runtime.0).unwrap();
        let foreign = claim.path.clone();
        drop(claim);
        sweep(&runtime.0, |path| {
            assert_eq!(path, foreign);
            false
        });
        assert!(foreign.is_dir());
        assert!(unclaimed.is_dir());
        assert!(alias.is_symlink());
        let fake_lock = runtime.0.join(format!("{PREFIX}123-11"));
        symlink(lock_path(&foreign), lock_path(&fake_lock)).unwrap();
        assert!(lock(&fake_lock, false).is_err());
    }

    #[test]
    fn inbound_mount_table_checks_type_source_and_escaped_path() {
        let path = Path::new("/run/user/1000/a b");
        for (fs, source, expected) in [
            ("fuse", FS_NAME, MountKind::Ours),
            ("fuse.hypr-rdp-clipboard", FS_NAME, MountKind::Ours),
            ("fuse", "sshfs", MountKind::Other),
            ("ext4", FS_NAME, MountKind::Other),
        ] {
            let table = format!("1 2 0:1 / /run/user/1000/a\\040b rw - {fs} {source} rw\n");
            assert_eq!(mount_kind(path, table.as_bytes()), expected);
            assert_eq!(
                mount_kind(Path::new("/other"), table.as_bytes()),
                MountKind::Absent
            );
        }
    }
}
