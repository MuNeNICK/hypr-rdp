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
use ironrdp_server::ServerEvent;
use tokio::sync::mpsc::UnboundedSender;

const WINDOWS_EPOCH_OFFSET_SECS: u64 = 11_644_473_600;

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
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("clipboard-file-worker".into())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    match command {
                        FileWorkerCommand::Freeze(paths) => {
                            let frozen = freeze_regular_files(paths);
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

pub(super) fn freeze_regular_files(paths: Vec<PathBuf>) -> Vec<FrozenFile> {
    paths.into_iter().filter_map(|path| {
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => { tracing::debug!(?path, "Clipboard: skipping non-regular file"); return None; }
            Err(error) => { tracing::warn!(?path, %error, "Clipboard: cannot stat selected file"); return None; }
        };
        let name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name.to_owned(),
            None => { tracing::warn!(?path, "Clipboard: skipping non-Unicode filename until name translation is enabled"); return None; }
        };
        Some(FrozenFile {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
            descriptor: FileDescriptor::new(name)
                .with_attributes(ClipboardFileAttributes::NORMAL)
                .with_last_write_time(filetime(metadata.modified().ok()))
                .with_file_size(metadata.len()),
        })
    }).collect()
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
