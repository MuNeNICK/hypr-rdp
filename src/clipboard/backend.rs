use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags,
    FileContentsRequest, FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
#[cfg(test)]
use ironrdp_cliprdr::pdu::{PackedFileList, MAX_FILE_COUNT};
use ironrdp_core::impl_as_any;
use ironrdp_pdu::IntoOwned;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc;

use super::files::{clear_selection, FileSelection, FileWorker, FileWorkerCommand, FrozenFiles};
use super::formats::{
    fix_bitfields_dib, normalize_lf, to_crlf, utf16le_to_utf8, PendingWrite, SelectionKind,
    MAX_CLIPBOARD_SIZE,
};
#[cfg(feature = "client-to-server")]
use super::mount::RemoteMount;
#[cfg(feature = "client-to-server")]
use super::remote::uri_payload;
use super::remote::RemoteFiles;
use super::wayland::clipboard_thread;
use crate::config::FileTransferMode;

#[derive(Clone, Debug, Default)]
pub(super) struct ClipboardEchoCandidate {
    text: Option<Vec<u8>>,
    cf_dib: Option<Vec<u8>>,
}

/// Sends one locally originated format list and snapshots its advertised data.
/// The snapshot is eligible for the next remote paste request only.
pub(super) fn announce_local_formats(
    event_sender: &mpsc::UnboundedSender<ServerEvent>,
    echo_candidate: &Arc<Mutex<Option<ClipboardEchoCandidate>>>,
    clipboard_data: &Arc<Mutex<Option<Vec<u8>>>>,
    clipboard_image: &Arc<Mutex<Option<Vec<u8>>>>,
    formats: Vec<ClipboardFormat>,
) {
    if formats.is_empty() {
        return;
    }

    let has_text = formats
        .iter()
        .any(|format| format.id == ClipboardFormatId::CF_UNICODETEXT);
    let has_cf_dib = formats
        .iter()
        .any(|format| format.id == ClipboardFormatId::CF_DIB);
    let candidate = ClipboardEchoCandidate {
        text: has_text
            .then(|| clipboard_data.lock().ok().and_then(|data| data.clone()))
            .flatten(),
        cf_dib: has_cf_dib
            .then(|| clipboard_image.lock().ok().and_then(|data| data.clone()))
            .flatten(),
    };

    if let Ok(mut current) = echo_candidate.lock() {
        *current = Some(candidate);
    }

    if event_sender
        .send(ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(
            formats,
        )))
        .is_err()
    {
        if let Ok(mut current) = echo_candidate.lock() {
            *current = None;
        }
    }
}

pub struct HyprCliprdrFactory {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    file_transfer_mode: FileTransferMode,
    file_transfer_max_chunk_bytes: u32,
    file_transfer_max_entries: usize,
}

impl HyprCliprdrFactory {
    pub fn new(
        file_transfer_mode: FileTransferMode,
        file_transfer_max_chunk_bytes: u32,
        file_transfer_max_entries: usize,
    ) -> Self {
        Self {
            event_sender: None,
            file_transfer_mode,
            file_transfer_max_chunk_bytes,
            file_transfer_max_entries,
        }
    }
}

impl ServerEventSender for HyprCliprdrFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.event_sender = Some(sender);
    }
}

impl CliprdrBackendFactory for HyprCliprdrFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        let clipboard_data = Arc::new(Mutex::new(None::<Vec<u8>>));
        let clipboard_image = Arc::new(Mutex::new(None::<Vec<u8>>));
        let pending_write = Arc::new(Mutex::new(None::<PendingWrite>));
        let echo_candidate = Arc::new(Mutex::new(None::<ClipboardEchoCandidate>));
        let running = Arc::new(AtomicBool::new(true));
        let files: FrozenFiles = Arc::default();
        let file_worker = self.event_sender.as_ref().map(|sender| {
            FileWorker::start(
                Arc::clone(&files),
                sender.clone(),
                self.file_transfer_max_chunk_bytes,
                self.file_transfer_max_entries,
            )
        });
        let remote_files = self
            .event_sender
            .as_ref()
            .map(|sender| RemoteFiles::new(sender.clone(), self.file_transfer_max_entries));

        Box::new(HyprCliprdrBackend {
            event_sender: self.event_sender.clone(),
            remote_formats: Vec::new(),
            watcher_thread: None,
            clipboard_data,
            clipboard_image,
            pending_write,
            echo_candidate,
            running,
            last_requested_format: None,
            pending_echo_candidate: None,
            file_transfer_mode: self.file_transfer_mode,
            files,
            file_worker,
            remote_files,
            #[cfg(feature = "client-to-server")]
            mount: None,
        })
    }
}

impl CliprdrServerFactory for HyprCliprdrFactory {}

struct HyprCliprdrBackend {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    remote_formats: Vec<ClipboardFormat>,
    watcher_thread: Option<thread::JoinHandle<()>>,
    clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
    clipboard_image: Arc<Mutex<Option<Vec<u8>>>>, // CF_DIB bytes
    pending_write: Arc<Mutex<Option<PendingWrite>>>,
    echo_candidate: Arc<Mutex<Option<ClipboardEchoCandidate>>>,
    running: Arc<AtomicBool>,
    last_requested_format: Option<ClipboardFormatId>,
    pending_echo_candidate: Option<ClipboardEchoCandidate>,
    file_transfer_mode: FileTransferMode,
    files: FrozenFiles,
    file_worker: Option<FileWorker>,
    remote_files: Option<RemoteFiles>,
    /// This session's mount, created the first time the client advertises
    /// files. Owned here rather than by [`RemoteFiles`], which the mount's own
    /// filesystem holds: owning it there would be a cycle, and a mount inside
    /// a cycle is never dropped and so never unmounted.
    #[cfg(feature = "client-to-server")]
    mount: Option<RemoteMount>,
}

impl_as_any!(HyprCliprdrBackend);

impl fmt::Debug for HyprCliprdrBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyprCliprdrBackend")
            .field("remote_formats", &self.remote_formats.len())
            .field("watching", &self.watcher_thread.is_some())
            .finish()
    }
}

impl Drop for HyprCliprdrBackend {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        // Cancel before unmounting: a read still waiting on the client would
        // otherwise hold the kernel until its timeout, on a connection this
        // drop is already tearing down.
        if let Some(remote_files) = &self.remote_files {
            remote_files.cancel_pending();
        }
        #[cfg(feature = "client-to-server")]
        drop(self.mount.take());
        if let Some(handle) = self.watcher_thread.take() {
            let _ = handle.join();
        }
    }
}

impl CliprdrBackend for HyprCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        "/tmp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        let mut capabilities = ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES;
        if self.file_transfer_mode.permits_to_client()
            || self.file_transfer_mode.permits_to_server()
        {
            capabilities |= ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
                | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
                | ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED;
        }
        capabilities
    }

    fn on_ready(&mut self) {
        tracing::info!("Clipboard channel ready");
        self.start_clipboard_watcher();
    }

    fn on_request_format_list(&mut self) {
        let formats: Vec<ClipboardFormat> = SelectionKind::ALL
            .into_iter()
            .filter(|kind| self.has_local_selection(*kind))
            .filter_map(Self::local_format_for_kind)
            .map(ClipboardFormat::new)
            .collect();

        if !formats.is_empty() {
            if let Some(ref sender) = self.event_sender {
                announce_local_formats(
                    sender,
                    &self.echo_candidate,
                    &self.clipboard_data,
                    &self.clipboard_image,
                    formats,
                );
            }
        }
        if self.file_transfer_mode.permits_to_client() {
            if let Some(files) = self
                .files
                .lock()
                .ok()
                .and_then(|files| files.entries.clone())
            {
                if let Some(sender) = &self.event_sender {
                    let descriptors = files.into_iter().map(|file| file.descriptor).collect();
                    let _ = sender.send(ServerEvent::Clipboard(
                        ClipboardMessage::SendInitiateFileCopy(descriptors),
                    ));
                }
            }
        }
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        _capabilities: ClipboardGeneralCapabilityFlags,
    ) {
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        tracing::trace!(
            formats = available_formats.len(),
            "Clipboard: remote clipboard updated"
        );
        self.remote_formats = available_formats.to_vec();
        clear_selection(&self.files);
        if let Some(remote_files) = &self.remote_files {
            remote_files.cancel_pending();
        }
        let echo_candidate = self
            .echo_candidate
            .lock()
            .ok()
            .and_then(|mut candidate| candidate.take());

        // FileGroupDescriptorW is a delayed format: asking for it makes the
        // protocol library parse the descriptors and call on_remote_file_list.
        // Prefer it while this direction is enabled, then retain the ordinary
        // text/image preference for all other clipboard selections.
        let format = self
            .file_transfer_mode
            .permits_to_server()
            .then(|| {
                available_formats
                    .iter()
                    .find(|format| format.name.as_ref() == Some(&ClipboardFormatName::FILE_LIST))
                    .map(|format| format.id)
            })
            .flatten()
            .or_else(|| {
                SelectionKind::REMOTE_PREFERENCE
                    .into_iter()
                    .find_map(|kind| Self::remote_format_for_kind(kind, available_formats))
            });

        let Some(format) = format else {
            self.last_requested_format = None;
            self.pending_echo_candidate = None;
            return;
        };

        // Format Data Responses carry no request ID. Keep one request state for
        // repeated announcements that select the same format.
        if self.last_requested_format == Some(format) {
            return;
        }

        self.last_requested_format = Some(format);
        self.pending_echo_candidate = echo_candidate;
        if let Some(ref sender) = self.event_sender {
            let _ = sender.send(ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(
                format,
            )));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let response = match Self::local_kind_for_format(request.format) {
            Some(SelectionKind::Text) => {
                let data = self.clipboard_data.lock().ok().and_then(|g| g.clone());
                match data {
                    Some(ref data) if !data.is_empty() => {
                        let text = String::from_utf8_lossy(data);
                        FormatDataResponse::new_unicode_string(&to_crlf(&text)).into_owned()
                    }
                    _ => FormatDataResponse::new_error().into_owned(),
                }
            }
            Some(SelectionKind::Image) => {
                let data = self.clipboard_image.lock().ok().and_then(|g| g.clone());
                match data {
                    Some(dib_data) if !dib_data.is_empty() => {
                        FormatDataResponse::new_data(dib_data).into_owned()
                    }
                    _ => FormatDataResponse::new_error().into_owned(),
                }
            }
            Some(SelectionKind::Files) => {
                tracing::trace!("Clipboard: file selection support is not enabled yet");
                FormatDataResponse::new_error().into_owned()
            }
            None => FormatDataResponse::new_error().into_owned(),
        };

        if let Some(ref sender) = self.event_sender {
            let _ = sender.send(ServerEvent::Clipboard(ClipboardMessage::SendFormatData(
                response,
            )));
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        self.handle_format_data_response(response, MAX_CLIPBOARD_SIZE);
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        if let Some(worker) = self.to_client_worker() {
            worker.send(FileWorkerCommand::Read(request));
            return;
        }
        if let Some(sender) = &self.event_sender {
            let _ = sender.send(ServerEvent::Clipboard(
                ClipboardMessage::SendFileContentsResponse(FileContentsResponse::new_error(
                    request.stream_id,
                )),
            ));
        }
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        if let Some(remote_files) = &self.remote_files {
            remote_files.on_response(response);
        }
    }

    fn on_remote_file_list(
        &mut self,
        files: &[ironrdp_cliprdr::pdu::FileDescriptor],
        _clip_data_id: Option<u32>,
    ) {
        // This is where a file list answers the request `on_remote_copy` made,
        // the way `handle_format_data_response` answers it for every other
        // format. Clear the same request state it clears: left set, it makes
        // `on_remote_copy` read the client's next file copy as a repeat of this
        // one and drop it, so only the first copy of a session ever arrives.
        self.last_requested_format = None;
        self.pending_echo_candidate = None;

        if !self.file_transfer_mode.permits_to_server() {
            return;
        }
        let Some(pending) = self.advertise_remote_files(files) else {
            return;
        };
        if let Ok(mut pending_write) = self.pending_write.lock() {
            *pending_write = Some(pending);
        }
    }

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

impl HyprCliprdrBackend {
    /// Records the client's selection and offers it to the Wayland clipboard
    /// as URIs under this session's mount, mounting on the first selection
    /// that holds anything pasteable.
    #[cfg(feature = "client-to-server")]
    fn advertise_remote_files(
        &mut self,
        files: &[ironrdp_cliprdr::pdu::FileDescriptor],
    ) -> Option<PendingWrite> {
        let Some(remote_files) = self.remote_files.clone() else {
            tracing::warn!("Clipboard: client-to-server file transfer is unavailable");
            return None;
        };
        let roots = remote_files.accept(files);
        if roots.is_empty() {
            tracing::warn!("Clipboard: the client's file list held nothing that can be pasted");
            return None;
        }
        if self.mount.is_none() {
            self.mount = Some(RemoteMount::create(remote_files.filesystem())?);
        }
        uri_payload(self.mount.as_ref()?.path(), &roots)
    }

    #[cfg(not(feature = "client-to-server"))]
    fn advertise_remote_files(
        &mut self,
        _files: &[ironrdp_cliprdr::pdu::FileDescriptor],
    ) -> Option<PendingWrite> {
        tracing::warn!("Clipboard: client-to-server file transfer is not compiled in");
        None
    }

    /// The file worker, but only while this session may copy files to the client.
    fn to_client_worker(&self) -> Option<&FileWorker> {
        self.file_worker
            .as_ref()
            .filter(|_| self.file_transfer_mode.permits_to_client())
    }

    fn has_local_selection(&self, kind: SelectionKind) -> bool {
        match kind {
            SelectionKind::Text => self
                .clipboard_data
                .lock()
                .ok()
                .and_then(|data| data.as_ref().map(|data| !data.is_empty()))
                .unwrap_or(false),
            SelectionKind::Image => self
                .clipboard_image
                .lock()
                .ok()
                .and_then(|data| data.as_ref().map(|data| !data.is_empty()))
                .unwrap_or(false),
            SelectionKind::Files => false,
        }
    }

    fn local_format_for_kind(kind: SelectionKind) -> Option<ClipboardFormatId> {
        match kind {
            SelectionKind::Text => Some(ClipboardFormatId::CF_UNICODETEXT),
            SelectionKind::Image => Some(ClipboardFormatId::CF_DIB),
            SelectionKind::Files => None,
        }
    }

    fn local_kind_for_format(format: ClipboardFormatId) -> Option<SelectionKind> {
        match format {
            ClipboardFormatId::CF_UNICODETEXT => Some(SelectionKind::Text),
            ClipboardFormatId::CF_DIB => Some(SelectionKind::Image),
            _ => None,
        }
    }

    fn remote_format_for_kind(
        kind: SelectionKind,
        formats: &[ClipboardFormat],
    ) -> Option<ClipboardFormatId> {
        let has_format = |id| formats.iter().any(|format| format.id == id);
        match kind {
            SelectionKind::Text => has_format(ClipboardFormatId::CF_UNICODETEXT)
                .then_some(ClipboardFormatId::CF_UNICODETEXT),
            SelectionKind::Image => {
                if has_format(ClipboardFormatId::CF_DIBV5) {
                    Some(ClipboardFormatId::CF_DIBV5)
                } else {
                    has_format(ClipboardFormatId::CF_DIB).then_some(ClipboardFormatId::CF_DIB)
                }
            }
            SelectionKind::Files => None,
        }
    }

    fn handle_format_data_response(
        &mut self,
        response: FormatDataResponse<'_>,
        max_clipboard_size: usize,
    ) {
        let requested_format = self.last_requested_format.take();
        let echo_candidate = self.pending_echo_candidate.take();

        if response.is_error() {
            return;
        }

        let data = response.data();
        if data.is_empty() {
            return;
        }

        if data.len() > max_clipboard_size {
            tracing::warn!(
                size = data.len(),
                max = max_clipboard_size,
                "Clipboard data too large, ignoring"
            );
            return;
        }

        match requested_format {
            Some(ClipboardFormatId::CF_DIBV5) => {
                match ironrdp_cliprdr_format::bitmap::dibv5_to_png(data) {
                    Ok(png_data) => {
                        tracing::trace!(len = png_data.len(), "Clipboard: converted DIBV5 to PNG");
                        if let Ok(mut guard) = self.pending_write.lock() {
                            *guard = Some(PendingWrite::Image(png_data));
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Clipboard: failed to convert DIBV5 to PNG: {}", e);
                    }
                }
            }
            Some(ClipboardFormatId::CF_DIB) => {
                if echo_candidate
                    .as_ref()
                    .and_then(|candidate| candidate.cf_dib.as_deref())
                    .is_some_and(|announced| announced == data)
                {
                    tracing::debug!("Clipboard: client image echoes our own copy, ignoring");
                    return;
                }
                let png_result = ironrdp_cliprdr_format::bitmap::dib_to_png(data).or_else(|_| {
                    let fixed = fix_bitfields_dib(data).ok_or_else(|| {
                        ironrdp_cliprdr_format::bitmap::BitmapError::Unsupported(
                            "cannot fix BITFIELDS",
                        )
                    })?;
                    ironrdp_cliprdr_format::bitmap::dib_to_png(&fixed)
                });
                match png_result {
                    Ok(png_data) => {
                        tracing::trace!(len = png_data.len(), "Clipboard: converted DIB to PNG");
                        if let Ok(mut guard) = self.pending_write.lock() {
                            *guard = Some(PendingWrite::Image(png_data));
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Clipboard: failed to convert DIB to PNG: {}", e);
                    }
                }
            }
            Some(ClipboardFormatId::CF_UNICODETEXT) => {
                let utf8 = utf16le_to_utf8(data);
                if utf8.is_empty() {
                    return;
                }

                let normalized = normalize_lf(&utf8);
                if echo_candidate
                    .as_ref()
                    .and_then(|candidate| candidate.text.as_deref())
                    .is_some_and(|announced| {
                        normalize_lf(&String::from_utf8_lossy(announced)) == normalized
                    })
                {
                    tracing::debug!("Clipboard: client text echoes our own copy, ignoring");
                    return;
                }

                tracing::trace!(
                    len = normalized.len(),
                    "Clipboard: received text from RDP client"
                );
                if let Ok(mut guard) = self.pending_write.lock() {
                    *guard = Some(PendingWrite::Text(normalized.into_bytes()));
                }
            }
            Some(format) => {
                tracing::trace!(?format, "Clipboard: ignoring unrequested response format");
            }
            None => {
                tracing::trace!("Clipboard: ignoring format data response without pending request");
            }
        }
    }

    fn start_clipboard_watcher(&mut self) {
        let sender = match self.event_sender.clone() {
            Some(s) => s,
            None => return,
        };

        let clipboard_data = Arc::clone(&self.clipboard_data);
        let clipboard_image = Arc::clone(&self.clipboard_image);
        let pending_write = Arc::clone(&self.pending_write);
        let echo_candidate = Arc::clone(&self.echo_candidate);
        let running = Arc::clone(&self.running);
        let file_selection = FileSelection::new(
            Arc::clone(&self.files),
            self.to_client_worker().map(|worker| worker.sender()),
        );
        let remote_files = self.remote_files.clone();

        match thread::Builder::new()
            .name("clipboard-watcher".into())
            .spawn(move || {
                if let Err(e) = clipboard_thread(
                    sender,
                    clipboard_data,
                    clipboard_image,
                    pending_write,
                    echo_candidate,
                    running,
                    file_selection,
                    remote_files,
                ) {
                    tracing::error!("Clipboard thread error: {:#}", e);
                }
            }) {
            Ok(handle) => {
                self.watcher_thread = Some(handle);
                tracing::info!("Clipboard: watching via wlr-data-control-v1");
            }
            Err(e) => {
                tracing::error!("Clipboard: failed to spawn watcher thread: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_cliprdr::pdu::{ClipboardFileAttributes, FileDescriptor};
    use proptest::prelude::*;
    use std::io::{Cursor, Write};
    use std::os::unix::ffi::OsStringExt;
    use std::time::Duration;

    const ONE_BY_ONE_RGBA_PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, b'I', b'D', b'A', b'T', 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00,
        0x00, 0x00, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82,
    ];

    fn backend_with_events() -> (HyprCliprdrBackend, mpsc::UnboundedReceiver<ServerEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        (
            HyprCliprdrBackend {
                event_sender: Some(event_tx),
                remote_formats: Vec::new(),
                watcher_thread: None,
                clipboard_data: Arc::new(Mutex::new(None)),
                clipboard_image: Arc::new(Mutex::new(None)),
                pending_write: Arc::new(Mutex::new(None)),
                echo_candidate: Arc::new(Mutex::new(None)),
                running: Arc::new(AtomicBool::new(true)),
                last_requested_format: None,
                pending_echo_candidate: None,
                file_transfer_mode: FileTransferMode::Both,
                files: Arc::default(),
                file_worker: None,
                remote_files: None,
                #[cfg(feature = "client-to-server")]
                mount: None,
            },
            event_rx,
        )
    }

    /// A backend wired to remote files a test can drive directly, which is how
    /// the inbound direction is exercised without a mount. Must be called from
    /// inside a runtime: the remote files capture the current handle.
    fn backend_with_remote_files() -> (
        HyprCliprdrBackend,
        RemoteFiles,
        mpsc::UnboundedReceiver<ServerEvent>,
    ) {
        let (mut backend, events) = backend_with_events();
        let remote_files = RemoteFiles::new(backend.event_sender.clone().unwrap(), MAX_FILE_COUNT);
        backend.remote_files = Some(remote_files.clone());
        (backend, remote_files, events)
    }

    fn utf16le(text: &str) -> Vec<u8> {
        let mut bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        bytes.extend_from_slice(&[0, 0]);
        bytes
    }

    fn recv_file_response(
        event_rx: &mut mpsc::UnboundedReceiver<ServerEvent>,
    ) -> FileContentsResponse<'static> {
        let Some(ServerEvent::Clipboard(ClipboardMessage::SendFileContentsResponse(response))) =
            event_rx.blocking_recv()
        else {
            panic!("expected file response");
        };
        response
    }

    #[test]
    fn file_request_callback_serves_and_refuses_frozen_file_ranges() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let path =
            std::env::temp_dir().join(format!("hypr-rdp-file-callback-{}", std::process::id()));
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"clipboard bytes")
            .unwrap();
        let files: FrozenFiles = Arc::new(Mutex::new(
            Some(super::super::files::freeze_regular_files(
                vec![path.clone()],
            ))
            .into(),
        ));
        let worker = FileWorker::start(Arc::clone(&files), event_tx.clone(), 8, 100);
        let mut backend = HyprCliprdrBackend {
            event_sender: Some(event_tx),
            remote_formats: Vec::new(),
            watcher_thread: None,
            clipboard_data: Arc::new(Mutex::new(None)),
            clipboard_image: Arc::new(Mutex::new(None)),
            pending_write: Arc::new(Mutex::new(None)),
            echo_candidate: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(true)),
            last_requested_format: None,
            pending_echo_candidate: None,
            file_transfer_mode: FileTransferMode::Both,
            files,
            file_worker: Some(worker),
            remote_files: None,
            #[cfg(feature = "client-to-server")]
            mount: None,
        };

        backend.on_file_contents_request(FileContentsRequest {
            stream_id: 9,
            index: 0,
            flags: ironrdp_cliprdr::pdu::FileContentsFlags::RANGE,
            position: 10,
            requested_size: 8,
            data_id: None,
        });

        let response = recv_file_response(&mut event_rx);
        assert_eq!(response.stream_id(), 9);
        assert_eq!(response.data(), b"bytes");

        for request in [
            FileContentsRequest {
                stream_id: 10,
                index: 1,
                flags: ironrdp_cliprdr::pdu::FileContentsFlags::RANGE,
                position: 0,
                requested_size: 1,
                data_id: None,
            },
            FileContentsRequest {
                stream_id: 11,
                index: 0,
                flags: ironrdp_cliprdr::pdu::FileContentsFlags::RANGE,
                position: 0,
                requested_size: 9,
                data_id: None,
            },
        ] {
            backend.on_file_contents_request(request);
            assert!(recv_file_response(&mut event_rx).is_error());
        }

        std::fs::remove_file(&path).unwrap();
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"replacement")
            .unwrap();
        backend.on_file_contents_request(FileContentsRequest {
            stream_id: 12,
            index: 0,
            flags: ironrdp_cliprdr::pdu::FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id: None,
        });
        assert!(recv_file_response(&mut event_rx).is_error());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn remote_file_reads_use_the_clipboard_callback_seam() {
        let (mut backend, remote_files, mut events) = backend_with_remote_files();

        let read = remote_files.read(0, 3, 5);
        let Some(ServerEvent::Clipboard(ClipboardMessage::SendFileContentsRequest(request))) =
            events.recv().await
        else {
            panic!("expected a remote file-content request");
        };
        assert_eq!(request.index, 0);
        assert_eq!(request.position, 3);
        assert_eq!(request.requested_size, 5);

        backend.on_file_contents_response(FileContentsResponse::new_data_response(
            request.stream_id,
            b"bytes".to_vec(),
        ));

        assert_eq!(read.await.unwrap().unwrap(), b"bytes");
    }

    #[tokio::test]
    async fn clipboard_owner_change_cancels_pending_remote_file_reads() {
        let (mut backend, remote_files, mut events) = backend_with_remote_files();

        let read = remote_files.read(0, 0, 1);
        let _ = events.recv().await.expect("remote file request");
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]);

        assert!(read.await.unwrap().is_err());
    }

    /// A session that ends with reads still in flight must fail them now: the
    /// connection they are waiting on is gone, and the process reading the
    /// mount would otherwise sit in the kernel until the read timeout.
    #[tokio::test]
    async fn session_end_cancels_pending_remote_file_reads() {
        let (backend, remote_files, mut events) = backend_with_remote_files();

        let read = remote_files.read(0, 0, 1);
        let _ = events.recv().await.expect("remote file request");
        drop(backend);

        assert!(read.await.unwrap().is_err());
    }

    /// A folder the client copied arrives as a flat descriptor list whose
    /// relative paths describe the tree. Rebuilding that tree is what the
    /// mount browses, and a read of a file several levels down has to reach
    /// the client as a request for that file's index in the original list.
    #[cfg(feature = "client-to-server")]
    #[tokio::test]
    async fn a_pasted_folder_reads_its_nested_files_through_the_callback_seam() {
        use super::super::remote_tree::{RemoteNodeKind, ROOT_INODE};
        use ironrdp_cliprdr::pdu::{ClipboardFileAttributes, FileDescriptor};

        let (mut backend, remote_files, mut events) = backend_with_remote_files();

        let advertised = remote_files.accept(&[
            FileDescriptor::new("project").with_attributes(ClipboardFileAttributes::DIRECTORY),
            FileDescriptor::new("src")
                .with_attributes(ClipboardFileAttributes::DIRECTORY)
                .with_relative_path("project"),
            FileDescriptor::new("main.rs")
                .with_attributes(ClipboardFileAttributes::NORMAL)
                .with_file_size(5)
                .with_relative_path("project\\src"),
            FileDescriptor::new("empty")
                .with_attributes(ClipboardFileAttributes::DIRECTORY)
                .with_relative_path("project"),
        ]);

        assert_eq!(advertised, ["project"]);
        let (empty, kind) = remote_files
            .resolve("project/empty")
            .expect("the empty directory is browsable");
        assert_eq!(kind, RemoteNodeKind::Directory);
        assert!(remote_files.child_names(empty).is_empty());
        assert_eq!(remote_files.child_names(ROOT_INODE), ["project"]);

        let (_, kind) = remote_files
            .resolve("project/src/main.rs")
            .expect("the nested file is browsable");
        let RemoteNodeKind::File { index } = kind else {
            panic!("expected a readable file at the bottom of the tree");
        };

        let read = remote_files.read(index, 0, 5);
        let Some(ServerEvent::Clipboard(ClipboardMessage::SendFileContentsRequest(request))) =
            events.recv().await
        else {
            panic!("expected a remote file-content request");
        };
        assert_eq!(request.index, 2);

        backend.on_file_contents_response(FileContentsResponse::new_data_response(
            request.stream_id,
            b"crate".to_vec(),
        ));

        assert_eq!(read.await.unwrap().unwrap(), b"crate");
    }

    /// A paste the server cannot give the desktop anywhere to read from costs
    /// the paste and nothing else: the backend leaves the clipboard as it was
    /// and goes on serving, where ending the session over it would take the
    /// user's whole desktop with it.
    ///
    /// This drives the arm where there is nothing to advertise *through*. The
    /// sibling arm — a mount that cannot be created — reaches the same
    /// handling by a different route, and is verified by hand rather than
    /// here; see row 48 of the acceptance matrix.
    #[cfg(feature = "client-to-server")]
    #[test]
    fn a_paste_with_nowhere_to_land_leaves_the_session_serving() {
        use ironrdp_cliprdr::pdu::FileDescriptor;

        let (mut backend, mut events) = backend_with_events();
        assert!(backend.remote_files.is_none());

        backend.on_remote_file_list(&[FileDescriptor::new("report.pdf").with_file_size(3)], None);

        assert!(
            backend.pending_write.lock().unwrap().is_none(),
            "nothing is offered to the desktop"
        );

        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]);
        assert!(
            matches!(
                events.blocking_recv(),
                Some(ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(
                    _
                )))
            ),
            "the session still serves the clipboard"
        );
    }

    /// Gated: without the client-to-server direction compiled in,
    /// `permits_to_server` is false whatever the mode says, so the backend
    /// never asks for the file list and the receive below would block forever
    /// rather than fail.
    #[cfg(feature = "client-to-server")]
    #[test]
    fn client_file_format_starts_the_delayed_file_list_exchange() {
        let (mut backend, mut events) = backend_with_events();
        let file_list = ClipboardFormat::new(ClipboardFormatId::new(0xc001))
            .with_name(ClipboardFormatName::FILE_LIST);

        backend.on_remote_copy(&[file_list]);

        let Some(ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(format))) =
            events.blocking_recv()
        else {
            panic!("expected delayed file-list request");
        };
        assert_eq!(format, ClipboardFormatId::new(0xc001));
    }

    /// A second copy on the client must reach the desktop like the first.
    ///
    /// `last_requested_format` dedupes repeated announcements of one selection,
    /// and `handle_format_data_response` clears it when the answer arrives. A
    /// file list does not come back that way — it arrives at
    /// `on_remote_file_list` — so if that arm does not clear the state too, the
    /// id stays set, every later announcement of the same format is dropped as
    /// a repeat, and only the first file copy of a whole session is ever
    /// honoured. Found against a real client in the slice 2 acceptance pass:
    /// the mount kept serving the first selection and no later copy appeared.
    #[cfg(feature = "client-to-server")]
    #[test]
    fn a_later_file_copy_on_the_client_is_fetched_like_the_first() {
        use ironrdp_cliprdr::pdu::FileDescriptor;

        let (mut backend, mut events) = backend_with_events();
        let file_list = ClipboardFormat::new(ClipboardFormatId::new(0xc001))
            .with_name(ClipboardFormatName::FILE_LIST);

        backend.on_remote_copy(std::slice::from_ref(&file_list));
        assert!(
            matches!(
                recv_clipboard_event(&mut events),
                ClipboardMessage::SendInitiatePaste(_)
            ),
            "the first copy is fetched"
        );
        backend.on_remote_file_list(&[FileDescriptor::new("one.txt").with_file_size(3)], None);

        backend.on_remote_copy(std::slice::from_ref(&file_list));

        let ClipboardMessage::SendInitiatePaste(format) = recv_clipboard_event(&mut events) else {
            panic!("the second copy must be fetched too, not dropped as a repeat");
        };
        assert_eq!(format, ClipboardFormatId::new(0xc001));
    }

    /// Builds a directory tree carrying every enumeration hazard ticket 04
    /// names: an empty directory, a symlink to a file, a symlink cycle back to
    /// the root, a FIFO, and a socket. Enumerating `root` must yield exactly the
    /// root, `nested`, `nested/empty`, `nested/document.txt`, and
    /// `nested/shortcut.txt`.
    ///
    /// Returns the bound socket, which the caller must keep alive for the walk.
    fn build_hazardous_tree(root: &std::path::Path) -> std::os::unix::net::UnixDatagram {
        let _ = std::fs::remove_dir_all(root);
        std::fs::create_dir_all(root.join("nested/empty")).unwrap();
        std::fs::File::create(root.join("nested/document.txt"))
            .unwrap()
            .write_all(b"contents")
            .unwrap();
        std::os::unix::fs::symlink(root, root.join("nested/loop")).unwrap();
        std::os::unix::fs::symlink(
            root.join("nested/document.txt"),
            root.join("nested/shortcut.txt"),
        )
        .unwrap();
        let fifo = std::ffi::CString::new(
            root.join("nested/ignored-fifo")
                .as_os_str()
                .as_encoded_bytes(),
        )
        .unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        std::os::unix::net::UnixDatagram::bind(root.join("nested/ignored-socket")).unwrap()
    }

    #[test]
    fn file_worker_offers_a_bounded_directory_tree() {
        let root =
            std::env::temp_dir().join(format!("hypr-rdp-file-worker-{}", std::process::id()));
        let socket = build_hazardous_tree(&root);

        let (mut backend, mut events) = backend_with_events();
        let worker = FileWorker::start(
            Arc::clone(&backend.files),
            backend.event_sender.as_ref().unwrap().clone(),
            1024,
            10,
        );
        worker.send(FileWorkerCommand::Freeze {
            paths: vec![root.clone()],
            generation: 0,
        });

        let Some(ServerEvent::Clipboard(ClipboardMessage::SendInitiateFileCopy(_))) =
            events.blocking_recv()
        else {
            panic!("expected the worker to freeze a file offer");
        };
        backend.on_request_format_list();
        let Some(ServerEvent::Clipboard(ClipboardMessage::SendInitiateFileCopy(descriptors))) =
            events.blocking_recv()
        else {
            panic!("expected the backend to re-advertise the file offer");
        };
        let decoded = FormatDataResponse::new_file_list(&PackedFileList { files: descriptors })
            .unwrap()
            .to_file_list()
            .unwrap();
        let root_name = root.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            decoded
                .files
                .iter()
                .map(|descriptor| descriptor.name.as_str())
                .collect::<Vec<_>>(),
            [
                root_name,
                &format!("{root_name}\\nested"),
                &format!("{root_name}\\nested\\empty"),
                &format!("{root_name}\\nested\\document.txt"),
                &format!("{root_name}\\nested\\shortcut.txt"),
            ]
        );
        // Directories arrive as directories and carry no content length; the
        // symlink arrives as the file it points at.
        assert_eq!(
            decoded
                .files
                .iter()
                .map(|descriptor| (descriptor.attributes, descriptor.file_size))
                .collect::<Vec<_>>(),
            [
                (Some(ClipboardFileAttributes::DIRECTORY), None),
                (Some(ClipboardFileAttributes::DIRECTORY), None),
                (Some(ClipboardFileAttributes::DIRECTORY), None),
                (Some(ClipboardFileAttributes::NORMAL), Some(8)),
                (Some(ClipboardFileAttributes::NORMAL), Some(8)),
            ]
        );

        drop(worker);

        let worker = FileWorker::start(
            Arc::clone(&backend.files),
            backend.event_sender.as_ref().unwrap().clone(),
            1024,
            3,
        );
        worker.send(FileWorkerCommand::Freeze {
            paths: vec![root.clone()],
            generation: 0,
        });
        let Some(ServerEvent::Clipboard(ClipboardMessage::SendInitiateFileCopy(_))) =
            events.blocking_recv()
        else {
            panic!("expected the worker to freeze a truncated file offer");
        };
        backend.on_request_format_list();
        let Some(ServerEvent::Clipboard(ClipboardMessage::SendInitiateFileCopy(descriptors))) =
            events.blocking_recv()
        else {
            panic!("expected the backend to re-advertise the truncated file offer");
        };
        let truncated = FormatDataResponse::new_file_list(&PackedFileList { files: descriptors })
            .unwrap()
            .to_file_list()
            .unwrap();
        // The work budget counts inspected entries, including skipped sockets
        // and cycles. Which child survives depends on read_dir order.
        assert!((2..=3).contains(&truncated.files.len()));
        assert_eq!(truncated.files[0].name, root_name);
        assert_eq!(truncated.files[1].name, format!("{root_name}\\nested"));

        drop(worker);
        drop(socket);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_offer_adjusts_names_that_windows_would_reject_without_dropping_them() {
        let (mut backend, mut events) = backend_with_events();
        let root = std::env::temp_dir().join(format!(
            "hypr-rdp-name-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let paths = [
            "a:b".into(),
            "a?b".into(),
            "CON.txt".into(),
            "trailing. ".into(),
            std::ffi::OsString::from_vec(vec![0xff]),
        ]
        .into_iter()
        .map(|name: std::ffi::OsString| {
            let path = root.join(name);
            std::fs::File::create(&path).unwrap();
            path
        })
        .collect();
        backend.files.lock().unwrap().entries = Some(super::super::files::freeze_paths(paths, 100));

        backend.on_request_format_list();

        let descriptors = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                    .await
                    .expect("timed out waiting for file offer")
                    .expect("backend stopped before offering files");
                let ServerEvent::Clipboard(ClipboardMessage::SendInitiateFileCopy(descriptors)) =
                    event
                else {
                    panic!("expected a file offer");
                };
                descriptors
            });
        let decoded: Vec<FileDescriptor> = descriptors
            .iter()
            .map(|descriptor| {
                ironrdp_core::decode(&ironrdp_core::encode_vec(descriptor).unwrap()).unwrap()
            })
            .collect();

        assert_eq!(
            decoded
                .iter()
                .map(|descriptor| descriptor.name.as_str())
                .collect::<Vec<_>>(),
            ["a_b", "a_b (2)", "CON_.txt", "trailing", "�"]
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn client_text_echo_of_our_copy_keeps_wayland_selection() {
        let (mut backend, mut event_rx) = backend_with_events();
        *backend.clipboard_data.lock().unwrap() = Some(b"hello\nworld\rend".to_vec());
        backend.on_request_format_list();
        let _ = recv_clipboard_event(&mut event_rx);
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]);
        let _ = recv_clipboard_event(&mut event_rx);

        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });
        let ClipboardMessage::SendFormatData(response) = recv_clipboard_event(&mut event_rx) else {
            panic!("expected SendFormatData");
        };
        let payload = response.data().to_vec();
        backend.handle_format_data_response(
            FormatDataResponse::new_data(payload.as_slice()),
            MAX_CLIPBOARD_SIZE,
        );

        assert!(backend.pending_write.lock().unwrap().is_none());
    }

    #[test]
    fn a_format_list_we_do_not_request_does_not_strand_the_candidate() {
        let (mut backend, mut event_rx) = backend_with_events();
        *backend.clipboard_data.lock().unwrap() = Some(b"hello".to_vec());
        backend.on_request_format_list();
        let _ = recv_clipboard_event(&mut event_rx);

        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::new(0x8000))]);

        assert!(backend.pending_echo_candidate.is_none());
        assert!(backend.last_requested_format.is_none());
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn duplicate_format_list_reuses_the_outstanding_echo_request() {
        let (mut backend, mut event_rx) = backend_with_events();
        *backend.clipboard_data.lock().unwrap() = Some(b"hello\nworld".to_vec());
        backend.on_request_format_list();
        let _ = recv_clipboard_event(&mut event_rx);

        let formats = [ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
        backend.on_remote_copy(&formats);
        assert!(matches!(
            recv_clipboard_event(&mut event_rx),
            ClipboardMessage::SendInitiatePaste(ClipboardFormatId::CF_UNICODETEXT)
        ));

        backend.on_remote_copy(&formats);
        assert!(event_rx.try_recv().is_err());

        let payload = utf16le("hello\r\nworld");
        backend.handle_format_data_response(
            FormatDataResponse::new_data(payload.as_slice()),
            MAX_CLIPBOARD_SIZE,
        );

        assert!(backend.pending_write.lock().unwrap().is_none());
        assert!(backend.last_requested_format.is_none());
        assert!(backend.pending_echo_candidate.is_none());

        backend.on_remote_copy(&formats);
        assert!(matches!(
            recv_clipboard_event(&mut event_rx),
            ClipboardMessage::SendInitiatePaste(ClipboardFormatId::CF_UNICODETEXT)
        ));
        backend.handle_format_data_response(
            FormatDataResponse::new_data(payload.as_slice()),
            MAX_CLIPBOARD_SIZE,
        );

        assert!(matches!(
            backend.pending_write.lock().unwrap().as_ref(),
            Some(PendingWrite::Text(text)) if text == b"hello\nworld"
        ));
    }

    #[test]
    fn client_text_copy_is_written_with_normalized_line_endings() {
        let (mut backend, _rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_UNICODETEXT);

        let payload = utf16le("from\rclient\r\nnext");
        backend.handle_format_data_response(
            FormatDataResponse::new_data(payload.as_slice()),
            MAX_CLIPBOARD_SIZE,
        );

        let pending = backend.pending_write.lock().unwrap();
        let Some(PendingWrite::Text(text)) = pending.as_ref() else {
            panic!("expected text pending write");
        };
        assert_eq!(text.as_slice(), b"from\nclient\nnext");
    }

    #[test]
    fn client_image_echo_of_our_copy_keeps_wayland_selection() {
        let (mut backend, mut event_rx) = backend_with_events();
        let dib = ironrdp_cliprdr_format::bitmap::png_to_cf_dib(ONE_BY_ONE_RGBA_PNG)
            .expect("test PNG converts to DIB");
        *backend.clipboard_image.lock().unwrap() = Some(dib.clone());
        backend.on_request_format_list();
        let _ = recv_clipboard_event(&mut event_rx);
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_DIB)]);
        let _ = recv_clipboard_event(&mut event_rx);

        backend.handle_format_data_response(
            FormatDataResponse::new_data(dib.as_slice()),
            MAX_CLIPBOARD_SIZE,
        );

        assert!(backend.pending_write.lock().unwrap().is_none());
    }

    #[test]
    fn later_client_text_copy_with_same_content_is_not_treated_as_an_echo() {
        let (mut backend, mut event_rx) = backend_with_events();
        *backend.clipboard_data.lock().unwrap() = Some(b"same\ntext".to_vec());
        backend.on_request_format_list();
        let _ = recv_clipboard_event(&mut event_rx);

        let formats = [ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
        let payload = utf16le("same\r\ntext");
        backend.on_remote_copy(&formats);
        let _ = recv_clipboard_event(&mut event_rx);
        backend.on_format_data_response(FormatDataResponse::new_data(&payload));
        assert!(backend.pending_write.lock().unwrap().is_none());

        backend.on_remote_copy(&formats);
        let _ = recv_clipboard_event(&mut event_rx);
        backend.on_format_data_response(FormatDataResponse::new_data(&payload));

        let pending = backend.pending_write.lock().unwrap();
        let Some(PendingWrite::Text(text)) = pending.as_ref() else {
            panic!("expected later client copy to replace the Wayland selection");
        };
        assert_eq!(text, b"same\ntext");
    }

    #[test]
    fn failed_echo_response_consumes_the_candidate() {
        let (mut backend, mut event_rx) = backend_with_events();
        *backend.clipboard_data.lock().unwrap() = Some(b"same".to_vec());
        backend.on_request_format_list();
        let _ = recv_clipboard_event(&mut event_rx);

        let formats = [ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
        backend.on_remote_copy(&formats);
        let _ = recv_clipboard_event(&mut event_rx);
        backend.on_format_data_response(FormatDataResponse::new_error());

        backend.on_remote_copy(&formats);
        let _ = recv_clipboard_event(&mut event_rx);
        backend.on_format_data_response(FormatDataResponse::new_data(utf16le("same")));

        assert!(matches!(
            backend.pending_write.lock().unwrap().as_ref(),
            Some(PendingWrite::Text(text)) if text == b"same"
        ));
    }

    #[test]
    fn cf_dibv5_response_is_not_compared_with_cf_dib_echo_candidate() {
        let (mut backend, mut event_rx) = backend_with_events();
        let dibv5 = ironrdp_cliprdr_format::bitmap::png_to_cf_dibv5(ONE_BY_ONE_RGBA_PNG)
            .expect("test PNG converts to DIBV5");
        *backend.clipboard_image.lock().unwrap() = Some(dibv5.clone());
        backend.on_request_format_list();
        let _ = recv_clipboard_event(&mut event_rx);

        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_DIBV5)]);
        let _ = recv_clipboard_event(&mut event_rx);
        backend.on_format_data_response(FormatDataResponse::new_data(&dibv5));

        assert_pending_image_pixel(&backend, png::ColorType::Rgba, &[255, 0, 0, 255]);
    }

    fn recv_clipboard_event(
        event_rx: &mut mpsc::UnboundedReceiver<ServerEvent>,
    ) -> ClipboardMessage {
        match event_rx.try_recv().expect("clipboard event queued") {
            ServerEvent::Clipboard(message) => message,
            other => panic!("unexpected server event: {other:?}"),
        }
    }

    fn decode_png(data: &[u8]) -> (u32, u32, png::ColorType, Vec<u8>) {
        let decoder = png::Decoder::new(Cursor::new(data));
        let mut reader = decoder.read_info().expect("PNG header decodes");
        let mut buffer = vec![0; reader.output_buffer_size().expect("PNG output buffer size")];
        let info = reader.next_frame(&mut buffer).expect("PNG frame decodes");

        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        buffer.truncate(info.buffer_size());
        (info.width, info.height, info.color_type, buffer)
    }

    fn rgba_png_pixel(pixel: [u8; 4]) -> Vec<u8> {
        let mut png_data = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_data, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("PNG header");
            writer.write_image_data(&pixel).expect("PNG pixel");
        }
        png_data
    }

    fn assert_pending_image_pixel(
        backend: &HyprCliprdrBackend,
        color_type: png::ColorType,
        pixel: &[u8],
    ) {
        let pending = backend.pending_write.lock().unwrap();
        let PendingWrite::Image(data) = pending.as_ref().expect("pending write") else {
            panic!("expected image pending write");
        };
        let (width, height, actual_color_type, actual_pixel) = decode_png(data);

        assert_eq!((width, height), (1, 1));
        assert_eq!(actual_color_type, color_type);
        assert_eq!(actual_pixel, pixel);
    }

    fn bitfields_dib_from_png() -> Vec<u8> {
        let dib = ironrdp_cliprdr_format::bitmap::png_to_cf_dib(ONE_BY_ONE_RGBA_PNG)
            .expect("test PNG converts to DIB");
        let mut bitfields = Vec::with_capacity(dib.len() + 12);
        bitfields.extend_from_slice(&dib[..16]);
        bitfields.extend_from_slice(&3u32.to_le_bytes());
        bitfields.extend_from_slice(&dib[20..40]);
        bitfields.extend_from_slice(&0x00ff_0000u32.to_le_bytes());
        bitfields.extend_from_slice(&0x0000_ff00u32.to_le_bytes());
        bitfields.extend_from_slice(&0x0000_00ffu32.to_le_bytes());
        bitfields.extend_from_slice(&dib[40..]);
        bitfields
    }

    #[test]
    fn request_format_list_advertises_text_and_image_formats() {
        let (mut backend, mut event_rx) = backend_with_events();
        *backend.clipboard_data.lock().unwrap() = Some(b"hello".to_vec());
        *backend.clipboard_image.lock().unwrap() = Some(vec![1, 2, 3, 4]);

        backend.on_request_format_list();

        let ClipboardMessage::SendInitiateCopy(formats) = recv_clipboard_event(&mut event_rx)
        else {
            panic!("expected SendInitiateCopy");
        };
        let ids = formats.iter().map(|format| format.id).collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![ClipboardFormatId::CF_UNICODETEXT, ClipboardFormatId::CF_DIB]
        );
        let candidate = backend.echo_candidate.lock().unwrap();
        let candidate = candidate.as_ref().expect("echo candidate armed");
        assert_eq!(candidate.text.as_deref(), Some(b"hello".as_slice()));
        assert_eq!(candidate.cf_dib.as_deref(), Some([1, 2, 3, 4].as_slice()));
    }

    #[test]
    fn failed_local_format_announcement_does_not_leave_an_echo_candidate() {
        let (mut backend, event_rx) = backend_with_events();
        drop(event_rx);
        *backend.clipboard_data.lock().unwrap() = Some(b"hello".to_vec());

        backend.on_request_format_list();

        assert!(backend.echo_candidate.lock().unwrap().is_none());
    }

    #[test]
    fn request_format_list_does_not_emit_empty_clipboard() {
        let (mut backend, mut event_rx) = backend_with_events();

        backend.on_request_format_list();

        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn remote_copy_prefers_unicode_then_dibv5_then_dib() {
        for (formats, expected) in [
            (
                vec![
                    ClipboardFormat::new(ClipboardFormatId::CF_DIB),
                    ClipboardFormat::new(ClipboardFormatId::CF_DIBV5),
                    ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
                ],
                ClipboardFormatId::CF_UNICODETEXT,
            ),
            (
                vec![
                    ClipboardFormat::new(ClipboardFormatId::CF_DIB),
                    ClipboardFormat::new(ClipboardFormatId::CF_DIBV5),
                ],
                ClipboardFormatId::CF_DIBV5,
            ),
            (
                vec![ClipboardFormat::new(ClipboardFormatId::CF_DIB)],
                ClipboardFormatId::CF_DIB,
            ),
        ] {
            let (mut backend, mut event_rx) = backend_with_events();

            backend.on_remote_copy(&formats);

            assert_eq!(backend.last_requested_format, Some(expected));
            let ClipboardMessage::SendInitiatePaste(format) = recv_clipboard_event(&mut event_rx)
            else {
                panic!("expected SendInitiatePaste");
            };
            assert_eq!(format, expected);
        }
    }

    #[test]
    fn remote_copy_ignores_unsupported_formats() {
        let (mut backend, mut event_rx) = backend_with_events();
        let formats = [ClipboardFormat::new(ClipboardFormatId::CF_TEXT)];

        backend.on_remote_copy(&formats);

        assert_eq!(backend.remote_formats, formats);
        assert_eq!(backend.last_requested_format, None);
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn remote_copy_clears_stale_requested_format_when_no_supported_format_exists() {
        let (mut backend, mut event_rx) = backend_with_events();

        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]);
        let ClipboardMessage::SendInitiatePaste(format) = recv_clipboard_event(&mut event_rx)
        else {
            panic!("expected SendInitiatePaste");
        };
        assert_eq!(format, ClipboardFormatId::CF_UNICODETEXT);
        assert_eq!(
            backend.last_requested_format,
            Some(ClipboardFormatId::CF_UNICODETEXT)
        );

        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_TEXT)]);

        assert_eq!(backend.last_requested_format, None);
        assert!(event_rx.try_recv().is_err());
    }

    fn unicode_response_text(data: &[u8]) -> String {
        let (pairs, _) = data.as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|c| u16::from_le_bytes(*c)).collect();
        let units = units.strip_suffix(&[0]).unwrap_or(&units);
        String::from_utf16_lossy(units)
    }

    #[test]
    fn unicode_text_response_ends_every_line_with_crlf() {
        let (mut backend, mut event_rx) = backend_with_events();
        *backend.clipboard_data.lock().unwrap() = Some("\none\rtwo\r\n四".as_bytes().to_vec());

        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });

        let ClipboardMessage::SendFormatData(response) = recv_clipboard_event(&mut event_rx) else {
            panic!("expected SendFormatData");
        };
        assert_eq!(
            unicode_response_text(response.data()),
            "\r\none\r\ntwo\r\n四"
        );
    }

    #[test]
    fn format_data_request_returns_unicode_text_response() {
        let (mut backend, mut event_rx) = backend_with_events();
        *backend.clipboard_data.lock().unwrap() = Some("hello".as_bytes().to_vec());

        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });

        let ClipboardMessage::SendFormatData(response) = recv_clipboard_event(&mut event_rx) else {
            panic!("expected SendFormatData");
        };
        assert!(!response.is_error());
        assert_eq!(
            response.data(),
            &[b'h', 0, b'e', 0, b'l', 0, b'l', 0, b'o', 0, 0, 0]
        );
    }

    #[test]
    fn format_data_response_writes_text_pending_for_wayland() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_UNICODETEXT);

        backend.on_format_data_response(FormatDataResponse::new_data(&[b'o', 0, b'k', 0, 0, 0]));

        let pending = backend.pending_write.lock().unwrap();
        match pending.as_ref().expect("pending write") {
            PendingWrite::Text(data) => assert_eq!(data, b"ok"),
            PendingWrite::Image(_) | PendingWrite::Files { .. } => {
                panic!("expected text pending write")
            }
        }
    }

    #[test]
    fn format_data_response_without_pending_request_is_ignored() {
        let (mut backend, _event_rx) = backend_with_events();

        backend.on_format_data_response(FormatDataResponse::new_data(&[b'o', 0, b'k', 0, 0, 0]));

        assert!(backend.pending_write.lock().unwrap().is_none());
    }

    #[test]
    fn late_format_data_response_after_unsupported_copy_is_ignored() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_UNICODETEXT);
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_TEXT)]);

        backend.on_format_data_response(FormatDataResponse::new_data(&[b'o', 0, b'k', 0, 0, 0]));

        assert_eq!(backend.last_requested_format, None);
        assert!(backend.pending_write.lock().unwrap().is_none());
    }

    #[test]
    fn format_data_response_ignores_oversized_payload_without_mutating_pending_write() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_UNICODETEXT);
        *backend.pending_write.lock().unwrap() = Some(PendingWrite::Text(b"old".to_vec()));
        let oversized = [0, 0, 0, 0, 0];

        backend.handle_format_data_response(FormatDataResponse::new_data(&oversized), 4);

        assert_eq!(backend.last_requested_format, None);
        let pending = backend.pending_write.lock().unwrap();
        match pending.as_ref().expect("existing pending write remains") {
            PendingWrite::Text(data) => assert_eq!(data, b"old"),
            PendingWrite::Image(_) | PendingWrite::Files { .. } => {
                panic!("expected existing text pending write")
            }
        }
    }

    #[test]
    fn format_data_response_writes_dib_image_pending_for_wayland() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_DIB);
        let dib = ironrdp_cliprdr_format::bitmap::png_to_cf_dib(ONE_BY_ONE_RGBA_PNG)
            .expect("test PNG converts to DIB");

        backend.on_format_data_response(FormatDataResponse::new_data(&dib));

        assert_eq!(backend.last_requested_format, None);
        assert_pending_image_pixel(&backend, png::ColorType::Rgb, &[255, 0, 0]);
    }

    #[test]
    fn format_data_response_writes_dibv5_image_pending_for_wayland() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_DIBV5);
        let dibv5 = ironrdp_cliprdr_format::bitmap::png_to_cf_dibv5(ONE_BY_ONE_RGBA_PNG)
            .expect("test PNG converts to DIBV5");

        backend.on_format_data_response(FormatDataResponse::new_data(&dibv5));

        assert_eq!(backend.last_requested_format, None);
        assert_pending_image_pixel(&backend, png::ColorType::Rgba, &[255, 0, 0, 255]);
    }

    #[test]
    fn format_data_response_preserves_dibv5_transparent_alpha_for_wayland() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_DIBV5);
        let png = rgba_png_pixel([17, 34, 51, 127]);
        let dibv5 =
            ironrdp_cliprdr_format::bitmap::png_to_cf_dibv5(&png).expect("PNG converts to DIBV5");

        backend.on_format_data_response(FormatDataResponse::new_data(&dibv5));

        assert_eq!(backend.last_requested_format, None);
        assert_pending_image_pixel(&backend, png::ColorType::Rgba, &[17, 34, 51, 127]);
    }

    #[test]
    fn format_data_response_writes_dib_alpha_as_rgb_for_wayland() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_DIB);
        let png = rgba_png_pixel([17, 34, 51, 127]);
        let dib = ironrdp_cliprdr_format::bitmap::png_to_cf_dib(&png).expect("PNG converts to DIB");

        backend.on_format_data_response(FormatDataResponse::new_data(&dib));

        assert_eq!(backend.last_requested_format, None);
        assert_pending_image_pixel(&backend, png::ColorType::Rgb, &[17, 34, 51]);
    }

    #[test]
    fn format_data_response_repairs_bitfields_dib_before_png_conversion() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_DIB);
        let dib = bitfields_dib_from_png();

        backend.on_format_data_response(FormatDataResponse::new_data(&dib));

        assert_eq!(backend.last_requested_format, None);
        assert_pending_image_pixel(&backend, png::ColorType::Rgb, &[255, 0, 0]);
    }

    #[test]
    fn format_data_response_ignores_corrupt_dib_without_pending_write() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_DIB);

        backend.on_format_data_response(FormatDataResponse::new_data(b"not a dib"));

        assert_eq!(backend.last_requested_format, None);
        assert!(backend.pending_write.lock().unwrap().is_none());
    }

    proptest! {
        #[test]
        fn generated_clipboard_image_responses_do_not_panic_or_write_invalid_png(
            data in proptest::collection::vec(any::<u8>(), 0..256),
            use_dibv5 in any::<bool>(),
        ) {
            let (mut backend, _event_rx) = backend_with_events();
            backend.last_requested_format = Some(if use_dibv5 {
                ClipboardFormatId::CF_DIBV5
            } else {
                ClipboardFormatId::CF_DIB
            });

            backend.handle_format_data_response(FormatDataResponse::new_data(&data), 256);

            if let Some(PendingWrite::Image(png_data)) = backend.pending_write.lock().unwrap().as_ref() {
                let _ = decode_png(png_data);
            }
            prop_assert_eq!(backend.last_requested_format, None);
        }
    }
}
