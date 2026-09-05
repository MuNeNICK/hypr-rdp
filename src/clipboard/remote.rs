//! Lazy, read-only materialization of files advertised by the RDP client.

use std::collections::HashMap;
#[cfg(feature = "client-to-server")]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_cliprdr::chunked_fetch::{ChunkedFetch, ChunkedFetchProgress};
use ironrdp_cliprdr::pdu::{FileContentsResponse, FileDescriptor};
use ironrdp_server::ServerEvent;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

use super::formats::PendingWrite;

const READ_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(feature = "client-to-server")]
const FIRST_FILE_INDEX: i32 = 0;
#[cfg(feature = "client-to-server")]
const REMOTE_FILE_INODE: fuser::INodeNo = fuser::INodeNo(2);

#[derive(Clone)]
pub(super) struct RemoteFiles {
    inner: Arc<Mutex<RemoteState>>,
    event_sender: mpsc::UnboundedSender<ServerEvent>,
    runtime: Handle,
    #[cfg(feature = "client-to-server")]
    mount: Arc<Mutex<Option<MountedRemoteFiles>>>,
}

struct RemoteState {
    files: Vec<FileDescriptor>,
    next_stream_id: u32,
    pending: HashMap<u32, PendingRead>,
}

struct PendingRead {
    answer: oneshot::Sender<Result<Vec<u8>, ()>>,
    fetch: ChunkedFetch,
}

#[cfg(feature = "client-to-server")]
struct MountedRemoteFiles {
    _session: fuser::BackgroundSession,
    path: PathBuf,
}

impl RemoteFiles {
    pub(super) fn new(event_sender: mpsc::UnboundedSender<ServerEvent>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RemoteState {
                files: Vec::new(),
                next_stream_id: 1,
                pending: HashMap::new(),
            })),
            event_sender,
            runtime: Handle::current(),
            #[cfg(feature = "client-to-server")]
            mount: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(feature = "client-to-server")]
    pub(super) fn advertise(&self, files: &[FileDescriptor]) -> Option<PendingWrite> {
        let first = files.first()?;
        if !is_safe_file_name(&first.name) {
            tracing::warn!(name = %first.name, "Clipboard: refusing unsafe remote file name");
            return None;
        }
        {
            let mut state = self.inner.lock().ok()?;
            state.files = files.to_vec();
        }

        let path = self.mount()?;

        let uri = format!(
            "{}\r\n",
            url::Url::from_file_path(path.join(&first.name))
                .ok()?
                .as_str()
        );
        let gnome = format!("copy\n{uri}");
        Some(PendingWrite::Files {
            uri_list: uri.into_bytes(),
            gnome_copied_files: gnome.into_bytes(),
        })
    }

    #[cfg(not(feature = "client-to-server"))]
    pub(super) fn advertise(&self, _files: &[FileDescriptor]) -> Option<PendingWrite> {
        tracing::warn!("Clipboard: client-to-server file transfer is not compiled in");
        None
    }

    /// Begins one ranged read. This is the backend seam shared by FUSE and tests.
    pub(super) fn read(
        &self,
        index: i32,
        position: u64,
        requested_size: u32,
    ) -> oneshot::Receiver<Result<Vec<u8>, ()>> {
        let (answer, receiver) = oneshot::channel();
        let (stream_id, request) = {
            let mut state = match self.inner.lock() {
                Ok(state) => state,
                Err(_) => return receiver,
            };
            let total_size = state
                .files
                .get(index as usize)
                .and_then(|file| file.file_size)
                .map(|size| size.saturating_sub(position).min(u64::from(requested_size)))
                .unwrap_or(u64::from(requested_size));
            if total_size == 0 {
                let _ = answer.send(Ok(Vec::new()));
                return receiver;
            }
            let stream_id = state.next_stream_id;
            state.next_stream_id = state.next_stream_id.wrapping_add(1).max(1);
            let mut fetch = ChunkedFetch::new(
                stream_id,
                index,
                total_size,
                requested_size.max(1),
                None,
                total_size,
            );
            let mut request = fetch.next_request().expect("non-empty remote range");
            request.position = position;
            state
                .pending
                .insert(stream_id, PendingRead { answer, fetch });
            (stream_id, request)
        };
        let sender = self.event_sender.clone();
        let state = Arc::clone(&self.inner);
        self.runtime.spawn(async move {
            if sender
                .send(ServerEvent::Clipboard(
                    ClipboardMessage::SendFileContentsRequest(request),
                ))
                .is_err()
            {
                respond(&state, stream_id, Err(()));
                return;
            }
            tokio::time::sleep(READ_TIMEOUT).await;
            respond(&state, stream_id, Err(()));
        });
        receiver
    }

    pub(super) fn on_response(&self, response: FileContentsResponse<'_>) {
        let pending = self
            .inner
            .lock()
            .ok()
            .and_then(|mut state| state.pending.remove(&response.stream_id()));
        let Some(mut pending) = pending else {
            return;
        };
        let result = match pending.fetch.on_response(&response) {
            ChunkedFetchProgress::Complete => Ok(pending.fetch.into_data()),
            ChunkedFetchProgress::InProgress | ChunkedFetchProgress::Failed => Err(()),
        };
        let _ = pending.answer.send(result);
    }

    pub(super) fn cancel_pending(&self) {
        let pending = self
            .inner
            .lock()
            .map(|mut state| std::mem::take(&mut state.pending))
            .unwrap_or_default();
        for (_, pending) in pending {
            let _ = pending.answer.send(Err(()));
        }
    }

    #[cfg(feature = "client-to-server")]
    fn first_file(&self) -> Option<FileDescriptor> {
        self.inner
            .lock()
            .ok()
            .and_then(|state| state.files.first().cloned())
    }

    #[cfg(feature = "client-to-server")]
    fn mount(&self) -> Option<PathBuf> {
        if let Some(mount) = self.mount.lock().ok()?.as_ref() {
            return Some(mount.path.clone());
        }
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")?;
        let path =
            PathBuf::from(runtime_dir).join(format!("hypr-rdp-clipboard-{}", std::process::id()));
        std::fs::create_dir_all(&path).ok()?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).ok()?;
        let filesystem = RemoteFilesystem {
            files: self.clone(),
        };
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            fuser::MountOption::RO,
            fuser::MountOption::FSName("hypr-rdp-clipboard".into()),
        ];
        match fuser::spawn_mount(filesystem, &path, &config) {
            Ok(session) => {
                *self.mount.lock().ok()? = Some(MountedRemoteFiles {
                    _session: session,
                    path: path.clone(),
                });
                Some(path)
            }
            Err(error) => {
                tracing::warn!(%error, "Clipboard: cannot mount remote files");
                None
            }
        }
    }
}

#[cfg(feature = "client-to-server")]
fn is_safe_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\'])
        && std::path::Path::new(name).is_relative()
        && name != "."
        && name != ".."
}

fn respond(state: &Arc<Mutex<RemoteState>>, stream_id: u32, result: Result<Vec<u8>, ()>) {
    if let Ok(mut state) = state.lock() {
        if let Some(pending) = state.pending.remove(&stream_id) {
            let _ = pending.answer.send(result);
        }
    }
}

#[cfg(feature = "client-to-server")]
#[derive(Clone)]
struct RemoteFilesystem {
    files: RemoteFiles,
}

#[cfg(feature = "client-to-server")]
impl fuser::Filesystem for RemoteFilesystem {
    fn getattr(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _fh: Option<fuser::FileHandle>,
        reply: fuser::ReplyAttr,
    ) {
        match file_attr(&self.files, ino) {
            Some(attr) => reply.attr(&Duration::ZERO, &attr),
            None => reply.error(fuser::Errno::ENOENT),
        }
    }

    fn lookup(
        &self,
        _req: &fuser::Request,
        parent: fuser::INodeNo,
        name: &std::ffi::OsStr,
        reply: fuser::ReplyEntry,
    ) {
        let file = self.files.first_file();
        match (parent, file) {
            (fuser::INodeNo::ROOT, Some(file)) if name == std::ffi::OsStr::new(&file.name) => {
                reply.entry(
                    &Duration::ZERO,
                    &file_attr(&self.files, REMOTE_FILE_INODE).expect("remote file exists"),
                    fuser::Generation(0),
                );
            }
            _ => reply.error(fuser::Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        mut reply: fuser::ReplyDirectory,
    ) {
        if ino != fuser::INodeNo::ROOT {
            reply.error(fuser::Errno::ENOTDIR);
            return;
        }
        let file = self.files.first_file();
        let entries = [
            (fuser::INodeNo::ROOT, fuser::FileType::Directory, ".".into()),
            (
                fuser::INodeNo::ROOT,
                fuser::FileType::Directory,
                "..".into(),
            ),
        ]
        .into_iter()
        .chain(
            file.into_iter()
                .map(|file| (REMOTE_FILE_INODE, fuser::FileType::RegularFile, file.name)),
        );
        for (entry_offset, (inode, kind, name)) in entries.enumerate().skip(offset as usize) {
            if reply.add(inode, (entry_offset + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _flags: fuser::OpenFlags,
        reply: fuser::ReplyOpen,
    ) {
        if ino == REMOTE_FILE_INODE {
            reply.opened(fuser::FileHandle(0), fuser::FopenFlags::FOPEN_DIRECT_IO);
        } else {
            reply.error(fuser::Errno::ENOENT);
        }
    }

    fn read(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: fuser::ReplyData,
    ) {
        if ino != REMOTE_FILE_INODE {
            reply.error(fuser::Errno::ENOENT);
            return;
        }
        let receiver = self.files.read(FIRST_FILE_INDEX, offset, size);
        self.files.runtime.spawn(async move {
            match receiver.await {
                Ok(Ok(data)) => reply.data(&data),
                _ => reply.error(fuser::Errno::EIO),
            }
        });
    }
}

#[cfg(feature = "client-to-server")]
fn file_attr(files: &RemoteFiles, ino: fuser::INodeNo) -> Option<fuser::FileAttr> {
    let file = files.first_file()?;
    let now = std::time::SystemTime::now();
    Some(fuser::FileAttr {
        ino,
        size: if ino == fuser::INodeNo::ROOT {
            0
        } else {
            file.file_size.unwrap_or(0)
        },
        blocks: 0,
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: if ino == fuser::INodeNo::ROOT {
            fuser::FileType::Directory
        } else {
            fuser::FileType::RegularFile
        },
        perm: if ino == fuser::INodeNo::ROOT {
            0o500
        } else {
            0o400
        },
        nlink: 1,
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    })
}
