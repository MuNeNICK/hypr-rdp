use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::SystemTime;

use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, FileContentsFlags, FileContentsRequest, FileContentsResponse,
    FileDescriptor,
};
#[cfg(test)]
use ironrdp_cliprdr::pdu::MAX_FILE_COUNT;
use ironrdp_server::ServerEvent;
use tokio::sync::mpsc::UnboundedSender;

const WINDOWS_EPOCH_OFFSET_SECS: u64 = 11_644_473_600;
const MAX_DIRECTORY_DEPTH: usize = 128;

#[derive(Clone, Debug)]
pub(super) struct FrozenFile {
    pub(super) path: PathBuf,
    device: u64,
    inode: u64,
    pub(super) descriptor: FileDescriptor,
}

pub(super) type FrozenFiles = Arc<Mutex<Option<Vec<FrozenFile>>>>;

pub(super) enum FileWorkerCommand {
    Freeze(Vec<PathBuf>),
    Read(FileContentsRequest),
    Stop,
}

pub(super) struct FileWorker {
    sender: mpsc::Sender<FileWorkerCommand>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FileWorker {
    pub(super) fn start(
        files: FrozenFiles,
        event_sender: UnboundedSender<ServerEvent>,
        max_chunk_bytes: u32,
        max_entries: usize,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("clipboard-file-worker".into())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    match command {
                        FileWorkerCommand::Freeze(paths) => {
                            let frozen = freeze_paths(paths, max_entries);
                            if let Ok(mut current) = files.lock() {
                                *current = (!frozen.is_empty()).then_some(frozen.clone());
                            }
                            if !frozen.is_empty() {
                                let descriptors =
                                    frozen.into_iter().map(|file| file.descriptor).collect();
                                let _ = event_sender.send(ServerEvent::Clipboard(
                                    ClipboardMessage::SendInitiateFileCopy(descriptors),
                                ));
                            }
                        }
                        FileWorkerCommand::Read(request) => {
                            let response = read_file_contents(&files, request, max_chunk_bytes);
                            let _ = event_sender.send(ServerEvent::Clipboard(
                                ClipboardMessage::SendFileContentsResponse(response),
                            ));
                        }
                        FileWorkerCommand::Stop => break,
                    }
                }
            })
            .expect("spawn clipboard file worker");
        Self {
            sender,
            handle: Some(handle),
        }
    }

    pub(super) fn send(&self, command: FileWorkerCommand) {
        let _ = self.sender.send(command);
    }

    pub(super) fn sender(&self) -> mpsc::Sender<FileWorkerCommand> {
        self.sender.clone()
    }
}

impl Drop for FileWorker {
    fn drop(&mut self) {
        self.send(FileWorkerCommand::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
pub(super) fn freeze_regular_files(paths: Vec<PathBuf>) -> Vec<FrozenFile> {
    freeze_paths(paths, MAX_FILE_COUNT)
}

pub(super) fn freeze_paths(paths: Vec<PathBuf>, max_entries: usize) -> Vec<FrozenFile> {
    let mut files = Vec::new();
    let mut visited_directories = HashSet::new();
    let mut used_names = HashMap::new();

    for path in paths {
        freeze_path(
            &path,
            &[],
            0,
            max_entries,
            &mut visited_directories,
            &mut used_names,
            &mut files,
        );
        if files.len() == max_entries {
            tracing::warn!(
                max_entries,
                "Clipboard: file selection truncated at entry limit"
            );
            break;
        }
    }

    files
}

fn freeze_path(
    path: &PathBuf,
    parent: &[String],
    depth: usize,
    max_entries: usize,
    visited_directories: &mut HashSet<(u64, u64)>,
    used_names: &mut HashMap<Vec<String>, HashSet<String>>,
    files: &mut Vec<FrozenFile>,
) {
    if files.len() == max_entries {
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
        let name = unique_file_name(parent, path, used_names);
        files.push(frozen_file(
            path.clone(),
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

    if !visited_directories.insert((metadata.dev(), metadata.ino())) {
        tracing::warn!(?path, "Clipboard: skipping directory symlink cycle");
        return;
    }

    let name = unique_file_name(parent, path, used_names);

    files.push(frozen_file(
        path.clone(),
        parent,
        name.clone(),
        metadata,
        ClipboardFileAttributes::DIRECTORY,
    ));

    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(?path, %error, "Clipboard: cannot enumerate directory");
            return;
        }
    };
    let mut children: Vec<_> = entries
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry.path()),
            Err(error) => {
                tracing::warn!(?path, %error, "Clipboard: cannot read directory entry");
                None
            }
        })
        .collect();
    children.sort();
    let mut relative_path = parent.to_vec();
    relative_path.push(name);

    for child in children
        .iter()
        .filter(|child| std::fs::metadata(child).is_ok_and(|metadata| metadata.is_dir()))
    {
        freeze_path(
            child,
            &relative_path,
            depth + 1,
            max_entries,
            visited_directories,
            used_names,
            files,
        );
        if files.len() == max_entries {
            return;
        }
    }
    for child in children
        .iter()
        .filter(|child| std::fs::metadata(child).is_ok_and(|metadata| !metadata.is_dir()))
    {
        freeze_path(
            child,
            &relative_path,
            depth + 1,
            max_entries,
            visited_directories,
            used_names,
            files,
        );
        if files.len() == max_entries {
            return;
        }
    }
}

fn unique_file_name(
    parent: &[String],
    path: &PathBuf,
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
        .with_last_write_time(filetime(metadata.modified().ok()))
        .with_file_size(metadata.len());
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

fn read_file_contents(
    files: &FrozenFiles,
    request: FileContentsRequest,
    max_chunk_bytes: u32,
) -> FileContentsResponse<'static> {
    let error = || FileContentsResponse::new_error(request.stream_id);
    let Some(file) = files.lock().ok().and_then(|files| {
        files
            .as_ref()
            .and_then(|files| files.get(request.index as usize).cloned())
    }) else {
        return error();
    };
    let Ok(metadata) = std::fs::metadata(&file.path) else {
        return error();
    };
    if metadata.dev() != file.device || metadata.ino() != file.inode || !metadata.is_file() {
        return error();
    }
    if request.flags.contains(FileContentsFlags::SIZE) {
        return FileContentsResponse::new_size_response(request.stream_id, metadata.len());
    }
    if !request.flags.contains(FileContentsFlags::RANGE) || request.requested_size > max_chunk_bytes
    {
        return error();
    }
    let mut data = vec![0; request.requested_size as usize];
    let Ok(mut source) = File::open(&file.path) else {
        return error();
    };
    if source.seek(SeekFrom::Start(request.position)).is_err() {
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
    use super::*;
    use std::io::Write;

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
        let files: FrozenFiles =
            Arc::new(Mutex::new(Some(freeze_regular_files(vec![path.clone()]))));
        let size = read_file_contents(&files, request(0, FileContentsFlags::SIZE, 0, 8), 64);
        assert_eq!(size.data_as_size().unwrap(), 15);
        let range = read_file_contents(&files, request(0, FileContentsFlags::RANGE, 10, 64), 64);
        assert_eq!(range.stream_id(), 7);
        assert_eq!(range.data(), b"bytes");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_out_of_range_changed_and_oversized_requests() {
        let path =
            std::env::temp_dir().join(format!("hypr-rdp-file-reject-test-{}", std::process::id()));
        File::create(&path).unwrap().write_all(b"first").unwrap();
        let files: FrozenFiles =
            Arc::new(Mutex::new(Some(freeze_regular_files(vec![path.clone()]))));
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
}
