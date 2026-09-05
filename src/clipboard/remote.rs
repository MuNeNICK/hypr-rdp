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
#[cfg(feature = "client-to-server")]
use super::remote_tree::{RemoteNode, RemoteNodeKind, RemoteTree};

const READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(super) struct RemoteFiles {
    inner: Arc<Mutex<RemoteState>>,
    event_sender: mpsc::UnboundedSender<ServerEvent>,
    runtime: Handle,
    #[cfg(feature = "client-to-server")]
    max_entries: usize,
    #[cfg(feature = "client-to-server")]
    mount: Arc<Mutex<Option<MountedRemoteFiles>>>,
}

struct RemoteState {
    /// The client's list, in the order it sent it: a content request names a
    /// file by its index here.
    files: Vec<FileDescriptor>,
    /// The same list as the browsable tree its relative paths describe.
    #[cfg(feature = "client-to-server")]
    tree: RemoteTree,
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
    pub(super) fn new(
        event_sender: mpsc::UnboundedSender<ServerEvent>,
        max_entries: usize,
    ) -> Self {
        #[cfg(not(feature = "client-to-server"))]
        let _ = max_entries;
        Self {
            inner: Arc::new(Mutex::new(RemoteState {
                files: Vec::new(),
                #[cfg(feature = "client-to-server")]
                tree: RemoteTree::empty(),
                next_stream_id: 1,
                pending: HashMap::new(),
            })),
            event_sender,
            runtime: Handle::current(),
            #[cfg(feature = "client-to-server")]
            max_entries,
            #[cfg(feature = "client-to-server")]
            mount: Arc::new(Mutex::new(None)),
        }
    }

    /// Records the client's list and returns the entries the Wayland side
    /// advertises as URIs, which are the ones the user selected.
    ///
    /// Split from [`RemoteFiles::advertise`] so the inbound tree can be driven
    /// without a mount.
    #[cfg(feature = "client-to-server")]
    pub(super) fn accept(&self, files: &[FileDescriptor]) -> Vec<String> {
        let Ok(mut state) = self.inner.lock() else {
            return Vec::new();
        };
        state.tree = RemoteTree::build(files, self.max_entries, state.tree.next_base());
        state.files = files.to_vec();
        state
            .tree
            .roots()
            .iter()
            .filter_map(|root| state.tree.node(*root).map(|node| node.name.clone()))
            .collect()
    }

    #[cfg(feature = "client-to-server")]
    pub(super) fn advertise(&self, files: &[FileDescriptor]) -> Option<PendingWrite> {
        let roots = self.accept(files);
        if roots.is_empty() {
            tracing::warn!("Clipboard: the client's file list held nothing that can be pasted");
            return None;
        }

        let path = self.mount()?;
        // Only the selected entries are advertised; a file manager walks into
        // whichever of them are directories through the mount itself.
        let uris: Vec<String> = roots
            .iter()
            .filter_map(|name| Some(url::Url::from_file_path(path.join(name)).ok()?.to_string()))
            .collect();
        if uris.is_empty() {
            return None;
        }
        Some(PendingWrite::Files {
            uri_list: uris
                .iter()
                .map(|uri| format!("{uri}\r\n"))
                .collect::<String>()
                .into_bytes(),
            gnome_copied_files: format!("copy\n{}", uris.join("\n")).into_bytes(),
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

    /// Reads the advertised tree under the state lock. `None` means the lock is
    /// poisoned, which the filesystem answers as an ordinary lookup failure.
    #[cfg(feature = "client-to-server")]
    fn with_tree<T>(&self, read: impl FnOnce(&RemoteTree) -> T) -> Option<T> {
        self.inner.lock().ok().map(|state| read(&state.tree))
    }

    /// Walks a `/`-separated path from the root the way the mount's lookup
    /// does, so a test can assert on the tree a file manager would browse.
    #[cfg(all(test, feature = "client-to-server"))]
    pub(super) fn resolve(&self, path: &str) -> Option<(u64, RemoteNodeKind)> {
        self.with_tree(|tree| {
            let inode = tree.resolve(path)?;
            Some((inode, tree.node(inode)?.kind))
        })
        .flatten()
    }

    #[cfg(all(test, feature = "client-to-server"))]
    pub(super) fn child_names(&self, inode: u64) -> Vec<String> {
        self.with_tree(|tree| {
            tree.children(inode)
                .iter()
                .filter_map(|child| tree.node(*child).map(|node| node.name.clone()))
                .collect()
        })
        .unwrap_or_default()
    }

    #[cfg(feature = "client-to-server")]
    fn attr(&self, inode: fuser::INodeNo) -> Option<fuser::FileAttr> {
        self.with_tree(|tree| {
            tree.node(inode.0)
                .map(|node| node_attr(tree, inode.0, node))
        })
        .flatten()
    }

    #[cfg(feature = "client-to-server")]
    fn kind(&self, inode: fuser::INodeNo) -> Option<RemoteNodeKind> {
        self.with_tree(|tree| tree.node(inode.0).map(|node| node.kind))
            .flatten()
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

fn respond(state: &Arc<Mutex<RemoteState>>, stream_id: u32, result: Result<Vec<u8>, ()>) {
    if let Ok(mut state) = state.lock() {
        if let Some(pending) = state.pending.remove(&stream_id) {
            let _ = pending.answer.send(result);
        }
    }
}

/// The mount's view of the advertised tree.
///
/// Deliberately thin: every handler is a tree lookup, and a read translates an
/// inode into the same index-and-range request the backend seam already
/// exercises without a mount.
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
        match self.files.attr(ino) {
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
        // Every name in the tree arrived as UTF-16 on the wire, so a name that
        // is not valid UTF-8 cannot be in it.
        let found = name.to_str().and_then(|name| {
            self.files
                .with_tree(|tree| {
                    let inode = tree.lookup(parent.0, name)?;
                    Some(node_attr(tree, inode, tree.node(inode)?))
                })
                .flatten()
        });
        match found {
            Some(attr) => reply.entry(&Duration::ZERO, &attr, fuser::Generation(0)),
            None => reply.error(fuser::Errno::ENOENT),
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
        let listing = self
            .files
            .with_tree(|tree| {
                let Some(node) = tree.node(ino.0) else {
                    return Err(fuser::Errno::ENOENT);
                };
                if !node.is_directory() {
                    return Err(fuser::Errno::ENOTDIR);
                }
                let mut entries = vec![
                    (ino.0, fuser::FileType::Directory, ".".to_owned()),
                    (node.parent, fuser::FileType::Directory, "..".to_owned()),
                ];
                entries.extend(tree.children(ino.0).iter().filter_map(|inode| {
                    let child = tree.node(*inode)?;
                    Some((*inode, file_type(child), child.name.clone()))
                }));
                Ok(entries)
            })
            .unwrap_or(Err(fuser::Errno::ENOENT));
        let entries = match listing {
            Ok(entries) => entries,
            Err(errno) => {
                reply.error(errno);
                return;
            }
        };
        for (entry_offset, (inode, kind, name)) in
            entries.into_iter().enumerate().skip(offset as usize)
        {
            if reply.add(fuser::INodeNo(inode), (entry_offset + 1) as u64, kind, name) {
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
        match self.files.kind(ino) {
            // Direct I/O so read sizes reach us unmodified and nothing is
            // cached on top of content fetched one range at a time.
            Some(RemoteNodeKind::File { .. }) => {
                reply.opened(fuser::FileHandle(0), fuser::FopenFlags::FOPEN_DIRECT_IO)
            }
            Some(RemoteNodeKind::Directory) => reply.error(fuser::Errno::EISDIR),
            None => reply.error(fuser::Errno::ENOENT),
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
        let Some(RemoteNodeKind::File { index }) = self.files.kind(ino) else {
            reply.error(fuser::Errno::ENOENT);
            return;
        };
        let receiver = self.files.read(index, offset, size);
        self.files.runtime.spawn(async move {
            match receiver.await {
                Ok(Ok(data)) => reply.data(&data),
                _ => reply.error(fuser::Errno::EIO),
            }
        });
    }
}

#[cfg(feature = "client-to-server")]
fn file_type(node: &RemoteNode) -> fuser::FileType {
    if node.is_directory() {
        fuser::FileType::Directory
    } else {
        fuser::FileType::RegularFile
    }
}

#[cfg(feature = "client-to-server")]
fn node_attr(tree: &RemoteTree, inode: u64, node: &RemoteNode) -> fuser::FileAttr {
    let directory = node.is_directory();
    fuser::FileAttr {
        ino: fuser::INodeNo(inode),
        size: node.size,
        blocks: node.size.div_ceil(512),
        atime: node.modified,
        mtime: node.modified,
        ctime: node.modified,
        crtime: node.modified,
        kind: file_type(node),
        perm: if directory { 0o500 } else { 0o400 },
        nlink: tree.link_count(inode),
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}
