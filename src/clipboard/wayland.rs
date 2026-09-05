use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ironrdp_cliprdr::pdu::{ClipboardFormat, ClipboardFormatId};
use ironrdp_server::ServerEvent;
use tokio::sync::mpsc;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

use super::backend::{announce_local_formats, ClipboardEchoCandidate};
use super::files::{uri_list_paths, FileSelection};
use super::formats::{
    PendingWrite, SelectionKind, FILE_URI_LIST_MIME, GNOME_COPIED_FILES_MIME, IMAGE_PNG_MIME,
    MAX_CLIPBOARD_SIZE, TEXT_MIME, TEXT_PLAIN_MIME, UTF8_MIME,
};
use super::remote::RemoteFiles;

const DATA_CONTROL_VERSION: u32 = 1;

fn data_control_manager_version(advertised_version: u32) -> u32 {
    advertised_version.min(DATA_CONTROL_VERSION)
}

pub(super) fn clipboard_thread(
    event_sender: mpsc::UnboundedSender<ServerEvent>,
    clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
    clipboard_image: Arc<Mutex<Option<Vec<u8>>>>,
    pending_write: Arc<Mutex<Option<PendingWrite>>>,
    echo_candidate: Arc<Mutex<Option<ClipboardEchoCandidate>>>,
    running: Arc<AtomicBool>,
    file_selection: FileSelection,
    remote_files: Option<RemoteFiles>,
) -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow::anyhow!("clipboard: failed to connect to Wayland: {}", e))?;
    let mut event_queue = conn.new_event_queue::<ClipState>();
    let qh = event_queue.handle();

    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = ClipState::new(
        event_sender,
        clipboard_data,
        clipboard_image,
        pending_write,
        echo_candidate,
        file_selection,
        remote_files,
    );

    let wayland_fd = conn.as_fd().as_raw_fd();
    let ready = dispatch_until_globals_ready(
        &conn,
        &mut event_queue,
        &mut state,
        wayland_fd,
        Instant::now() + COMPOSITOR_REPLY_TIMEOUT,
    )?;

    if !ready {
        let mut missing = Vec::new();
        if state.manager.is_none() {
            missing.push("zwlr_data_control_manager_v1");
        }
        if state.seat.is_none() {
            missing.push("wl_seat");
        }
        anyhow::bail!(
            "clipboard: {} not advertised within {:?}",
            missing.join(" and "),
            COMPOSITOR_REPLY_TIMEOUT
        );
    }

    let manager = state.manager.as_ref().expect("checked above").clone();

    let seat = state.seat.as_ref().expect("checked above").clone();

    let device = manager.get_data_device(&seat, &qh, ());
    state.device = Some(device);

    tracing::info!("Clipboard: wlr-data-control-v1 device bound");

    loop {
        if !running.load(Ordering::Relaxed) {
            break;
        }

        // Dispatch all pending events
        loop {
            let n = event_queue
                .dispatch_pending(&mut state)
                .map_err(|e| anyhow::anyhow!("clipboard: dispatch_pending failed: {}", e))?;
            if n == 0 {
                break;
            }
        }
        conn.flush()
            .map_err(|e| anyhow::anyhow!("clipboard: flush failed: {}", e))?;

        // RDP → Wayland: pick up pending_write and set selection
        if let Some(pending) = state.take_pending_write(Instant::now()) {
            // Destroy previous source to prevent protocol object leak
            if let Some(old) = state.active_source.take() {
                old.destroy();
            }
            let source = manager.create_data_source(&qh, ());
            match &pending {
                PendingWrite::Text(data) => {
                    tracing::trace!(len = data.len(), "Clipboard: writing text to Wayland");
                    source.offer(TEXT_MIME.to_string());
                    source.offer(UTF8_MIME.to_string());
                    source.offer(TEXT_PLAIN_MIME.to_string());
                }
                PendingWrite::Image(data) => {
                    tracing::trace!(len = data.len(), "Clipboard: writing image to Wayland");
                    source.offer(IMAGE_PNG_MIME.to_string());
                }
                PendingWrite::Files { uri_list, .. } => {
                    tracing::trace!(len = uri_list.len(), "Clipboard: writing files to Wayland");
                    source.offer(FILE_URI_LIST_MIME.to_string());
                    source.offer(GNOME_COPIED_FILES_MIME.to_string());
                }
            }
            state.active_selection = Some(ActiveSelection {
                kind: pending.kind(),
                pending,
            });

            if let Some(dev) = state.device.as_ref() {
                dev.set_selection(Some(&source));
            }
            state.active_source = Some(source);
            // The event loop handles the resulting Selection notification.
            conn.flush().map_err(|e| {
                anyhow::anyhow!("clipboard: flush after set_selection failed: {}", e)
            })?;
        }

        let guard = match event_queue.prepare_read() {
            Some(g) => g,
            None => continue,
        };

        if poll_wayland_fd(wayland_fd, WAYLAND_POLL_INTERVAL)? {
            guard
                .read()
                .map_err(|e| anyhow::anyhow!("clipboard: read failed: {}", e))?;
        } else {
            drop(guard);
        }
    }

    Ok(())
}

struct ClipState {
    event_sender: mpsc::UnboundedSender<ServerEvent>,
    /// Deadline for Selection events caused by our own write.
    suppress_echo_until: Option<Instant>,
    clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
    clipboard_image: Arc<Mutex<Option<Vec<u8>>>>,
    pending_write: Arc<Mutex<Option<PendingWrite>>>,
    echo_candidate: Arc<Mutex<Option<ClipboardEchoCandidate>>>,
    file_selection: FileSelection,
    remote_files: Option<RemoteFiles>,
    manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    device: Option<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1>,
    offer_mimes: HashMap<ObjectId, Vec<String>>,
    active_selection: Option<ActiveSelection>,
    /// Currently active data source; destroyed when replaced to avoid protocol object leak.
    active_source: Option<zwlr_data_control_source_v1::ZwlrDataControlSourceV1>,
}

struct ActiveSelection {
    kind: SelectionKind,
    pending: PendingWrite,
}

impl ClipState {
    fn new(
        event_sender: mpsc::UnboundedSender<ServerEvent>,
        clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
        clipboard_image: Arc<Mutex<Option<Vec<u8>>>>,
        pending_write: Arc<Mutex<Option<PendingWrite>>>,
        echo_candidate: Arc<Mutex<Option<ClipboardEchoCandidate>>>,
        file_selection: FileSelection,
        remote_files: Option<RemoteFiles>,
    ) -> Self {
        Self {
            event_sender,
            suppress_echo_until: None,
            clipboard_data,
            clipboard_image,
            pending_write,
            echo_candidate,
            file_selection,
            remote_files,
            manager: None,
            seat: None,
            device: None,
            offer_mimes: HashMap::new(),
            active_selection: None,
            active_source: None,
        }
    }

    /// Forget every cached format, so nothing from the previous selection stays
    /// advertisable after the compositor has replaced it.
    fn clear_cached_selection(&self) {
        if let Ok(mut data) = self.clipboard_data.lock() {
            *data = None;
        }
        if let Ok(mut image) = self.clipboard_image.lock() {
            *image = None;
        }
        self.file_selection.clear();
    }

    fn local_owner_changed(&self) {
        self.file_selection.clear();
        if let Some(remote_files) = &self.remote_files {
            remote_files.cancel_pending();
        }
    }

    /// Take one pending write and arm suppression before replacing its source.
    fn take_pending_write(&mut self, now: Instant) -> Option<PendingWrite> {
        let pending = self.pending_write.lock().ok().and_then(|mut g| g.take())?;
        self.suppress_echo_until = Some(now + ECHO_SUPPRESSION_TIMEOUT);
        Some(pending)
    }

    /// Suppress the compositor response burst, then accept later selections.
    fn selection_is_our_echo(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.suppress_echo_until else {
            return false;
        };
        if now >= deadline {
            self.suppress_echo_until = None;
            return false;
        }

        self.suppress_echo_until = Some(deadline.min(now + ECHO_SUPPRESSION_GRACE));
        true
    }
}

/// Dispatch initial registry events without an unbounded Wayland round trip.
fn dispatch_until_globals_ready(
    conn: &Connection,
    event_queue: &mut EventQueue<ClipState>,
    state: &mut ClipState,
    wayland_fd: RawFd,
    deadline: Instant,
) -> anyhow::Result<bool> {
    loop {
        loop {
            let n = event_queue
                .dispatch_pending(state)
                .map_err(|e| anyhow::anyhow!("clipboard: dispatch_pending failed: {}", e))?;
            if n == 0 {
                break;
            }
        }
        if state.manager.is_some() && state.seat.is_some() {
            return Ok(true);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        conn.flush()
            .map_err(|e| anyhow::anyhow!("clipboard: flush failed: {}", e))?;
        let Some(guard) = event_queue.prepare_read() else {
            continue;
        };
        if poll_wayland_fd(wayland_fd, remaining)? {
            guard
                .read()
                .map_err(|e| anyhow::anyhow!("clipboard: read failed: {}", e))?;
        } else {
            drop(guard);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClipState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "zwlr_data_control_manager_v1" => {
                    // Primary-selection support starts at v2. Stay on v1 until
                    // this backend handles its offer lifecycle.
                    state.manager =
                        Some(registry.bind(name, data_control_manager_version(version), qh, ()));
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(1), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for ClipState {
    wayland_client::event_created_child!(ClipState, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        0 => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
    ]);

    fn event(
        state: &mut Self,
        _proxy: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.offer_mimes.insert(id.id(), Vec::new());
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                if state.selection_is_our_echo(Instant::now()) {
                    if let Some(offer) = id {
                        state.offer_mimes.remove(&offer.id());
                        offer.destroy();
                    }
                    return;
                }

                state.local_owner_changed();
                if let Ok(mut candidate) = state.echo_candidate.lock() {
                    *candidate = None;
                }

                let offer = match id {
                    Some(offer) => offer,
                    None => {
                        state.clear_cached_selection();
                        return;
                    }
                };

                let offer_id = offer.id();
                let mimes = state.offer_mimes.remove(&offer_id);
                let mimes = match mimes {
                    Some(m) => m,
                    None => {
                        offer.destroy();
                        return;
                    }
                };

                let text_mime = SelectionKind::Text.offered_wayland_mime(&mimes);
                let image_mime = SelectionKind::Image.offered_wayland_mime(&mimes);
                let files_mime = SelectionKind::Files.offered_wayland_mime(&mimes);

                if text_mime.is_none() && image_mime.is_none() && files_mime.is_none() {
                    // No supported MIME — clear stale caches
                    state.clear_cached_selection();
                    offer.destroy();
                    return;
                }

                let mut formats = Vec::new();

                // Clear stale caches for formats NOT present in the new selection.
                // Without this, switching image→text or text→image leaves the
                // previous format cached and re-advertisable to the RDP client.
                if text_mime.is_none() {
                    if let Ok(mut g) = state.clipboard_data.lock() {
                        *g = None;
                    }
                }
                if image_mime.is_none() {
                    if let Ok(mut g) = state.clipboard_image.lock() {
                        *g = None;
                    }
                }

                if let Some(ref mime) = text_mime {
                    if let Some(data) = read_offer_data(&offer, mime, conn) {
                        if !data.is_empty() {
                            tracing::trace!(len = data.len(), "Clipboard: read text data");
                            if let Ok(mut g) = state.clipboard_data.lock() {
                                *g = Some(data);
                            }
                            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
                        }
                    }
                }

                if let Some(ref mime) = image_mime {
                    if let Some(png_data) = read_offer_data(&offer, mime, conn) {
                        if !png_data.is_empty() {
                            // Convert PNG to CF_DIB for RDP clients
                            match ironrdp_cliprdr_format::bitmap::png_to_cf_dib(&png_data) {
                                Ok(dib_data) => {
                                    tracing::trace!(
                                        png_len = png_data.len(),
                                        dib_len = dib_data.len(),
                                        "Clipboard: converted PNG to CF_DIB"
                                    );
                                    if let Ok(mut g) = state.clipboard_image.lock() {
                                        *g = Some(dib_data);
                                    }
                                    formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Clipboard: PNG to DIB conversion failed: {}",
                                        e
                                    );
                                }
                            }
                        }
                    }
                }

                if let Some(ref mime) = files_mime {
                    if let Some(uri_list) = read_offer_data(&offer, mime, conn) {
                        if !state.file_selection.freeze(uri_list_paths(&uri_list))
                            && text_mime.is_none()
                            && !uri_list.is_empty()
                        {
                            // URI lists without file entries remain ordinary text clipboard data.
                            if let Ok(mut data) = state.clipboard_data.lock() {
                                *data = Some(uri_list);
                            }
                            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
                        }
                    }
                }

                offer.destroy();

                if !formats.is_empty() {
                    announce_local_formats(
                        &state.event_sender,
                        &state.echo_candidate,
                        &state.clipboard_data,
                        &state.clipboard_image,
                        formats,
                    );
                }
            }
            zwlr_data_control_device_v1::Event::Finished => {
                tracing::warn!("Clipboard: data control device finished");
                state.device = None;
            }
            _ => {}
        }
    }
}

const WAYLAND_POLL_INTERVAL: Duration = Duration::from_millis(100);
const COMPOSITOR_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
const ECHO_SUPPRESSION_TIMEOUT: Duration = Duration::from_secs(2);
const ECHO_SUPPRESSION_GRACE: Duration = Duration::from_millis(100);

/// No-progress and overall deadlines for clipboard pipe transfers.
const PIPE_STALL_TIMEOUT: Duration = Duration::from_secs(2);
const PIPE_TOTAL_TIMEOUT: Duration = Duration::from_secs(20);

fn poll_wayland_fd(fd: RawFd, timeout: Duration) -> std::io::Result<bool> {
    let deadline = Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }

        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = remaining.as_millis().max(1).min(i32::MAX as u128) as libc::c_int;

        match unsafe { libc::poll(&mut pollfd, 1, timeout_ms) } {
            0 => return Ok(false),
            n if n > 0 => {
                if pollfd.revents & libc::POLLNVAL != 0 {
                    return Err(std::io::Error::from_raw_os_error(libc::EBADF));
                }
                return Ok(pollfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0);
            }
            _ => {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

fn pipe_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn set_nonblocking(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn poll_fd(fd: std::os::fd::RawFd, events: libc::c_short) -> std::io::Result<bool> {
    let mut pollfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    match unsafe { libc::poll(&mut pollfd, 1, 100) } {
        0 => Ok(false),
        n if n > 0 => Ok(true),
        _ => {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            }
        }
    }
}

fn transfer_timed_out(
    started: std::time::Instant,
    last_progress: std::time::Instant,
    stall: Duration,
    total: Duration,
) -> bool {
    started.elapsed() >= total || last_progress.elapsed() >= stall
}

/// Read up to `max_size` bytes from a pipe without unbounded blocking: the
/// fd is switched to non-blocking and the loop aborts when no byte arrives
/// within `stall` or the transfer exceeds `total`. Returns Ok(None) when
/// the payload exceeds `max_size`.
fn read_pipe_bounded(
    fd: &OwnedFd,
    max_size: usize,
    stall: Duration,
    total: Duration,
) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;

    set_nonblocking(fd.as_raw_fd())?;
    let started = std::time::Instant::now();
    let mut last_progress = started;
    let mut data = Vec::new();
    let mut chunk = [0u8; 65536];
    let mut file = std::fs::File::from(fd.try_clone()?);
    loop {
        if transfer_timed_out(started, last_progress, stall, total) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "clipboard transfer stalled",
            ));
        }
        if !poll_fd(fd.as_raw_fd(), libc::POLLIN)? {
            continue;
        }
        match file.read(&mut chunk) {
            Ok(0) => return Ok(Some(data)),
            Ok(n) => {
                if data.len().saturating_add(n) > max_size {
                    return Ok(None);
                }
                data.extend_from_slice(&chunk[..n]);
                last_progress = std::time::Instant::now();
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Write all of `data` into a pipe without unbounded blocking, with the
/// same deadline discipline as [`read_pipe_bounded`].
fn write_pipe_bounded(
    fd: &OwnedFd,
    data: &[u8],
    stall: Duration,
    total: Duration,
) -> std::io::Result<()> {
    use std::io::Write;

    set_nonblocking(fd.as_raw_fd())?;
    let started = std::time::Instant::now();
    let mut last_progress = started;
    let mut written = 0;
    let mut file = std::fs::File::from(fd.try_clone()?);
    while written < data.len() {
        if transfer_timed_out(started, last_progress, stall, total) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "clipboard transfer stalled",
            ));
        }
        if !poll_fd(fd.as_raw_fd(), libc::POLLOUT)? {
            continue;
        }
        match file.write(&data[written..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "clipboard peer stopped reading",
                ))
            }
            Ok(n) => {
                written += n;
                last_progress = std::time::Instant::now();
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Read data from a clipboard offer via pipe.
fn read_offer_data(
    offer: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    mime: &str,
    conn: &Connection,
) -> Option<Vec<u8>> {
    let (read_fd, write_fd) = match pipe_cloexec() {
        Ok(fds) => fds,
        Err(e) => {
            tracing::warn!("Clipboard: failed to create pipe: {}", e);
            return None;
        }
    };

    offer.receive(mime.to_string(), write_fd.as_fd());
    let _ = conn.flush();
    drop(write_fd);

    match read_pipe_bounded(
        &read_fd,
        MAX_CLIPBOARD_SIZE,
        PIPE_STALL_TIMEOUT,
        PIPE_TOTAL_TIMEOUT,
    ) {
        Ok(Some(data)) => Some(data),
        Ok(None) => {
            tracing::warn!(
                max = MAX_CLIPBOARD_SIZE,
                "Clipboard: offer data too large, ignoring"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                "Clipboard: failed to read offer data (mime={}): {}",
                mime,
                e
            );
            None
        }
    }
}

impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        proxy: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            if let Some(mimes) = state.offer_mimes.get_mut(&proxy.id()) {
                mimes.push(mime_type);
            }
        }
    }
}

impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        proxy: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                let selection = state.active_selection.as_ref();

                let should_send = match selection {
                    Some(selection) => selection.kind.accepts_wayland_mime(&mime_type),
                    None => {
                        tracing::debug!(%mime_type, "Clipboard: ignoring send without selection kind");
                        false
                    }
                };

                if should_send {
                    if let Some(selection) = selection {
                        if let Some(data) = selection.pending.data_for_mime(&mime_type) {
                            write_source_data(&fd, data);
                        }
                    }
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                // Protocol requires destroying the source after cancellation
                proxy.destroy();
                if state
                    .active_source
                    .as_ref()
                    .is_some_and(|s| s.id() == proxy.id())
                {
                    state.active_source = None;
                    state.active_selection = None;
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(ClipState: ignore wl_seat::WlSeat);
delegate_noop!(ClipState: ignore zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);

fn write_source_data(fd: &OwnedFd, data: &[u8]) -> bool {
    match write_pipe_bounded(fd, data, PIPE_STALL_TIMEOUT, PIPE_TOTAL_TIMEOUT) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("Clipboard: failed to write source data: {}", e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    const FAST_STALL: Duration = Duration::from_millis(300);
    const FAST_TOTAL: Duration = Duration::from_secs(3);

    fn clip_state() -> ClipState {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        ClipState::new(
            event_tx,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            FileSelection::new(Arc::default(), None),
            None,
        )
    }

    #[tokio::test]
    async fn a_local_owner_change_cancels_a_pending_client_read() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let remote = RemoteFiles::new(events, 100);
        let read = remote.read(0, 0, 1);
        receiver.recv().await.expect("remote range was requested");
        let mut state = clip_state();
        state.remote_files = Some(remote);
        state.local_owner_changed();
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(100), read).await,
            Ok(Ok(Err(())))
        ));
    }

    #[test]
    fn taking_the_rdp_write_arms_echo_suppression() {
        let now = Instant::now();

        for pending in [
            PendingWrite::Text(b"pasted from the client".to_vec()),
            PendingWrite::Image(vec![0u8; 16]),
        ] {
            let mut state = clip_state();
            assert!(state.take_pending_write(now).is_none());
            assert!(!state.selection_is_our_echo(now));

            *state.pending_write.lock().unwrap() = Some(pending);

            assert!(state.take_pending_write(now).is_some());
            assert_eq!(
                state.suppress_echo_until,
                Some(now + ECHO_SUPPRESSION_TIMEOUT)
            );
            assert!(state.pending_write.lock().unwrap().is_none());
        }
    }

    #[test]
    fn compositor_response_burst_is_suppressed_then_expires() {
        let mut state = clip_state();
        let started = Instant::now();
        *state.pending_write.lock().unwrap() = Some(PendingWrite::Text(vec![1]));
        state.take_pending_write(started).unwrap();

        let first = started + Duration::from_millis(10);
        assert!(state.selection_is_our_echo(first));
        let grace_deadline = first + ECHO_SUPPRESSION_GRACE;
        assert_eq!(state.suppress_echo_until, Some(grace_deadline));

        assert!(state.selection_is_our_echo(first + Duration::from_millis(1)));
        assert_eq!(state.suppress_echo_until, Some(grace_deadline));

        assert!(!state.selection_is_our_echo(grace_deadline));
        assert_eq!(state.suppress_echo_until, None);
    }

    #[test]
    fn wayland_poll_returns_when_the_fd_stays_unreadable() {
        let (read_fd, write_fd) = pipe_pair();
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let poller = std::thread::spawn(move || {
            result_tx
                .send(poll_wayland_fd(
                    read_fd.as_raw_fd(),
                    Duration::from_millis(50),
                ))
                .unwrap();
        });

        let result = result_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("Wayland poll exceeded its deadline");
        drop(write_fd);
        poller.join().unwrap();

        assert!(!result.unwrap());
    }

    #[test]
    fn wayland_poll_rejects_an_invalid_fd() {
        let error = poll_wayland_fd(i32::MAX, Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }

    fn pipe_pair() -> (OwnedFd, OwnedFd) {
        pipe_cloexec().unwrap()
    }

    #[test]
    fn clipboard_pipe_ends_are_close_on_exec() {
        let (read_fd, write_fd) = pipe_pair();

        for fd in [&read_fd, &write_fd] {
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
    }

    #[test]
    fn transfer_timeout_enforces_total_duration_despite_recent_progress() {
        let now = std::time::Instant::now();

        assert!(transfer_timed_out(
            now - Duration::from_secs(2),
            now,
            Duration::from_secs(10),
            Duration::from_secs(1),
        ));
    }

    #[test]
    fn pipe_read_returns_data_when_the_writer_closes() {
        let (read_fd, write_fd) = pipe_pair();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::File::from(write_fd);
            file.write_all(b"clipboard payload").unwrap();
        });

        let data = read_pipe_bounded(&read_fd, 1024, FAST_STALL, FAST_TOTAL)
            .unwrap()
            .unwrap();

        assert_eq!(data, b"clipboard payload");
        writer.join().unwrap();
    }

    #[test]
    fn pipe_read_times_out_when_the_writer_stalls_mid_transfer() {
        // A source that writes part of the payload and never closes used to
        // block read_to_end forever and wedge the watcher thread.
        let (read_fd, write_fd) = pipe_pair();
        let mut file = std::fs::File::from(write_fd.try_clone().unwrap());
        file.write_all(b"partial").unwrap();

        let error = read_pipe_bounded(&read_fd, 1024, FAST_STALL, FAST_TOTAL).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        drop(write_fd);
    }

    #[test]
    fn pipe_read_rejects_oversized_payloads() {
        let (read_fd, write_fd) = pipe_pair();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::File::from(write_fd);
            file.write_all(&[0u8; 2048]).unwrap();
        });

        assert!(read_pipe_bounded(&read_fd, 1024, FAST_STALL, FAST_TOTAL)
            .unwrap()
            .is_none());
        writer.join().unwrap();
    }

    #[test]
    fn pipe_read_survives_a_slow_but_progressing_writer() {
        let (read_fd, write_fd) = pipe_pair();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::File::from(write_fd);
            for chunk in [b"slow".as_slice(), b"-", b"drip"] {
                file.write_all(chunk).unwrap();
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        let data = read_pipe_bounded(&read_fd, 1024, FAST_STALL, FAST_TOTAL)
            .unwrap()
            .unwrap();

        assert_eq!(data, b"slow-drip");
        writer.join().unwrap();
    }

    #[test]
    fn pipe_write_completes_when_the_reader_drains() {
        let (read_fd, write_fd) = pipe_pair();
        let payload = vec![7u8; 256 * 1024]; // larger than the default pipe buffer
        let reader = std::thread::spawn(move || {
            let mut file = std::fs::File::from(read_fd);
            let mut out = Vec::new();
            file.read_to_end(&mut out).unwrap();
            out
        });

        write_pipe_bounded(&write_fd, &payload, FAST_STALL, FAST_TOTAL).unwrap();
        drop(write_fd);

        assert_eq!(reader.join().unwrap(), payload);
    }

    #[test]
    fn pipe_write_times_out_when_the_reader_stops_reading() {
        // A paste target that stops draining its pipe used to block
        // write_all forever once the payload exceeded the pipe buffer.
        let (read_fd, write_fd) = pipe_pair();
        let payload = vec![7u8; 256 * 1024];

        let error = write_pipe_bounded(&write_fd, &payload, FAST_STALL, FAST_TOTAL).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        drop(read_fd);
    }

    #[test]
    fn pipe_write_fails_fast_when_the_reader_is_gone() {
        let (read_fd, write_fd) = pipe_pair();
        drop(read_fd);

        assert!(write_pipe_bounded(&write_fd, b"clipboard", FAST_STALL, FAST_TOTAL).is_err());
    }

    #[test]
    fn source_data_writer_reports_both_outcomes() {
        let (read_fd, write_fd) = pipe_pair();
        let reader = std::thread::spawn(move || {
            let mut file = std::fs::File::from(read_fd);
            let mut out = Vec::new();
            file.read_to_end(&mut out).unwrap();
            out
        });
        assert!(write_source_data(&write_fd, b"clipboard"));
        drop(write_fd);
        assert_eq!(reader.join().unwrap(), b"clipboard");

        let (read_fd, write_fd) = pipe_pair();
        drop(read_fd);
        assert!(!write_source_data(&write_fd, b"clipboard"));
    }

    #[test]
    fn data_control_manager_version_is_limited_to_used_v1_surface() {
        assert_eq!(data_control_manager_version(1), 1);
        assert_eq!(data_control_manager_version(2), 1);
        assert_eq!(data_control_manager_version(u32::MAX), 1);
    }
}
