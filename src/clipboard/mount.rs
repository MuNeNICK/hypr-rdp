//! The lifecycle of the mount that makes the client's advertised files browsable.
//!
//! Automatic unmount is deliberately not used: it requires permitting other
//! users to access the mount, which requires the machine's administrator to
//! edit a system configuration file — friction that contradicts working by
//! default, and an unnecessary widening of who can read the mount. Instead a
//! mount is unmounted when its session ends, and a mount orphaned by an
//! abnormal exit is swept away the next time the server starts. The runtime
//! directory is cleared at logout, so the window for stale state is a single
//! session.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Every directory this server mounts into is named this way, so the sweep can
/// tell its own leftovers from everything else in the runtime directory.
const MOUNT_PREFIX: &str = "hypr-rdp-clipboard-";

/// Distinguishes concurrent mounts within one process, so a session starting
/// before its predecessor has finished dropping does not collide with it.
static NEXT_MOUNT: AtomicU64 = AtomicU64::new(0);

/// A mounted view of one session's remote files, unmounted when it is dropped.
pub(super) struct RemoteMount {
    session: Option<fuser::BackgroundSession>,
    path: PathBuf,
    /// Held for as long as this mount exists. The kernel releases it however
    /// this process exits, which is what lets the next start tell an orphan
    /// from a mount a concurrent instance is still serving.
    _lock: File,
}

impl RemoteMount {
    /// Mounts `filesystem` under a private directory in the user's runtime
    /// directory. `None` means the paste has nowhere to point at, which the
    /// caller reports rather than advertising an empty selection.
    pub(super) fn create<FS: fuser::Filesystem + Send + 'static>(filesystem: FS) -> Option<Self> {
        let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
            tracing::warn!("Clipboard: XDG_RUNTIME_DIR is unset, cannot mount remote files");
            return None;
        };
        Self::create_in(Path::new(&runtime_dir), filesystem)
    }

    /// The mount itself, over an injected runtime directory, so the path a
    /// system that cannot mount takes is exercised without one.
    fn create_in<FS: fuser::Filesystem + Send + 'static>(
        runtime_dir: &Path,
        filesystem: FS,
    ) -> Option<Self> {
        let path = runtime_dir.join(mount_dir_name(
            std::process::id(),
            NEXT_MOUNT.fetch_add(1, Ordering::Relaxed),
        ));
        if let Err(error) = create_private_dir(&path) {
            tracing::warn!(%error, path = %path.display(), "Clipboard: cannot create the mount directory");
            return None;
        }
        let lock = match hold_lock(&path) {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "Clipboard: cannot claim the mount");
                let _ = std::fs::remove_dir(&path);
                return None;
            }
        };

        let mut config = fuser::Config::default();
        config.mount_options = vec![
            fuser::MountOption::RO,
            fuser::MountOption::FSName("hypr-rdp-clipboard".into()),
        ];
        match fuser::spawn_mount(filesystem, &path, &config) {
            Ok(session) => {
                tracing::debug!(path = %path.display(), "Clipboard: mounted the client's files");
                Some(Self {
                    session: Some(session),
                    path,
                    _lock: lock,
                })
            }
            Err(error) => {
                tracing::warn!(%error, "Clipboard: cannot mount remote files");
                // Leaving these behind would hand the next start's sweep work
                // that never was a mount.
                let _ = std::fs::remove_dir(&path);
                let _ = std::fs::remove_file(lock_path(&path));
                None
            }
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RemoteMount {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            // Unmount before removing the directory: a mount point is busy
            // until the kernel has let go of it.
            if let Err(error) = session.umount_and_join() {
                tracing::warn!(%error, path = %self.path.display(), "Clipboard: unmounting remote files failed");
            }
        }
        if let Err(error) = std::fs::remove_dir(&self.path) {
            tracing::warn!(%error, path = %self.path.display(), "Clipboard: the mount directory was left behind");
        }
        let _ = std::fs::remove_file(lock_path(&self.path));
    }
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

fn mount_dir_name(pid: u32, sequence: u64) -> String {
    format!("{MOUNT_PREFIX}{pid}-{sequence}")
}

/// Whether a directory name is one this module produced. The sweep must not
/// touch what it did not create, and the process id the name carries is for
/// the operator reading a log line — what a mount is still in use is settled
/// by its lock, not by its name.
fn is_mount_dir_name(name: &str) -> bool {
    let Some((pid, sequence)) = name
        .strip_prefix(MOUNT_PREFIX)
        .and_then(|rest| rest.split_once('-'))
    else {
        return false;
    };
    sequence.parse::<u64>().is_ok() && pid.parse::<u32>().is_ok_and(|pid| pid > 0)
}

/// The lock sitting beside a mount directory, rather than inside it: once the
/// filesystem is mounted, nothing in the directory is on the local disk.
fn lock_path(mount: &Path) -> PathBuf {
    let mut name = mount.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Claims a mount for this process for as long as the returned file is open.
fn hold_lock(mount: &Path) -> std::io::Result<File> {
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(mount))?;
    // SAFETY: the descriptor is owned by `lock` and outlives the call.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(lock)
}

/// Whether nothing holds this mount any more. A process id cannot answer that
/// — ids are reused, and a reused one would hide an orphan for the rest of the
/// session — but a lock the kernel releases on exit, however that exit
/// happened, answers it exactly.
fn is_orphaned(mount: &Path) -> bool {
    let path = lock_path(mount);
    if !path.exists() {
        // Nothing claimed it: either the claim was already cleaned up, or the
        // mount predates the lock. Either way nobody is serving it.
        return true;
    }
    // Taking the lock is the test; releasing it immediately leaves the mount
    // exactly as it was for the caller to decide about.
    hold_lock(mount).is_ok()
}

/// Removes the mounts of servers that exited without unmounting.
pub(crate) fn sweep_orphan_mounts() {
    let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    for path in sweep(Path::new(&runtime_dir), is_orphaned, unmount) {
        tracing::info!(path = %path.display(), "Clipboard: removed an orphaned remote-files mount");
    }
}

/// The sweep itself, over an injected view of which mounts are orphaned and of
/// how a path is unmounted, so it can be exercised without a mount.
fn sweep(
    runtime_dir: &Path,
    is_orphaned: impl Fn(&Path) -> bool,
    unmount: impl Fn(&Path),
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    let mut swept = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_name().to_str().is_some_and(is_mount_dir_name) {
            continue;
        }
        let path = entry.path();
        // A mount still claimed is this process's own or a concurrent
        // instance's, and neither is the sweep's to remove.
        if !is_orphaned(&path) {
            continue;
        }
        unmount(&path);
        match std::fs::remove_dir(&path) {
            Ok(()) => {
                let _ = std::fs::remove_file(lock_path(&path));
                swept.push(path);
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "Clipboard: cannot remove an orphaned mount");
            }
        }
    }
    swept
}

/// Whether the kernel still has something mounted at `path`. The mount table
/// is read rather than the path stat-ed, because a mount whose server died
/// answers a stat with ENOTCONN — which is exactly the case being swept.
fn is_mount_point(path: &Path) -> bool {
    // Read as bytes: a path is not required to be UTF-8, and one that is not
    // must still match rather than silently never matching.
    let Ok(mounts) = std::fs::read("/proc/self/mountinfo") else {
        return false;
    };
    let mount_point = escape_mount_point(path.as_os_str().as_encoded_bytes());
    // Field five of every line is the mount point, escaped the same way.
    mounts
        .split(|byte| *byte == b'\n')
        .any(|line| line.split(|byte| *byte == b' ').nth(4) == Some(mount_point.as_slice()))
}

fn escape_mount_point(path: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(path.len());
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

/// Unmounts a path this process did not mount, which the crate offers no API
/// for. Nothing here needs a configuration change on the user's machine: the
/// mount helper is the same setuid binary that mounting already goes through.
fn unmount(path: &Path) {
    // A directory left behind by a mount that never succeeded is not mounted,
    // and asking six mount helpers to unmount it in turn would only be noise.
    if !is_mount_point(path) {
        return;
    }
    // The plain syscall is not an option: the kernel refuses it to the
    // unprivileged user whose mount this is, which is the whole reason the
    // setuid helper exists.
    for program in unmount_programs() {
        // Lazily, so a mount some other process still holds open detaches now
        // rather than staying until that process notices.
        match Command::new(&program)
            .arg("-u")
            .arg("-q")
            .arg("-z")
            .arg(path)
            .status()
        {
            Ok(status) if status.success() => return,
            _ => continue,
        }
    }
    tracing::warn!(path = %path.display(), "Clipboard: no mount helper could unmount an orphan");
}

/// Where to look for a mount helper, most specific first: the environment
/// override both this module and the filesystem crate honour, then the names
/// and paths the crate itself searches, then the NixOS wrapper directory the
/// crate does not know about but a self-built binary still has to reach.
fn unmount_programs() -> Vec<String> {
    let mut programs = Vec::new();
    if let Some(configured) = std::env::var_os("FUSERMOUNT_PATH") {
        programs.push(configured.to_string_lossy().into_owned());
    }
    programs.extend(
        [
            "fusermount3",
            "fusermount",
            "/sbin/fusermount3",
            "/sbin/fusermount",
            "/bin/fusermount3",
            "/bin/fusermount",
            // NixOS keeps its setuid wrappers outside any package path and
            // outside a minimal unit's PATH, so name them.
            "/run/wrappers/bin/fusermount3",
            "/run/wrappers/bin/fusermount",
        ]
        .map(str::to_owned),
    );
    programs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A runtime directory holding the given entries, removed when dropped.
    struct RuntimeDir {
        path: PathBuf,
    }

    impl RuntimeDir {
        fn with(names: &[&str]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "hypr-rdp-sweep-{}-{}",
                std::process::id(),
                NEXT_MOUNT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            for name in names {
                std::fs::create_dir_all(path.join(name)).unwrap();
            }
            Self { path }
        }

        fn holds(&self, name: &str) -> bool {
            self.path.join(name).exists()
        }
    }

    impl Drop for RuntimeDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// A system that cannot mount — no FUSE, no mount helper, an unusable
    /// runtime directory — must cost the operator the paste direction and
    /// nothing else. `create` answering `None` is what lets the caller carry
    /// on with the session instead of ending it.
    #[test]
    fn a_mount_that_cannot_be_created_degrades_to_nothing_rather_than_failing() {
        struct NoFilesystem;
        impl fuser::Filesystem for NoFilesystem {}

        let unusable = Path::new("/proc/hypr-rdp-has-no-runtime-directory-here");

        assert!(RemoteMount::create_in(unusable, NoFilesystem).is_none());
        assert!(!unusable.exists(), "nothing is left behind");
    }

    #[test]
    fn an_orphaned_mount_is_unmounted_and_removed() {
        let dir = RuntimeDir::with(&[&mount_dir_name(4242, 0)]);
        let orphan = dir.path.join(mount_dir_name(4242, 0));
        let unmounted = Mutex::new(Vec::new());

        let swept = sweep(
            &dir.path,
            |_| true,
            |path| unmounted.lock().unwrap().push(path.to_path_buf()),
        );

        assert_eq!(swept, std::slice::from_ref(&orphan));
        assert_eq!(*unmounted.lock().unwrap(), [orphan]);
        assert!(!dir.holds(&mount_dir_name(4242, 0)));
    }

    #[test]
    fn a_mount_a_concurrent_instance_still_holds_is_left_alone() {
        let dir = RuntimeDir::with(&[&mount_dir_name(4242, 0), &mount_dir_name(4243, 7)]);
        let orphan = dir.path.join(mount_dir_name(4242, 0));
        let in_use = dir.path.join(mount_dir_name(4243, 7));
        let unmounted = Mutex::new(Vec::new());

        let swept = sweep(
            &dir.path,
            |mount| mount != in_use,
            |path| unmounted.lock().unwrap().push(path.to_path_buf()),
        );

        assert_eq!(swept, std::slice::from_ref(&orphan));
        assert_eq!(*unmounted.lock().unwrap(), [orphan]);
        assert!(dir.holds(&mount_dir_name(4243, 7)));
    }

    /// The claim, not the process id, is what separates the two cases above:
    /// an id can be handed to an unrelated process, and a mount hidden behind
    /// a reused id would never be swept at all.
    #[test]
    fn a_mount_is_orphaned_exactly_while_nobody_holds_its_lock() {
        let dir = RuntimeDir::with(&[&mount_dir_name(4242, 0)]);
        let mount = dir.path.join(mount_dir_name(4242, 0));
        assert!(is_orphaned(&mount), "an unclaimed mount is an orphan");

        let claim = hold_lock(&mount).expect("the mount is claimable");
        assert!(!is_orphaned(&mount), "a claimed mount is still in use");

        drop(claim);

        assert!(is_orphaned(&mount), "a released mount is an orphan again");
        std::fs::remove_file(lock_path(&mount)).unwrap();
    }

    #[test]
    fn the_sweep_never_touches_a_directory_it_did_not_create() {
        let names = [
            "pulse",
            "hypr-rdp-clipboard",
            "hypr-rdp-clipboard-",
            "hypr-rdp-clipboard-notapid-0",
            "hypr-rdp-clipboard-4242",
            "hypr-rdp-clipboard-4242-later",
            "hypr-rdp-clipboard-0-0",
        ];
        let dir = RuntimeDir::with(&names);
        let unmounted = Mutex::new(Vec::new());

        let swept = sweep(
            &dir.path,
            |_| true,
            |path| unmounted.lock().unwrap().push(path.to_path_buf()),
        );

        assert!(swept.is_empty());
        assert!(unmounted.lock().unwrap().is_empty());
        for name in names {
            assert!(dir.holds(name), "{name} was swept away");
        }
    }

    /// The mount itself is not unit-testable — these need a kernel that will
    /// mount FUSE for this user — but the lifecycle they pin down is the whole
    /// of the hygiene this module exists for, so they are kept runnable with
    /// `cargo test -- --ignored` rather than left as prose.
    pub(super) struct NothingMounted;

    impl fuser::Filesystem for NothingMounted {}

    /// They share one runtime directory and one process id, and the sweep is
    /// defined over exactly that pair, so they cannot run at the same time.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|held| held.into_inner())
    }

    #[test]
    #[ignore = "requires a kernel and mount helper that will mount FUSE for this user"]
    fn a_session_that_ends_normally_unmounts_and_removes_its_directory() {
        let _serialized = serialized();
        let mount = RemoteMount::create(NothingMounted).expect("the remote files mount");
        let path = mount.path().to_path_buf();
        assert!(is_mount_point(&path), "{} is not mounted", path.display());

        drop(mount);

        assert!(
            !is_mount_point(&path),
            "{} is still mounted",
            path.display()
        );
        assert!(!path.exists(), "{} was left behind", path.display());
    }

    /// Sessions overlap: a reconnect can build its backend before the previous
    /// one has finished dropping. Naming a mount after the process alone would
    /// put the second one on top of the first.
    #[test]
    #[ignore = "requires a kernel and mount helper that will mount FUSE for this user"]
    fn two_sessions_in_one_process_mount_and_unmount_separately() {
        let _serialized = serialized();
        let first = RemoteMount::create(NothingMounted).expect("the first mount");
        let second = RemoteMount::create(NothingMounted).expect("the second mount");
        let (first_path, second_path) = (first.path().to_path_buf(), second.path().to_path_buf());
        assert_ne!(first_path, second_path);
        assert!(is_mount_point(&first_path) && is_mount_point(&second_path));

        drop(first);

        assert!(!is_mount_point(&first_path) && !first_path.exists());
        assert!(
            is_mount_point(&second_path),
            "the surviving session lost its mount"
        );

        drop(second);

        assert!(!is_mount_point(&second_path) && !second_path.exists());
    }

    #[test]
    #[ignore = "requires a kernel and mount helper that will mount FUSE for this user"]
    fn the_sweep_unmounts_a_mount_no_drop_ever_ran_for() {
        let _serialized = serialized();
        let mount = RemoteMount::create(NothingMounted).expect("the remote files mount");
        let path = mount.path().to_path_buf();
        // What an abnormal exit leaves behind: still mounted, with the claim
        // released by the kernel and no drop left to run. Closing the
        // descriptor is exactly what dying does to it; forgetting the mount
        // afterwards keeps its unmount from running and its file from being
        // closed twice.
        // SAFETY: the descriptor is open here and never closed again.
        unsafe { libc::close(mount._lock.as_raw_fd()) };
        std::mem::forget(mount);
        assert!(is_mount_point(&path));
        assert!(is_orphaned(&path));

        let swept = sweep(path.parent().unwrap(), is_orphaned, unmount);

        assert!(
            swept.contains(&path),
            "the sweep passed over {}",
            path.display()
        );
        assert!(
            !is_mount_point(&path),
            "{} is still mounted",
            path.display()
        );
        assert!(!path.exists(), "{} was left behind", path.display());
    }

    /// Two mounts of one process conflict on the lock the same way two
    /// processes do, so a running server's own mount survives its own sweep.
    #[test]
    fn a_mount_this_process_is_still_serving_survives_its_own_sweep() {
        let dir = RuntimeDir::with(&[&mount_dir_name(std::process::id(), 0)]);
        let mount = dir.path.join(mount_dir_name(std::process::id(), 0));
        let _claim = hold_lock(&mount).expect("the mount is claimable");

        let swept = sweep(&dir.path, is_orphaned, |_| {});

        assert!(swept.is_empty());
        assert!(dir.holds(&mount_dir_name(std::process::id(), 0)));
    }
}
