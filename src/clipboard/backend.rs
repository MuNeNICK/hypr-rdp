use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
#[cfg(test)]
use ironrdp_cliprdr::pdu::PackedFileList;
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags,
    FileContentsRequest, FileContentsResponse, FileDescriptor, FormatDataRequest,
    FormatDataResponse, LockDataId,
};
use ironrdp_core::impl_as_any;
use ironrdp_pdu::IntoOwned;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc;

use super::files::{
    clear_selection, file_stream_enabled, set_file_capabilities, FileSelection, FileWorker,
    FrozenFiles,
};
use super::formats::{
    fix_bitfields_dib, normalize_lf, to_crlf, utf16le_to_utf8, PendingWrite, SelectionKind,
    MAX_CLIPBOARD_SIZE,
};
use super::inbound::InboundClipboard;
use super::wayland::{clipboard_thread, ClipboardShared};
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
        let inbound = self
            .event_sender
            .as_ref()
            .filter(|_| {
                self.file_transfer_mode.permits_to_server() && InboundClipboard::available()
            })
            .map(|sender| {
                InboundClipboard::new(
                    sender.clone(),
                    Arc::clone(&pending_write),
                    self.file_transfer_max_entries,
                    self.file_transfer_max_chunk_bytes,
                )
            });
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
            inbound,
            file_list_request: None,
        })
    }
}

impl CliprdrServerFactory for HyprCliprdrFactory {}

/// File-list responses have no request ID. Drain an invalidated exchange before
/// asking for the latest selection, including when that selection is text.
enum FileListRequest {
    Waiting(u64),
    Draining(Option<RemoteSelectionRequest>),
}

struct RemoteSelectionRequest {
    format: ClipboardFormatId,
    file_generation: Option<u64>,
}

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
    inbound: Option<InboundClipboard>,
    file_list_request: Option<FileListRequest>,
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
        drop(self.inbound.take());
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
        if file_stream_enabled(&self.files) {
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
        capabilities: ClipboardGeneralCapabilityFlags,
    ) {
        set_file_capabilities(
            &self.files,
            self.file_transfer_mode.permits_to_client()
                && capabilities.contains(ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED),
            capabilities.contains(ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED),
        );
        if let Some(inbound) = &self.inbound {
            inbound.set_capabilities(
                capabilities.contains(ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED),
                capabilities.contains(ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED),
            );
        }
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        tracing::trace!(
            formats = available_formats.len(),
            "Clipboard: remote clipboard updated"
        );
        self.remote_formats = available_formats.to_vec();
        clear_selection(&self.files);
        let remote_generation = self
            .inbound
            .as_ref()
            .and_then(InboundClipboard::begin_remote_copy);
        let echo_candidate = self
            .echo_candidate
            .lock()
            .ok()
            .and_then(|mut candidate| candidate.take());

        let file_format = self
            .inbound
            .as_ref()
            .filter(|inbound| inbound.enabled())
            .and_then(|_| {
                available_formats
                    .iter()
                    .find(|format| format.name.as_ref() == Some(&ClipboardFormatName::FILE_LIST))
                    .map(|format| format.id)
            });
        let format = file_format.or_else(|| {
            SelectionKind::REMOTE_PREFERENCE
                .into_iter()
                .find_map(|kind| Self::remote_format_for_kind(kind, available_formats))
        });
        let file_generation = file_format.and(remote_generation);

        if self.file_list_request.is_some()
            || (file_format.is_some() && self.last_requested_format.is_some())
        {
            self.file_list_request = Some(FileListRequest::Draining(format.map(|format| {
                RemoteSelectionRequest {
                    format,
                    file_generation,
                }
            })));
            self.pending_echo_candidate = echo_candidate;
            return;
        }

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

        if let Some(ref sender) = self.event_sender {
            if sender
                .send(ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(
                    format,
                )))
                .is_ok()
            {
                self.last_requested_format = Some(format);
                self.file_list_request = file_generation.map(FileListRequest::Waiting);
                self.pending_echo_candidate = echo_candidate;
            }
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
        if let Some(request) = self.file_list_request.take() {
            self.finish_file_list_request(request);
            return;
        }
        self.handle_format_data_response(response, MAX_CLIPBOARD_SIZE);
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        if let Some(worker) = self.to_client_worker() {
            worker.read(request);
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
        if let Some(inbound) = &self.inbound {
            inbound.on_response(response);
        }
    }

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], data_id: Option<u32>) {
        match self.file_list_request.take() {
            Some(FileListRequest::Waiting(generation)) => {
                self.last_requested_format = None;
                self.pending_echo_candidate = None;
                if let Some(inbound) = &self.inbound {
                    inbound.accept_for(generation, files, data_id);
                }
            }
            Some(request @ FileListRequest::Draining(_)) => self.finish_file_list_request(request),
            None => {}
        }
    }

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

impl HyprCliprdrBackend {
    fn finish_file_list_request(&mut self, request: FileListRequest) {
        self.last_requested_format = None;
        let FileListRequest::Draining(Some(RemoteSelectionRequest {
            format,
            file_generation,
        })) = request
        else {
            self.pending_echo_candidate = None;
            return;
        };
        if file_generation.is_some_and(|generation| {
            self.inbound.as_ref().and_then(InboundClipboard::generation) != Some(generation)
        }) {
            self.pending_echo_candidate = None;
            return;
        }
        if let Some(sender) = &self.event_sender {
            if sender
                .send(ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(
                    format,
                )))
                .is_ok()
            {
                self.last_requested_format = Some(format);
                self.file_list_request = file_generation.map(FileListRequest::Waiting);
                return;
            }
        }
        self.pending_echo_candidate = None;
    }

    /// The file worker, but only while this session may copy files to the client.
    fn to_client_worker(&self) -> Option<&FileWorker> {
        self.file_worker
            .as_ref()
            .filter(|_| file_stream_enabled(&self.files))
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

        let running = Arc::clone(&self.running);
        let shared = ClipboardShared {
            event_sender: sender,
            clipboard_data: Arc::clone(&self.clipboard_data),
            clipboard_image: Arc::clone(&self.clipboard_image),
            pending_write: Arc::clone(&self.pending_write),
            echo_candidate: Arc::clone(&self.echo_candidate),
            file_selection: FileSelection::new(
                Arc::clone(&self.files),
                self.to_client_worker().map(|worker| worker.handle()),
            ),
            inbound: self.inbound.as_ref().map(InboundClipboard::handle),
        };

        match thread::Builder::new()
            .name("clipboard-watcher".into())
            .spawn(move || {
                if let Err(e) = clipboard_thread(shared, running) {
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
                file_transfer_mode: FileTransferMode::ToClient,
                files: Arc::new(Mutex::new(None.into())),
                file_worker: None,
                inbound: None,
                file_list_request: None,
            },
            event_rx,
        )
    }

    fn utf16le(text: &str) -> Vec<u8> {
        let mut bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        bytes.extend_from_slice(&[0, 0]);
        bytes
    }

    #[cfg(feature = "client-to-server")]
    fn inbound_backend() -> (HyprCliprdrBackend, mpsc::UnboundedReceiver<ServerEvent>) {
        let (mut backend, receiver) = backend_with_events();
        backend.file_transfer_mode = FileTransferMode::Both;
        backend.inbound = Some(InboundClipboard::new(
            backend.event_sender.as_ref().unwrap().clone(),
            Arc::clone(&backend.pending_write),
            100,
            1024,
        ));
        backend.on_process_negotiated_capabilities(
            ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
        );
        (backend, receiver)
    }

    #[cfg(feature = "client-to-server")]
    fn incoming_file_format(id: u32) -> ClipboardFormat {
        ClipboardFormat::new(ClipboardFormatId::new(id)).with_name(ClipboardFormatName::FILE_LIST)
    }

    #[cfg(feature = "client-to-server")]
    fn take_paste(receiver: &mut mpsc::UnboundedReceiver<ServerEvent>) -> ClipboardFormatId {
        let ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(format)) =
            receiver.try_recv().expect("one paste request")
        else {
            panic!("expected a paste request")
        };
        format
    }

    #[cfg(feature = "client-to-server")]
    #[tokio::test]
    async fn inbound_file_lists_drain_before_requesting_latest_selection() {
        let (mut backend, mut events) = inbound_backend();
        backend.on_remote_copy(&[incoming_file_format(0xc001)]);
        assert_eq!(take_paste(&mut events), ClipboardFormatId::new(0xc001));
        backend.on_remote_copy(&[incoming_file_format(0xc002)]);
        backend.on_remote_copy(&[incoming_file_format(0xc003)]);
        assert!(events.try_recv().is_err());
        backend.on_remote_file_list(&[FileDescriptor::new("old").with_file_size(1)], None);
        assert!(backend.pending_write.lock().unwrap().is_none());
        assert_eq!(take_paste(&mut events), ClipboardFormatId::new(0xc003));
        assert!(events.try_recv().is_err());
        // Failed descriptor responses release the slot so the next copy can retry.
        backend.on_format_data_response(FormatDataResponse::new_error());
        backend.on_remote_copy(&[incoming_file_format(0xc003)]);
        assert_eq!(take_paste(&mut events), ClipboardFormatId::new(0xc003));
    }

    #[cfg(feature = "client-to-server")]
    #[tokio::test]
    async fn inbound_file_list_after_local_owner_change_is_ignored() {
        for deferred in [false, true] {
            let (mut backend, mut events) = inbound_backend();
            backend.on_remote_copy(&[incoming_file_format(0xc001)]);
            take_paste(&mut events);
            if deferred {
                backend.on_remote_copy(&[incoming_file_format(0xc002)]);
            }
            let handle = backend.inbound.as_ref().unwrap().handle();
            handle.invalidate(); // The Wayland local-owner callback's production seam.
            let generation = backend.inbound.as_ref().unwrap().generation();
            backend.on_remote_file_list(&[FileDescriptor::new("stale").with_file_size(1)], None);
            assert_eq!(
                backend.inbound.as_ref().unwrap().generation(),
                generation,
                "a stale descriptor response must not install another selection"
            );
            assert!(
                events.try_recv().is_err(),
                "a deferred stale request must not be sent"
            );
            assert!(backend.file_list_request.is_none());
            assert!(backend.pending_write.lock().unwrap().is_none());
            backend.on_remote_copy(&[incoming_file_format(0xc003)]);
            assert_eq!(take_paste(&mut events), ClipboardFormatId::new(0xc003));
        }
    }

    #[cfg(feature = "client-to-server")]
    #[tokio::test]
    async fn inbound_waits_for_text_response_and_respects_negotiated_streaming() {
        let (mut backend, mut events) = inbound_backend();
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]);
        assert_eq!(take_paste(&mut events), ClipboardFormatId::CF_UNICODETEXT);
        backend.on_remote_copy(&[incoming_file_format(0xc001)]);
        assert!(events.try_recv().is_err());
        backend.on_format_data_response(FormatDataResponse::new_unicode_string("old text"));
        assert!(backend.pending_write.lock().unwrap().is_none());
        assert_eq!(take_paste(&mut events), ClipboardFormatId::new(0xc001));
        backend.on_format_data_response(FormatDataResponse::new_error());
        backend.on_process_negotiated_capabilities(ClipboardGeneralCapabilityFlags::empty());
        backend.on_remote_copy(&[
            incoming_file_format(0xc001),
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
        ]);
        assert_eq!(take_paste(&mut events), ClipboardFormatId::CF_UNICODETEXT);
    }

    #[cfg(feature = "client-to-server")]
    #[tokio::test]
    async fn inbound_public_channel_replay_does_not_publish_superseded_files() {
        use ironrdp_cliprdr::pdu::{
            Capabilities, ClipboardPdu, ClipboardProtocolVersion, FormatList, PackedFileList,
        };
        use ironrdp_cliprdr::CliprdrServer;
        use ironrdp_svc::SvcProcessor;

        // Replay production policy without starting a real compositor connection.
        #[derive(Debug)]
        struct ReplayBackend(HyprCliprdrBackend);
        ironrdp_core::impl_as_any!(ReplayBackend);
        impl CliprdrBackend for ReplayBackend {
            fn temporary_directory(&self) -> &str {
                self.0.temporary_directory()
            }
            fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
                self.0.client_capabilities()
            }
            fn on_ready(&mut self) {}
            fn on_request_format_list(&mut self) {
                self.0.on_request_format_list();
            }
            fn on_process_negotiated_capabilities(
                &mut self,
                flags: ClipboardGeneralCapabilityFlags,
            ) {
                self.0.on_process_negotiated_capabilities(flags);
            }
            fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
                self.0.on_remote_copy(formats);
            }
            fn on_format_data_request(&mut self, request: FormatDataRequest) {
                self.0.on_format_data_request(request);
            }
            fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
                self.0.on_format_data_response(response);
            }
            fn on_remote_file_list(&mut self, files: &[FileDescriptor], data_id: Option<u32>) {
                self.0.on_remote_file_list(files, data_id);
            }
            fn on_file_contents_request(&mut self, request: FileContentsRequest) {
                self.0.on_file_contents_request(request);
            }
            fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
                self.0.on_file_contents_response(response);
            }
            fn on_lock(&mut self, data_id: LockDataId) {
                self.0.on_lock(data_id);
            }
            fn on_unlock(&mut self, data_id: LockDataId) {
                self.0.on_unlock(data_id);
            }
        }

        fn process(channel: &mut CliprdrServer, pdu: ClipboardPdu<'_>) {
            channel
                .process(&ironrdp_core::encode_vec(&pdu).unwrap())
                .unwrap();
        }
        let (backend, mut events) = inbound_backend();
        let pending = Arc::clone(&backend.pending_write);
        let flags = backend.client_capabilities();
        assert!(!flags.contains(ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA));
        let mut channel = CliprdrServer::new(Box::new(ReplayBackend(backend)));
        channel.start().unwrap();
        process(
            &mut channel,
            ClipboardPdu::Capabilities(Capabilities::new(ClipboardProtocolVersion::V2, flags)),
        );
        let files = [incoming_file_format(0xc001)];
        process(
            &mut channel,
            ClipboardPdu::FormatList(FormatList::new_unicode(&files, true).unwrap()),
        );
        channel.initiate_paste(take_paste(&mut events)).unwrap();
        let text = [ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
        process(
            &mut channel,
            ClipboardPdu::FormatList(FormatList::new_unicode(&text, true).unwrap()),
        );
        assert!(events.try_recv().is_err());
        process(
            &mut channel,
            ClipboardPdu::FormatDataResponse(
                FormatDataResponse::new_file_list(&PackedFileList {
                    files: vec![FileDescriptor::new("superseded").with_file_size(1)],
                })
                .unwrap(),
            ),
        );
        assert!(pending.lock().unwrap().is_none());
        assert_eq!(take_paste(&mut events), ClipboardFormatId::CF_UNICODETEXT);
        channel
            .initiate_paste(ClipboardFormatId::CF_UNICODETEXT)
            .unwrap();
        process(
            &mut channel,
            ClipboardPdu::FormatDataResponse(FormatDataResponse::new_unicode_string("latest")),
        );
        assert!(
            matches!(pending.lock().unwrap().as_ref(), Some(PendingWrite::Text(bytes)) if bytes == b"latest")
        );
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
            file_transfer_mode: FileTransferMode::ToClient,
            files,
            file_worker: Some(worker),
            inbound: None,
            file_list_request: None,
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
        assert!(
            FileSelection::new(Arc::clone(&backend.files), Some(worker.handle()))
                .freeze(vec![root.clone()])
        );

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
        assert!(
            FileSelection::new(Arc::clone(&backend.files), Some(worker.handle()))
                .freeze(vec![root.clone()])
        );
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
            _ => panic!("expected text pending write"),
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
            _ => panic!("expected existing text pending write"),
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

    #[test]
    fn negotiated_file_policy_preserves_text_and_respects_config_off() {
        let (mut backend, mut events) = backend_with_events();
        *backend.clipboard_data.lock().unwrap() = Some(b"still text".to_vec());
        backend.on_process_negotiated_capabilities(
            ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES,
        );
        backend.on_request_format_list();
        let Some(ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(formats))) =
            events.blocking_recv()
        else {
            panic!("text offer missing");
        };
        assert_eq!(formats[0].id, ClipboardFormatId::CF_UNICODETEXT);
        assert!(events.try_recv().is_err());
        let full = ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED;
        backend.on_process_negotiated_capabilities(full);
        assert!(file_stream_enabled(&backend.files));
        backend.file_transfer_mode = FileTransferMode::Off;
        backend.on_process_negotiated_capabilities(full);
        assert!(!file_stream_enabled(&backend.files));
    }

    #[test]
    fn peer_without_file_streaming_does_not_get_file_offer() {
        let (mut backend, mut events) = backend_with_events();
        backend.on_process_negotiated_capabilities(
            ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES,
        );
        let path =
            std::env::temp_dir().join(format!("hypr-rdp-review-caps-{}", std::process::id()));
        std::fs::write(&path, b"file").unwrap();
        backend.files.lock().unwrap().entries = Some(super::super::files::freeze_regular_files(
            vec![path.clone()],
        ));
        backend.on_request_format_list();
        let event = events.try_recv();
        std::fs::remove_file(path).unwrap();
        assert!(
            !matches!(
                event,
                Ok(ServerEvent::Clipboard(
                    ClipboardMessage::SendInitiateFileCopy(_)
                ))
            ),
            "file offer emitted although the peer negotiated text/image only"
        );
    }
}
