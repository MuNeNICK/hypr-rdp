use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use ironrdp_cliprdr::backend::ClipboardMessage;
#[cfg(test)]
use ironrdp_cliprdr::pdu::MAX_FILE_COUNT;
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, FileContentsFlags, FileContentsRequest, FileContentsResponse,
    FileDescriptor,
};
use ironrdp_server::ServerEvent;
use tokio::sync::mpsc::UnboundedSender;

/// Seconds between the FILETIME epoch (1601) and the Unix epoch.
pub(super) const WINDOWS_EPOCH_OFFSET_SECS: u64 = 11_644_473_600;
pub(super) const MAX_DIRECTORY_DEPTH: usize = 128;
/// The length a `SIZE` request must ask for: the answer is a single 64-bit value.
const SIZE_RESPONSE_BYTES: u32 = 8;

/// How many read requests may wait for the worker at once.
///
/// A client that asks faster than the filesystem answers is refused the excess
/// rather than allowed to grow this queue without limit. Selections do not queue
/// here — they wait in a single latest-wins slot — so this depth is entirely the
/// client's read backlog.
const READ_QUEUE_DEPTH: usize = 64;

/// How long teardown waits for the worker before abandoning it.
///
/// A worker parked in a read on a filesystem that has stopped answering — a dead
/// network mount, or this server's own remote-files mount after the client is
/// gone — must not hold up the end of a session.
const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub(super) struct FrozenFile {
    pub(super) path: PathBuf,
    device: u64,
    inode: u64,
    pub(super) descriptor: FileDescriptor,
}

#[derive(Default)]
pub(super) struct FrozenSelection {
    generation: u64,
    pub(super) entries: Option<Vec<FrozenFile>>,
}

impl From<Option<Vec<FrozenFile>>> for FrozenSelection {
    fn from(entries: Option<Vec<FrozenFile>>) -> Self {
        Self {
            generation: 0,
            entries,
        }
    }
}

pub(super) type FrozenFiles = Arc<Mutex<FrozenSelection>>;

pub(super) fn clear_selection(files: &FrozenFiles) {
    if let Ok(mut current) = files.lock() {
        current.generation = current.generation.wrapping_add(1);
        current.entries = None;
    }
}

enum FileWorkerCommand {
    /// Look at the selection slot and the stop flag. Carries nothing, because
    /// what it announces is state: a wake lost to a full queue costs a turn of
    /// the loop, not the announcement.
    Wake,
    Read(FileContentsRequest),
}

struct PendingSelection {
    paths: Vec<PathBuf>,
    generation: u64,
}

/// A cloneable way to reach one session's file worker.
///
/// Reads queue, and are refused once the queue is full: the caller is the RDP
/// callback, and blocking it stalls video, audio and input, which is the whole
/// reason this worker exists. Selections do not queue at all — the newest one
/// replaces whatever has not been started, because a new selection invalidates
/// the reads queued against the old one.
#[derive(Clone)]
pub(super) struct FileWorkerHandle {
    commands: mpsc::SyncSender<FileWorkerCommand>,
    selection: Arc<Mutex<Option<PendingSelection>>>,
    events: UnboundedSender<ServerEvent>,
}

impl FileWorkerHandle {
    /// Queue a read, or answer it as a protocol failure when the queue is full.
    /// Never blocks.
    fn read(&self, request: FileContentsRequest) {
        let stream_id = request.stream_id;
        if self
            .commands
            .try_send(FileWorkerCommand::Read(request))
            .is_ok()
        {
            return;
        }
        tracing::warn!(
            depth = READ_QUEUE_DEPTH,
            "Clipboard: refusing a file read; the worker is behind"
        );
        let _ = self.events.send(ServerEvent::Clipboard(
            ClipboardMessage::SendFileContentsResponse(FileContentsResponse::new_error(stream_id)),
        ));
    }

    /// Hand the worker a selection to freeze, replacing one it has not started.
    /// Returns `false` only when the worker is gone.
    pub(super) fn freeze(&self, paths: Vec<PathBuf>, generation: u64) -> bool {
        let Ok(mut slot) = self.selection.lock() else {
            return false;
        };
        *slot = Some(PendingSelection { paths, generation });
        drop(slot);
        match self.commands.try_send(FileWorkerCommand::Wake) {
            // A full queue means the worker is mid-command and will look at the
            // slot on its next turn, so a lost wake delays the announcement
            // rather than losing it.
            Ok(()) | Err(mpsc::TrySendError::Full(_)) => true,
            Err(mpsc::TrySendError::Disconnected(_)) => {
                tracing::warn!("Clipboard: file worker is gone; offering the selection as text");
                false
            }
        }
    }
}

fn take_selection(slot: &Mutex<Option<PendingSelection>>) -> Option<PendingSelection> {
    slot.lock().ok()?.take()
}

/// One session's file worker: the thread that walks the filesystem and reads
/// from it, so that neither happens on the Wayland event thread or inside an
/// RDP callback.
pub(super) struct FileWorker {
    handle: FileWorkerHandle,
    stopping: Arc<AtomicBool>,
    finished: mpsc::Receiver<()>,
    stop_timeout: Duration,
}

impl FileWorker {
    pub(super) fn start(
        files: FrozenFiles,
        event_sender: UnboundedSender<ServerEvent>,
        max_chunk_bytes: u32,
        max_entries: usize,
    ) -> Self {
        Self::start_with(
            files,
            event_sender,
            max_entries,
            WORKER_STOP_TIMEOUT,
            move |files, request| read_file_contents(files, request, max_chunk_bytes),
        )
    }

    /// [`start`](Self::start) with the read step and the teardown bound
    /// supplied, which is how a test drives a read that never returns without
    /// waiting the production timeout for it.
    fn start_with(
        files: FrozenFiles,
        event_sender: UnboundedSender<ServerEvent>,
        max_entries: usize,
        stop_timeout: Duration,
        read: impl Fn(&FrozenFiles, FileContentsRequest) -> FileContentsResponse<'static>
            + Send
            + 'static,
    ) -> Self {
        let (commands, receiver) = mpsc::sync_channel(READ_QUEUE_DEPTH);
        let (finished_sender, finished) = mpsc::channel();
        let handle = FileWorkerHandle {
            commands,
            selection: Arc::default(),
            events: event_sender.clone(),
        };
        let stopping = Arc::new(AtomicBool::new(false));
        // The thread holds the slot, not a handle: a handle of its own would
        // keep the command channel alive and hide the worker's own shutdown.
        let selection = Arc::clone(&handle.selection);
        let flag = Arc::clone(&stopping);
        thread::Builder::new()
            .name("clipboard-file-worker".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    // A newer selection invalidates the reads queued behind it,
                    // so it is taken before them.
                    if let Some(pending) = take_selection(&selection) {
                        let frozen = freeze_paths(pending.paths, max_entries);
                        publish_selection(&files, pending.generation, frozen, &event_sender);
                        continue;
                    }
                    match receiver.recv() {
                        Ok(FileWorkerCommand::Wake) => continue,
                        Ok(FileWorkerCommand::Read(request)) => {
                            let response = read(&files, request);
                            let _ = event_sender.send(ServerEvent::Clipboard(
                                ClipboardMessage::SendFileContentsResponse(response),
                            ));
                        }
                        Err(_) => break,
                    }
                }
                let _ = finished_sender.send(());
            })
            .expect("spawn clipboard file worker");
        Self {
            handle,
            stopping,
            finished,
            stop_timeout,
        }
    }

    pub(super) fn read(&self, request: FileContentsRequest) {
        self.handle.read(request);
    }

    pub(super) fn handle(&self) -> FileWorkerHandle {
        self.handle.clone()
    }
}

fn publish_selection(
    files: &FrozenFiles,
    generation: u64,
    frozen: Vec<FrozenFile>,
    event_sender: &UnboundedSender<ServerEvent>,
) {
    if let Ok(mut current) = files.lock() {
        if current.generation != generation {
            return;
        }
        // Serialize publication with invalidation, including the event enqueue.
        current.entries = (!frozen.is_empty()).then_some(frozen.clone());
        if !frozen.is_empty() {
            let descriptors = frozen.into_iter().map(|file| file.descriptor).collect();
            let _ = event_sender.send(ServerEvent::Clipboard(
                ClipboardMessage::SendInitiateFileCopy(descriptors),
            ));
        }
    }
}

impl Drop for FileWorker {
    fn drop(&mut self) {
        // A flag rather than a queued command: a stop message would sit behind
        // every read already queued, so teardown would first serve the backlog.
        self.stopping.store(true, Ordering::Relaxed);
        // Wake a worker parked on an empty queue. Best effort: a full queue
        // means it is mid-command and will see the flag on its next turn.
        let _ = self.handle.commands.try_send(FileWorkerCommand::Wake);
        if let Err(mpsc::RecvTimeoutError::Timeout) = self.finished.recv_timeout(self.stop_timeout)
        {
            // Abandoning keeps the frozen entries alive until the read returns,
            // which costs one session's worth of memory on a path only a wedged
            // filesystem reaches. Waiting instead costs the session's teardown.
            tracing::warn!("Clipboard: abandoning the file worker; a read has not returned");
        }
    }
}

/// The Wayland watcher's handle on the file selection offered to the client.
///
/// Holds the frozen list the RDP backend serves reads from, and the worker that
/// rebuilds that list off the Wayland event thread. A `None` worker means
/// server-to-client file transfer is off.
pub(super) struct FileSelection {
    frozen: FrozenFiles,
    worker: Option<FileWorkerHandle>,
}

impl FileSelection {
    pub(super) fn new(frozen: FrozenFiles, worker: Option<FileWorkerHandle>) -> Self {
        Self { frozen, worker }
    }

    /// Forget the frozen selection so the client can no longer read it.
    pub(super) fn clear(&self) {
        clear_selection(&self.frozen);
    }

    /// Hand a new selection to the file worker, which freezes it off this
    /// thread and advertises it. Returns `false` when there is nothing to
    /// freeze or file transfer is off, so the caller can fall back to offering
    /// the selection as text.
    pub(super) fn freeze(&self, paths: Vec<PathBuf>) -> bool {
        let Some(worker) = self.worker.as_ref().filter(|_| !paths.is_empty()) else {
            return false;
        };
        let Ok(mut current) = self.frozen.lock() else {
            return false;
        };
        current.generation = current.generation.wrapping_add(1);
        current.entries = None;
        let generation = current.generation;
        drop(current);
        worker.freeze(paths, generation)
    }
}

#[cfg(test)]
pub(super) fn freeze_regular_files(paths: Vec<PathBuf>) -> Vec<FrozenFile> {
    freeze_paths(paths, MAX_FILE_COUNT)
}

pub(super) fn freeze_paths(paths: Vec<PathBuf>, max_entries: usize) -> Vec<FrozenFile> {
    let mut walk = Walk::new(max_entries);
    let mut paths = paths.into_iter();
    for path in paths.by_ref() {
        if walk.remaining == 0 {
            walk.truncated = true;
            break;
        }
        walk.remaining -= 1;
        walk.freeze_path(&path, &[], 0);
        if walk.is_full() {
            break;
        }
    }
    // Anything left in the iterator is a selection entry the ceiling cost us.
    if paths.next().is_some() {
        walk.truncated = true;
    }
    if walk.truncated {
        tracing::warn!(
            max_entries,
            "Clipboard: file selection truncated at entry limit"
        );
    }
    walk.files
}

/// One recursive enumeration of a clipboard selection.
///
/// Carries the state the walk threads through every level: the entry ceiling,
/// the directories on the current ancestor chain (which bounds symlink cycles), the names
/// already handed out per directory, and whether the ceiling actually cost us
/// an entry.
struct Walk {
    max_entries: usize,
    remaining: usize,
    ancestor_directories: HashSet<(u64, u64)>,
    used_names: HashMap<Vec<String>, HashSet<String>>,
    files: Vec<FrozenFile>,
    truncated: bool,
}

impl Walk {
    fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            remaining: max_entries,
            ancestor_directories: HashSet::new(),
            used_names: HashMap::new(),
            files: Vec::new(),
            truncated: false,
        }
    }

    fn is_full(&self) -> bool {
        self.files.len() >= self.max_entries
    }

    fn freeze_path(&mut self, path: &Path, parent: &[String], depth: usize) {
        if self.is_full() {
            self.truncated = true;
            return;
        }
        if depth > MAX_DIRECTORY_DEPTH {
            tracing::warn!(
                ?path,
                max_depth = MAX_DIRECTORY_DEPTH,
                "Clipboard: skipping directory beyond recursion limit"
            );
            return;
        }

        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::warn!(?path, %error, "Clipboard: cannot stat selected entry");
                return;
            }
        };
        if metadata.is_file() {
            let name = self.unique_name(parent, path);
            self.files.push(frozen_file(
                path.to_path_buf(),
                parent,
                name,
                metadata,
                ClipboardFileAttributes::NORMAL,
            ));
            return;
        }
        if !metadata.is_dir() {
            tracing::warn!(?path, "Clipboard: skipping non-regular clipboard entry");
            return;
        }

        if !self
            .ancestor_directories
            .insert((metadata.dev(), metadata.ino()))
        {
            tracing::warn!(?path, "Clipboard: skipping directory symlink cycle");
            return;
        }

        let identity = (metadata.dev(), metadata.ino());
        let name = self.unique_name(parent, path);
        self.files.push(frozen_file(
            path.to_path_buf(),
            parent,
            name.clone(),
            metadata,
            ClipboardFileAttributes::DIRECTORY,
        ));

        self.freeze_children(path, parent, name, depth);
        self.ancestor_directories.remove(&identity);
    }

    fn freeze_children(&mut self, path: &Path, parent: &[String], name: String, depth: usize) {
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(?path, %error, "Clipboard: cannot enumerate directory");
                return;
            }
        };
        let mut children =
            self.collect_children(path, entries.map(|entry| entry.map(|entry| entry.path())));
        children.sort();
        let mut relative_path = parent.to_vec();
        relative_path.push(name);

        // Directories first, so every entry arrives after the directory holding it.
        let (directories, leaves): (Vec<_>, Vec<_>) = children
            .into_iter()
            .partition(|child| std::fs::metadata(child).is_ok_and(|child| child.is_dir()));
        for child in directories.iter().chain(leaves.iter()) {
            if self.is_full() {
                self.truncated = true;
                return;
            }
            self.freeze_path(child, &relative_path, depth + 1);
        }
    }

    fn collect_children(
        &mut self,
        path: &Path,
        entries: impl Iterator<Item = std::io::Result<PathBuf>>,
    ) -> Vec<PathBuf> {
        let mut children = Vec::new();
        for entry in entries {
            if self.remaining == 0 {
                self.truncated = true;
                break;
            }
            // Bound inspected entries, including skipped/error entries, not
            // just the descriptors eventually emitted. Reserve each slot once.
            self.remaining -= 1;
            match entry {
                Ok(entry) => children.push(entry),
                Err(error) => {
                    tracing::warn!(?path, %error, "Clipboard: cannot read directory entry");
                }
            }
        }
        children
    }

    fn unique_name(&mut self, parent: &[String], path: &Path) -> String {
        unique_file_name(parent, path, &mut self.used_names)
    }
}

fn unique_file_name(
    parent: &[String],
    path: &Path,
    used_names: &mut HashMap<Vec<String>, HashSet<String>>,
) -> String {
    let raw_name = path
        .file_name()
        .map(|name| String::from_utf8_lossy(name.as_encoded_bytes()).into_owned())
        .unwrap_or_else(|| "unnamed".into());
    let sanitized = sanitize_file_name(&raw_name);
    let used = used_names.entry(parent.to_vec()).or_default();
    let mut candidate = sanitized.clone();
    let (stem, extension) = split_extension(&sanitized);
    let mut suffix = 2;
    while !used.insert(windows_name_key(&candidate)) {
        candidate = format!("{stem} ({suffix}){extension}");
        suffix += 1;
    }
    candidate
}

fn sanitize_file_name(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .map(|character| {
            if matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            ) || character <= '\u{1f}'
            {
                '_'
            } else {
                character
            }
        })
        .collect();
    sanitized = sanitized.trim_end_matches(['.', ' ']).to_owned();
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        sanitized = "unnamed".into();
    }

    let (stem, extension) = split_extension(&sanitized);
    if is_windows_device_name(stem) {
        sanitized = format!("{stem}_{extension}");
    }
    sanitized
}

fn split_extension(name: &str) -> (&str, &str) {
    let Some(index) = name.rfind('.') else {
        return (name, "");
    };
    if index == 0 {
        (name, "")
    } else {
        name.split_at(index)
    }
}

fn is_windows_device_name(stem: &str) -> bool {
    let name = stem.to_ascii_uppercase();
    matches!(name.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || name
            .strip_prefix("COM")
            .or_else(|| name.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

fn windows_name_key(name: &str) -> String {
    name.to_uppercase()
}

fn frozen_file(
    path: PathBuf,
    parent: &[String],
    name: String,
    metadata: std::fs::Metadata,
    attributes: ClipboardFileAttributes,
) -> FrozenFile {
    let mut descriptor = FileDescriptor::new(name)
        .with_attributes(attributes)
        .with_last_write_time(filetime(metadata.modified().ok()));
    if !metadata.is_dir() {
        descriptor = descriptor.with_file_size(metadata.len());
    }
    if !parent.is_empty() {
        descriptor = descriptor.with_relative_path(parent.join("\\"));
    }
    FrozenFile {
        path,
        device: metadata.dev(),
        inode: metadata.ino(),
        descriptor,
    }
}

fn filetime(modified: Option<SystemTime>) -> u64 {
    modified
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs().saturating_add(WINDOWS_EPOCH_OFFSET_SECS)) * 10_000_000)
        .unwrap_or_default()
}

/// The inverse of [`filetime`], for the descriptors a client sends us.
/// Only the inbound direction reads times off the wire.
/// `None` for the zero value the protocol uses to mean "not carried", and for
/// any stamp older than the Unix epoch.
#[cfg(feature = "client-to-server")]
pub(super) fn system_time(filetime: u64) -> Option<SystemTime> {
    let seconds = (filetime / 10_000_000).checked_sub(WINDOWS_EPOCH_OFFSET_SECS)?;
    Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds))
}

fn read_file_contents(
    files: &FrozenFiles,
    request: FileContentsRequest,
    max_chunk_bytes: u32,
) -> FileContentsResponse<'static> {
    read_file_contents_with_open(files, request, max_chunk_bytes, open_source)
}

fn open_source(path: &Path) -> std::io::Result<File> {
    // A path replaced by a FIFO must not block open. Symlinks remain supported.
    File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

/// The single operation a well-formed file-contents request describes.
enum RequestedOperation {
    Size,
    Range { position: u64, length: u32 },
}

/// The one operation a request asks for, or `None` if it asks for no operation
/// the protocol allows.
///
/// MS-RDPECLIP lets a request describe exactly one operation: `SIZE` and `RANGE`
/// are mutually exclusive, and a `SIZE` request must ask for eight bytes at
/// position zero, because the answer is a single 64-bit value. Anything else is
/// refused rather than interpreted, which is why this is a lookup and not a pair
/// of flag tests at the point of use.
///
/// Bits outside those two are reserved, and the protocol library preserves them
/// through decoding rather than masking them off. An unknown bit is therefore
/// logged and ignored: refusing one would refuse a client that sets a bit this
/// implementation has not seen, and a refused request costs the user a paste.
fn requested_operation(request: &FileContentsRequest) -> Option<RequestedOperation> {
    let reserved = request
        .flags
        .difference(FileContentsFlags::SIZE | FileContentsFlags::RANGE);
    if !reserved.is_empty() {
        tracing::debug!(
            flags = format!("{:#x}", request.flags.bits()),
            "Clipboard: ignoring reserved flags on a file-contents request"
        );
    }
    let operation = match (
        request.flags.contains(FileContentsFlags::SIZE),
        request.flags.contains(FileContentsFlags::RANGE),
    ) {
        (true, false) if request.position == 0 && request.requested_size == SIZE_RESPONSE_BYTES => {
            Some(RequestedOperation::Size)
        }
        (false, true) => Some(RequestedOperation::Range {
            position: request.position,
            length: request.requested_size,
        }),
        _ => None,
    };
    if operation.is_none() {
        tracing::warn!(
            flags = format!("{:#x}", request.flags.bits()),
            position = request.position,
            requested_size = request.requested_size,
            "Clipboard: refusing a file-contents request the protocol does not allow"
        );
    }
    operation
}

fn read_file_contents_with_open(
    files: &FrozenFiles,
    request: FileContentsRequest,
    max_chunk_bytes: u32,
    open: impl FnOnce(&Path) -> std::io::Result<File>,
) -> FileContentsResponse<'static> {
    let error = || FileContentsResponse::new_error(request.stream_id);
    // Refuse a malformed request before it reaches the filesystem.
    let Some(operation) = requested_operation(&request) else {
        return error();
    };
    let Some(file) = files.lock().ok().and_then(|files| {
        files
            .entries
            .as_ref()
            .and_then(|files| files.get(request.index as usize).cloned())
    }) else {
        return error();
    };
    // Validate the opened object, not a separate lookup of a mutable path.
    let Ok(mut source) = open(&file.path) else {
        return error();
    };
    let Ok(metadata) = source.metadata() else {
        return error();
    };
    if metadata.dev() != file.device || metadata.ino() != file.inode || !metadata.is_file() {
        return error();
    }
    match operation {
        RequestedOperation::Size => {
            FileContentsResponse::new_size_response(request.stream_id, metadata.len())
        }
        RequestedOperation::Range { position, length } => {
            // The configured ceiling, not a protocol rule: it is what keeps the
            // client from choosing how much this server allocates.
            if length > max_chunk_bytes {
                return error();
            }
            let mut data = vec![0; length as usize];
            if source.seek(SeekFrom::Start(position)).is_err() {
                return error();
            }
            match source.read(&mut data) {
                Ok(count) => {
                    data.truncate(count);
                    FileContentsResponse::new_data_response(request.stream_id, data)
                }
                Err(_) => error(),
            }
        }
    }
}

pub(super) fn uri_list_paths(data: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(data)
        .lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            (!line.is_empty() && !line.starts_with('#'))
                .then_some(line)
                .and_then(|line| url::Url::parse(line).ok())
                .filter(|url| url.scheme() == "file")
                .and_then(|url| url.to_file_path().ok())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use std::io::Write;

    #[test]
    fn a_path_swapped_during_open_never_serves_the_replacement() {
        let root = std::env::temp_dir().join(format!("hypr-rdp-open-race-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("selected");
        std::fs::write(&path, b"selected bytes").unwrap();
        let files: FrozenFiles = Arc::new(Mutex::new(
            Some(freeze_regular_files(vec![path.clone()])).into(),
        ));
        for flags in [FileContentsFlags::RANGE, FileContentsFlags::SIZE] {
            let response =
                read_file_contents_with_open(&files, request(0, flags, 0, 8), 64, |path| {
                    // Preserve the original inode so allocator reuse cannot mask the swap.
                    std::fs::rename(path, root.join("original")).unwrap();
                    std::fs::write(path, b"unselected secret").unwrap();
                    open_source(path)
                });
            assert!(response.is_error());
            std::fs::rename(root.join("original"), &path).unwrap();
        }
        // Swapping to a FIFO must also return, without waiting for a writer.
        std::fs::rename(&path, root.join("original")).unwrap();
        let fifo = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(
            read_file_contents(&files, request(0, FileContentsFlags::RANGE, 0, 8), 64).is_error()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_aliases_are_copied_but_ancestor_cycles_are_skipped() {
        let root = std::env::temp_dir().join(format!("hypr-rdp-aliases-{}", std::process::id()));
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/file"), b"contents").unwrap();
        std::os::unix::fs::symlink("real", root.join("alias")).unwrap();
        std::os::unix::fs::symlink("..", root.join("real/cycle")).unwrap();
        let frozen = freeze_paths(vec![root.clone()], 100);
        let paths: Vec<_> = frozen
            .iter()
            .map(|file| file.path.strip_prefix(&root).unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                Path::new(""),
                Path::new("alias"),
                Path::new("alias/file"),
                Path::new("real"),
                Path::new("real/file")
            ]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_inspection_stops_at_the_budget_even_for_errors() {
        for errors in [false, true] {
            let mut inspected = 0;
            let entries = std::iter::repeat_with(|| {
                inspected += 1;
                assert!(
                    inspected <= 4,
                    "enumerated past the budget and overflow probe"
                );
                if errors {
                    Err(std::io::Error::other("unreadable entry"))
                } else {
                    Ok(PathBuf::from("child"))
                }
            });
            let mut walk = Walk::new(3);
            let children = walk.collect_children(Path::new("parent"), entries);
            assert_eq!(children.len(), if errors { 0 } else { 3 });
            assert_eq!(inspected, 4);
            assert_eq!(walk.remaining, 0);
            assert!(walk.truncated);
        }
    }

    #[test]
    fn a_completed_walk_cannot_publish_after_clear_or_a_new_freeze() {
        let path =
            std::env::temp_dir().join(format!("hypr-rdp-stale-freeze-{}", std::process::id()));
        std::fs::write(&path, b"old selection").unwrap();
        for newer_freeze in [false, true] {
            let files: FrozenFiles = Arc::default();
            let (handle, commands, _) = test_handle();
            let selection = FileSelection::new(files.clone(), Some(handle.clone()));
            assert!(selection.freeze(vec![path.clone()]));
            assert!(matches!(commands.recv().unwrap(), FileWorkerCommand::Wake));
            let pending = take_selection(&handle.selection).expect("the selection to freeze");
            let generation = pending.generation;
            let result = freeze_paths(pending.paths, 100);
            if newer_freeze {
                assert!(selection.freeze(vec![path.clone()]));
            } else {
                selection.clear();
            }
            let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            publish_selection(&files, generation, result, &events);
            assert!(files.lock().unwrap().entries.is_none());
            assert!(
                receiver.try_recv().is_err(),
                "stale offer reached the client"
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    /// A handle whose worker is the test itself: the commands it queues and the
    /// selections it parks are inspected rather than served.
    fn test_handle() -> (
        FileWorkerHandle,
        mpsc::Receiver<FileWorkerCommand>,
        tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    ) {
        let (commands, receiver) = mpsc::sync_channel(READ_QUEUE_DEPTH);
        let (events, event_receiver) = tokio::sync::mpsc::unbounded_channel();
        (
            FileWorkerHandle {
                commands,
                selection: Arc::default(),
                events,
            },
            receiver,
            event_receiver,
        )
    }

    fn range_request() -> FileContentsRequest {
        request(0, FileContentsFlags::RANGE, 0, 8)
    }

    #[test]
    fn a_read_arriving_at_a_full_queue_is_refused_rather_than_waited_on() {
        let (handle, commands, mut events) = test_handle();
        // Nothing drains the queue, so it fills to the depth it was built with.
        for _ in 0..READ_QUEUE_DEPTH {
            handle.read(range_request());
        }
        assert!(
            events.try_recv().is_err(),
            "a queued read was answered before the worker ran"
        );

        handle.read(range_request());

        let Ok(ServerEvent::Clipboard(ClipboardMessage::SendFileContentsResponse(response))) =
            events.try_recv()
        else {
            panic!("expected the refused read to be answered");
        };
        assert!(response.is_error());
        assert_eq!(commands.try_iter().count(), READ_QUEUE_DEPTH);
    }

    #[test]
    fn teardown_does_not_wait_for_a_read_that_never_returns() {
        let stop_timeout = Duration::from_millis(200);
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let (started, read_started) = mpsc::channel();
        // Held for the life of the test, so the read the worker starts never
        // completes on its own.
        let (_never, blocked) = mpsc::channel::<()>();
        let blocked = Mutex::new(blocked);
        let worker = FileWorker::start_with(
            Arc::default(),
            events,
            100,
            stop_timeout,
            move |_, request| {
                let _ = started.send(());
                let _ = blocked.lock().expect("the blocked read").recv();
                FileContentsResponse::new_error(request.stream_id)
            },
        );

        worker.read(range_request());
        read_started
            .recv_timeout(Duration::from_secs(5))
            .expect("the worker to start the read");
        // A backlog teardown must not serve before it stops.
        for _ in 0..8 {
            worker.read(range_request());
        }

        let teardown = Instant::now();
        drop(worker);
        let elapsed = teardown.elapsed();

        assert!(
            elapsed >= stop_timeout,
            "teardown returned before the bound, so it did not wait for the worker at all"
        );
        assert!(
            elapsed < stop_timeout * 10,
            "teardown waited on a read that never returns: {elapsed:?}"
        );
    }

    fn request(
        index: i32,
        flags: FileContentsFlags,
        position: u64,
        requested_size: u32,
    ) -> FileContentsRequest {
        FileContentsRequest {
            stream_id: 7,
            index,
            flags,
            position,
            requested_size,
            data_id: None,
        }
    }

    #[test]
    fn serves_a_frozen_regular_file_by_size_and_range() {
        let path = std::env::temp_dir().join(format!("hypr-rdp-file-test-{}", std::process::id()));
        File::create(&path)
            .unwrap()
            .write_all(b"clipboard bytes")
            .unwrap();
        let files: FrozenFiles = Arc::new(Mutex::new(
            Some(freeze_regular_files(vec![path.clone()])).into(),
        ));
        let size = read_file_contents(&files, request(0, FileContentsFlags::SIZE, 0, 8), 64);
        assert_eq!(size.data_as_size().unwrap(), 15);
        let range = read_file_contents(&files, request(0, FileContentsFlags::RANGE, 10, 64), 64);
        assert_eq!(range.stream_id(), 7);
        assert_eq!(range.data(), b"bytes");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn refuses_a_request_that_does_not_describe_one_operation() {
        let path =
            std::env::temp_dir().join(format!("hypr-rdp-file-flags-test-{}", std::process::id()));
        File::create(&path)
            .unwrap()
            .write_all(b"clipboard bytes")
            .unwrap();
        let files: FrozenFiles = Arc::new(Mutex::new(
            Some(freeze_regular_files(vec![path.clone()])).into(),
        ));
        // Both operations at once, and neither of them, describe no operation.
        let both = FileContentsFlags::SIZE | FileContentsFlags::RANGE;
        assert!(read_file_contents(&files, request(0, both, 0, 8), 64).is_error());
        assert!(
            read_file_contents(&files, request(0, FileContentsFlags::empty(), 0, 8), 64).is_error()
        );
        // A size request answers one 64-bit value, so any other position or
        // length is not a size request.
        assert!(
            read_file_contents(&files, request(0, FileContentsFlags::SIZE, 1, 8), 64).is_error()
        );
        assert!(
            read_file_contents(&files, request(0, FileContentsFlags::SIZE, 0, 4), 64).is_error()
        );
        // A reserved bit alongside one operation is ignored, not refused.
        let reserved =
            FileContentsFlags::from_bits_retain(FileContentsFlags::SIZE.bits() | 0x0000_0004);
        assert_eq!(
            read_file_contents(&files, request(0, reserved, 0, 8), 64)
                .data_as_size()
                .unwrap(),
            15
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_out_of_range_changed_and_oversized_requests() {
        let path =
            std::env::temp_dir().join(format!("hypr-rdp-file-reject-test-{}", std::process::id()));
        File::create(&path).unwrap().write_all(b"first").unwrap();
        let files: FrozenFiles = Arc::new(Mutex::new(
            Some(freeze_regular_files(vec![path.clone()])).into(),
        ));
        assert!(
            read_file_contents(&files, request(1, FileContentsFlags::SIZE, 0, 8), 4).is_error()
        );
        assert!(
            read_file_contents(&files, request(0, FileContentsFlags::RANGE, 0, 5), 4).is_error()
        );
        std::fs::remove_file(&path).unwrap();
        File::create(&path).unwrap().write_all(b"second").unwrap();
        assert!(
            read_file_contents(&files, request(0, FileContentsFlags::SIZE, 0, 8), 4).is_error()
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn freezes_a_selection_only_when_transfer_is_on_and_paths_remain() {
        let frozen: FrozenFiles = Arc::new(Mutex::new(Some(Vec::new()).into()));
        let (handle, commands, _) = test_handle();
        let selection = FileSelection::new(Arc::clone(&frozen), Some(handle.clone()));

        selection.clear();
        assert!(frozen.lock().unwrap().entries.is_none());
        assert!(!selection.freeze(Vec::new()));
        assert!(selection.freeze(vec![PathBuf::from("/nonexistent")]));
        assert!(matches!(commands.try_recv(), Ok(FileWorkerCommand::Wake)));
        assert_eq!(
            take_selection(&handle.selection).expect("the selection to freeze").paths,
            [PathBuf::from("/nonexistent")]
        );

        let disabled = FileSelection::new(Arc::clone(&frozen), None);
        assert!(!disabled.freeze(vec![PathBuf::from("/nonexistent")]));
    }
}
