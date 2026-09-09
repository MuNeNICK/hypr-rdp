use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ironrdp_server::{
    ConnectionHandler, ConnectionInfo, Credentials, PostConnectionAction, RdpServer, ServerError,
    SoundServerFactory, TlsIdentityCtx,
};

use crate::audio::{AudioMode, HyprSoundFactory};
use crate::capture::{HyprDisplay, HyprDisplayHandle};
use crate::clipboard::HyprCliprdrFactory;
use crate::config::{ConfigCredentials, RuntimeConfig};
use crate::egfx::{EgfxShared, HyprGfxFactory};
use crate::input::{HyprInputHandler, RdpInputSessionSink, SharedOutputLayout};

mod session_hooks;
mod tls;

use session_hooks::{session_hooks_from_config, SharedSessionHooks};

pub struct ServerContext {
    server: RdpServer,
    addr: SocketAddr,
    session_hooks: Option<SharedSessionHooks>,
    /// Shared with the connection handler: the accept loop below owns the
    /// session-end boundary, and releasing held keys is part of it.
    input_session_sink: Arc<dyn RdpInputSessionSink>,
    pub display_handle: HyprDisplayHandle,
}

pub async fn setup(config: RuntimeConfig) -> Result<ServerContext> {
    let hyprland_instance =
        crate::hyprland::initialize().context("failed to select the Hyprland instance")?;
    let RuntimeConfig {
        bind,
        cert,
        key,
        credentials,
        resolution,
        headless_scale,
        capture_mode,
        bitrate,
        quality,
        rate_control,
        fps,
        max_frames_in_flight,
        egfx_codec,
        keyboard_layout_policy,
        audio_mode,
        h264_backend,
        resolution_fixed,
        output,
        on_session_start,
        on_session_end,
        file_transfer_mode,
        file_transfer_max_chunk_bytes,
        file_transfer_max_entries,
    } = config;

    let egfx_shared = Arc::new(EgfxShared::with_codec_policy(
        max_frames_in_flight,
        egfx_codec,
    ));
    let output_layout = Arc::new(SharedOutputLayout::new());

    let (display, display_handle, (rdp_width, rdp_height)) = HyprDisplay::new(
        resolution,
        headless_scale,
        capture_mode,
        Arc::clone(&egfx_shared),
        Arc::clone(&output_layout),
        bitrate,
        quality,
        rate_control,
        fps,
        h264_backend,
        resolution_fixed,
        output,
    )
    .await
    .context("failed to initialize display capture")?;
    egfx_shared.set_surface_size(rdp_width, rdp_height);
    let input_handler =
        HyprInputHandler::new(rdp_width, rdp_height, output_layout, keyboard_layout_policy)
            .context("failed to initialize input handler")?;
    let input_session_sink = input_handler
        .rdp_input_session_handle()
        .context("input handler has no command channel")?;
    let input_session_sink: Arc<dyn RdpInputSessionSink> = Arc::new(input_session_sink);

    let gfx_factory = HyprGfxFactory::new(Arc::clone(&egfx_shared));
    let cliprdr_factory = HyprCliprdrFactory::new(
        file_transfer_mode,
        file_transfer_max_chunk_bytes,
        file_transfer_max_entries,
    );
    let sound_factory = sound_factory_for_audio_mode(audio_mode);
    let session_hooks =
        session_hooks_from_config(on_session_start, on_session_end, Some(hyprland_instance));

    let builder = RdpServer::builder().with_addr(bind);

    let (cert_path, key_path) = tls::resolve_tls_paths(cert.as_deref(), key.as_deref())?;

    let tls_ctx = TlsIdentityCtx::init_from_paths(Path::new(&cert_path), Path::new(&key_path))
        .context("failed to load TLS certificates")?;
    let acceptor = tls_ctx
        .make_acceptor()
        .context("failed to create TLS acceptor")?;

    let credentials = ironrdp_credentials(credentials);
    let secured_builder = match security_mode_for_credentials(&credentials) {
        ServerSecurityMode::Tls => builder.with_tls(acceptor),
        ServerSecurityMode::Hybrid => builder.with_hybrid(acceptor, tls_ctx.pub_key),
    };

    let mut server = secured_builder
        .with_input_handler(input_handler)
        .with_display_handler(display)
        .with_connection_handler(Some(Box::new(ClientConnectionHandler::new(
            Arc::clone(&input_session_sink),
            session_hooks.clone(),
        ))))
        .with_gfx_factory(Some(Box::new(gfx_factory)))
        .with_cliprdr_factory(Some(Box::new(cliprdr_factory)))
        .with_sound_factory(sound_factory)
        .build();

    server.set_credentials(credentials);

    tracing::info!("RDP server configured for {}", bind);

    Ok(ServerContext {
        server,
        addr: bind,
        session_hooks,
        input_session_sink,
        display_handle,
    })
}

fn sound_factory_for_audio_mode(audio_mode: AudioMode) -> Option<Box<dyn SoundServerFactory>> {
    match audio_mode {
        AudioMode::Mirror | AudioMode::Redirect => {
            Some(Box::new(HyprSoundFactory::new(audio_mode)))
        }
        AudioMode::Off => None,
    }
}

pub async fn serve(ctx: &mut ServerContext) -> Result<()> {
    let listener = bind_listener(ctx.addr)?;
    tracing::info!("Listening for RDP connections on {}", ctx.addr);
    serve_on(
        listener,
        &mut ctx.server,
        ctx.session_hooks.as_ref(),
        ctx.input_session_sink.as_ref(),
    )
    .await
}

fn server_run_error(error: ServerError) -> anyhow::Error {
    anyhow::Error::new(error)
}

/// Accept connections and serve one session at a time, closing any extra
/// connection immediately instead of leaving it to hang in the backlog.
///
/// `RdpServer::run()` accepts serially, so while a session runs a second
/// client sits unanswered until the first ends and appears to hang (issue #8).
async fn serve_on(
    listener: tokio::net::TcpListener,
    server: &mut RdpServer,
    session_hooks: Option<&SharedSessionHooks>,
    input_session_sink: &dyn RdpInputSessionSink,
) -> Result<()> {
    loop {
        let (stream, peer) = match accept_session(&listener).await {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!("Accept failed: {:#}", error);
                tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                continue;
            }
        };
        tracing::info!(%peer, "RDP connection accepted");

        let mut session = std::pin::pin!(server.run_connection(stream));
        let result = loop {
            tokio::select! {
                // Session first, so a client reconnecting the instant a session
                // ends is served by the outer loop rather than bounced here.
                biased;
                result = &mut session => break result,
                extra = listener.accept() => match extra {
                    Ok((extra_stream, extra_peer)) => {
                        tracing::debug!(peer = %extra_peer, "Session active; rejecting connection");
                        drop(extra_stream);
                    }
                    Err(err) => {
                        // A resource limit leaves the socket queued and the
                        // listener readable, so retrying at once would spin.
                        tracing::warn!("Accept failed while a session is active: {}", err);
                        tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    }
                },
            }
        };
        // This loop stands in for `RdpServer::run`, the only caller of
        // `on_disconnected`, so the session-end boundary is ours: release the
        // keys the session held and run the end hook. Both are no-ops for a
        // connection that never established a session. (IronRDP releases the
        // session's static channels itself on the way out of
        // `run_connection_with`, so that boundary needs nothing here.)
        input_session_sink.session_ended();
        if let Some(hooks) = session_hooks {
            hooks.session_ended();
        }
        if let Err(err) = result {
            tracing::error!("Connection error: {:#}", server_run_error(err));
        }
    }
}

/// Pause after an accept failure (EMFILE/ENFILE/ENOBUFS keep the listener
/// readable, so an immediate retry would busy-loop).
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Accept one connection and set the socket options `RdpServer::run` would
/// have set. RDP output is small writes the peer waits on, so Nagle off.
async fn accept_session(
    listener: &tokio::net::TcpListener,
) -> Result<(tokio::net::TcpStream, SocketAddr)> {
    let (stream, peer) = listener.accept().await.context("accept failed")?;
    if let Err(err) = stream.set_nodelay(true) {
        tracing::warn!(%peer, "Failed to set TCP_NODELAY: {}", err);
    }
    if !set_session_liveness_timeout(&stream) {
        tracing::warn!(%peer, "Failed to set session liveness timeout");
    }
    Ok((stream, peer))
}

/// How long a dead peer may hold the single session slot before the kernel
/// drops it. Without this a peer that stops responding keeps the slot for
/// `tcp_retries2` (~13-30 min), so a reconnect is refused that whole time.
const SESSION_LIVENESS_TIMEOUT_SECS: u32 = 30;

/// Bound the dead-peer hold. `TCP_USER_TIMEOUT` caps retransmission of
/// unacknowledged data; `SO_KEEPALIVE` (with the interval knobs) makes an
/// idle-but-dead peer produce probes so the timeout bites even when the server
/// is only waiting to read. The trade-off: a live session across a network
/// stall longer than the bound is torn down and the client must reconnect
/// (mstsc does so with its auto-reconnect cookie). Returns false if any option
/// did not take.
fn set_session_liveness_timeout(stream: &tokio::net::TcpStream) -> bool {
    use std::os::fd::AsRawFd as _;
    let fd = stream.as_raw_fd();
    let set = |level: libc::c_int, opt: libc::c_int, val: libc::c_int| -> bool {
        // SAFETY: `fd` is a live socket owned by `stream` for the call, and
        // `val` outlives the pointer passed to `setsockopt`.
        unsafe {
            libc::setsockopt(
                fd,
                level,
                opt,
                std::ptr::addr_of!(val).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            ) == 0
        }
    };
    let secs = SESSION_LIVENESS_TIMEOUT_SECS as libc::c_int;
    set(libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, secs * 1000)
        && set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1)
        && set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, secs / 2)
        && set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 5)
        && set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3)
}

fn bind_listener(addr: SocketAddr) -> Result<tokio::net::TcpListener> {
    let socket = match addr {
        SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4().context("create IPv4 socket")?,
        SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6().context("create IPv6 socket")?,
    };
    // SO_REUSEADDR matches RdpServer::run(): restarts must not trip over a
    // socket still in TIME_WAIT.
    #[cfg(unix)]
    socket.set_reuseaddr(true).context("set SO_REUSEADDR")?;
    socket.bind(addr).context("bind listen address")?;
    // Match RdpServer::run's LISTENER_BACKLOG so a burst of reconnects is not
    // dropped by the kernel before the loop can reject them.
    socket.listen(1024).context("start listener")
}

/// Adapts IronRDP connection boundaries to application-owned policies.
struct ClientConnectionHandler {
    input_session_sink: Arc<dyn RdpInputSessionSink>,
    session_hooks: Option<SharedSessionHooks>,
}

impl ClientConnectionHandler {
    fn new(
        input_session_sink: Arc<dyn RdpInputSessionSink>,
        session_hooks: Option<SharedSessionHooks>,
    ) -> Self {
        Self {
            input_session_sink,
            session_hooks,
        }
    }
}

impl ConnectionHandler for ClientConnectionHandler {
    fn on_connection_info(&mut self, info: &ConnectionInfo) {
        self.input_session_sink
            .set_keyboard_layout(info.keyboard_layout);
        if let Some(hooks) = &self.session_hooks {
            hooks.session_started();
        }
    }

    /// Kept as a safety net: `serve_on` owns the accept loop this would be
    /// called from, and reports the boundary itself. `session_ended` is
    /// idempotent, so reaching both paths would still run one end command.
    fn on_disconnected(
        &mut self,
        _peer: SocketAddr,
        _duration: Duration,
        _error: Option<&ServerError>,
    ) -> PostConnectionAction {
        self.input_session_sink.session_ended();
        if let Some(hooks) = &self.session_hooks {
            hooks.session_ended();
        }
        PostConnectionAction::Continue
    }
}

fn ironrdp_credentials(credentials: Option<ConfigCredentials>) -> Option<Credentials> {
    credentials.map(|credentials| Credentials {
        username: credentials.username,
        password: credentials.password,
        domain: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerSecurityMode {
    Tls,
    Hybrid,
}

fn security_mode_for_credentials(credentials: &Option<Credentials>) -> ServerSecurityMode {
    if credentials.is_some() {
        ServerSecurityMode::Hybrid
    } else {
        ServerSecurityMode::Tls
    }
}

#[cfg(test)]
mod tests {
    use super::session_hooks::test_support::{
        echo_start, hook_log_path, shared_test_hooks, wait_for_log, LOG_CEILING,
    };
    use super::*;

    use ironrdp_pdu::gcc::KeyboardType;
    use ironrdp_server::{
        ConnectionHandler, ConnectionInfo, PostConnectionAction, RdpServer, ServerEvent,
    };
    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;
    use tokio::sync::oneshot;

    fn test_peer() -> SocketAddr {
        "127.0.0.1:39999".parse().unwrap()
    }

    fn test_connection_info() -> ConnectionInfo {
        ConnectionInfo::new(0x0409, KeyboardType::IBM_ENHANCED, String::new())
    }

    #[test]
    fn server_run_error_keeps_the_cause_the_server_reported() {
        use ironrdp_server::ServerErrorExt as _;
        let error = ServerError::io(
            "accepting a client",
            std::io::Error::other("tls handshake failed"),
        );

        let converted = server_run_error(error);
        let rendered = format!("{converted:#}");

        assert!(
            rendered.contains("accepting a client"),
            "context lost: {rendered}"
        );
        assert!(
            rendered.contains("tls handshake failed"),
            "cause lost: {rendered}"
        );
    }
    #[test]
    fn connection_handler_drives_hooks_on_both_boundaries() {
        struct NoopSink;
        impl RdpInputSessionSink for NoopSink {
            fn set_keyboard_layout(&self, _keyboard_layout: u32) {}
            fn session_ended(&self) {}
        }

        let log = hook_log_path("forwarding");
        let hooks = shared_test_hooks(&log, echo_start(&log, ""), true);
        let mut handler = ClientConnectionHandler::new(Arc::new(NoopSink), Some(hooks));

        handler.on_connection_info(&test_connection_info());
        assert_eq!(wait_for_log(&log, "start\n", LOG_CEILING), "start\n");

        let action = handler.on_disconnected(test_peer(), Duration::from_secs(1), None);
        assert_eq!(action, PostConnectionAction::Continue);

        assert_eq!(
            wait_for_log(&log, "start\nend\n", LOG_CEILING),
            "start\nend\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn disconnecting_notifies_the_input_session_sink() {
        use std::sync::{Arc, Mutex};

        struct ReleaseRecordingSink {
            released: Arc<Mutex<bool>>,
        }

        impl RdpInputSessionSink for ReleaseRecordingSink {
            fn set_keyboard_layout(&self, _keyboard_layout: u32) {}
            fn session_ended(&self) {
                *self.released.lock().unwrap() = true;
            }
        }

        let released = Arc::new(Mutex::new(false));
        let mut handler = ClientConnectionHandler::new(
            Arc::new(ReleaseRecordingSink {
                released: Arc::clone(&released),
            }),
            None,
        );

        handler.on_disconnected(test_peer(), Duration::from_secs(1), None);

        assert!(*released.lock().unwrap());
    }

    #[test]
    fn on_connection_info_forwards_keyboard_layout_to_sink() {
        use std::sync::{Arc, Mutex};

        struct RecordingSink {
            layouts: Arc<Mutex<Vec<u32>>>,
        }

        impl RdpInputSessionSink for RecordingSink {
            fn set_keyboard_layout(&self, keyboard_layout: u32) {
                self.layouts.lock().unwrap().push(keyboard_layout);
            }
            fn session_ended(&self) {}
        }

        let layouts = Arc::new(Mutex::new(Vec::new()));
        let sink = RecordingSink {
            layouts: Arc::clone(&layouts),
        };
        let mut handler = ClientConnectionHandler::new(Arc::new(sink), None);

        handler.on_connection_info(&ConnectionInfo::new(
            0x00000407,
            KeyboardType::IBM_ENHANCED,
            String::new(),
        ));

        assert_eq!(*layouts.lock().unwrap(), vec![0x00000407]);
    }

    #[test]
    fn server_maps_config_credentials_without_reclassifying_them() {
        assert_eq!(
            security_mode_for_credentials(&None),
            ServerSecurityMode::Tls
        );

        for (username, password) in [("user", "pass"), ("user", ""), ("", "pass")] {
            let credentials = ironrdp_credentials(Some(ConfigCredentials {
                username: username.into(),
                password: password.into(),
            }));
            assert_eq!(
                security_mode_for_credentials(&credentials),
                ServerSecurityMode::Hybrid
            );
            let credentials = credentials.as_ref().expect("configured credentials");

            assert_eq!(credentials.username, username);
            assert_eq!(credentials.password, password);
            assert_eq!(credentials.domain, None);
        }
    }

    #[test]
    fn audio_mode_off_disables_sound_factory_wiring() {
        assert!(sound_factory_for_audio_mode(AudioMode::Mirror).is_some());
        assert!(sound_factory_for_audio_mode(AudioMode::Redirect).is_some());
        assert!(sound_factory_for_audio_mode(AudioMode::Off).is_none());
    }

    /// Counts session ends, so a test can tell whether the accept loop
    /// reported the boundary the connection handler would have.
    struct CountingSink(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl RdpInputSessionSink for CountingSink {
        fn set_keyboard_layout(&self, _keyboard_layout: u32) {}
        fn session_ended(&self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn serve_on_reports_session_end_to_sink_and_hooks() {
        let mut server = RdpServer::builder()
            .with_addr(([127, 0, 0, 1], 0))
            .with_no_security()
            .with_no_input()
            .with_no_display()
            .build();

        let log = hook_log_path("accept-loop-end");
        let hooks = shared_test_hooks(&log, echo_start(&log, ""), true);
        // The start boundary belongs to the connection handler; this test
        // covers the end boundary, which `RdpServer::run` would report and
        // `serve_on` therefore has to report itself.
        hooks.session_started();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");

        // `on_disconnected` is only called from `RdpServer::run`'s own accept
        // loop, which this replaces, so releasing the keys the session left
        // held is this loop's job too. Missing it leaves a modifier stuck in
        // the compositor after every disconnect.
        let releases = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = CountingSink(std::sync::Arc::clone(&releases));

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let loop_hooks = hooks.clone();
                tokio::task::spawn_local(async move {
                    let _ = serve_on(listener, &mut server, Some(&loop_hooks), &sink).await;
                });

                // A client that connects and leaves: the session it stood for
                // has to reach the end hook.
                let client = TcpStream::connect(addr).await.expect("connect");
                drop(client);

                // Async wait: a blocking one would stop the accept loop task
                // from ever running on this single-threaded LocalSet.
                let deadline = std::time::Instant::now() + LOG_CEILING;
                let content = loop {
                    let content = std::fs::read_to_string(&log).unwrap_or_default();
                    if content == "start\nend\n" || std::time::Instant::now() > deadline {
                        break content;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                };

                assert_eq!(
                    content, "start\nend\n",
                    "a session that ends under our own accept loop must still run the end hook"
                );
                assert_eq!(
                    releases.load(std::sync::atomic::Ordering::SeqCst),
                    1,
                    "the accept loop must also release the keys the session held"
                );
                std::fs::remove_file(&log).expect("remove hook log");
            })
            .await;
    }

    #[tokio::test]
    async fn accepted_session_socket_has_nagle_disabled() {
        // `RdpServer::run` sets this on every socket it accepts; the accept
        // loop that replaces it has to do the same, or every small write waits
        // on the previous one's acknowledgement.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");

        let client = tokio::spawn(async move { tokio::net::TcpStream::connect(addr).await });
        let (stream, _peer) = accept_session(&listener).await.expect("accept");

        assert!(
            stream.nodelay().expect("read TCP_NODELAY"),
            "Nagle still enabled on the accepted session socket"
        );
        drop(client.await.expect("client task").expect("client connect"));
    }

    #[tokio::test]
    async fn accepted_session_socket_has_a_liveness_timeout() {
        // A dead peer must not hold the single slot for tcp_retries2. We can
        // only assert the options were set here -- a real teardown needs a
        // black-hole peer, which loopback cannot produce.
        use std::os::fd::AsRawFd as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");

        let client = tokio::spawn(async move { tokio::net::TcpStream::connect(addr).await });
        let (stream, _peer) = accept_session(&listener).await.expect("accept");

        let fd = stream.as_raw_fd();
        let mut val: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: fd is a live socket owned by `stream`; val/len outlive the call.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_USER_TIMEOUT,
                std::ptr::addr_of_mut!(val).cast::<libc::c_void>(),
                &mut len,
            )
        };
        assert_eq!(rc, 0, "getsockopt TCP_USER_TIMEOUT failed");
        assert_eq!(
            val,
            (SESSION_LIVENESS_TIMEOUT_SECS * 1000) as libc::c_int,
            "TCP_USER_TIMEOUT not set to the liveness bound"
        );

        let mut ka: libc::c_int = 0;
        let mut kalen = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: as above.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_KEEPALIVE,
                std::ptr::addr_of_mut!(ka).cast::<libc::c_void>(),
                &mut kalen,
            )
        };
        assert_eq!(rc, 0, "getsockopt SO_KEEPALIVE failed");
        assert_ne!(ka, 0, "SO_KEEPALIVE not enabled");

        // The keepalive cadence is what actually makes an idle-but-dead peer
        // produce probes for TCP_USER_TIMEOUT to bound; verify each knob, not
        // just that keepalive is on.
        let read_int = |opt: libc::c_int| -> libc::c_int {
            let mut v: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: fd is a live socket owned by `stream`; v/len outlive the call.
            let rc = unsafe {
                libc::getsockopt(
                    fd,
                    libc::IPPROTO_TCP,
                    opt,
                    std::ptr::addr_of_mut!(v).cast::<libc::c_void>(),
                    &mut len,
                )
            };
            assert_eq!(rc, 0, "getsockopt failed");
            v
        };
        assert_eq!(
            read_int(libc::TCP_KEEPIDLE),
            (SESSION_LIVENESS_TIMEOUT_SECS / 2) as libc::c_int,
            "TCP_KEEPIDLE not set to half the liveness bound"
        );
        assert_eq!(read_int(libc::TCP_KEEPINTVL), 5, "TCP_KEEPINTVL not 5s");
        assert_eq!(read_int(libc::TCP_KEEPCNT), 3, "TCP_KEEPCNT not 3");

        drop(client.await.expect("client task").expect("client connect"));
    }

    #[tokio::test]
    async fn serve_on_rejects_second_client_while_session_active_and_serves_next() {
        // The failure being fixed is a connection nobody answers, and a read
        // that times out cannot tell "the server is waiting for my handshake"
        // from "the server stopped accepting". The positive proof that the
        // loop took the next client is that it bounces the one behind it: a
        // loop that stopped accepting bounces nobody.
        let mut server = RdpServer::builder()
            .with_addr(([127, 0, 0, 1], 0))
            .with_no_security()
            .with_no_input()
            .with_no_display()
            .build();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let ended = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let sink = CountingSink(std::sync::Arc::clone(&ended));
                tokio::task::spawn_local(async move {
                    let _ = serve_on(listener, &mut server, None, &sink).await;
                });

                let mut buf = [0u8; 8];

                // Occupy the session, and wait for a bounce to confirm the
                // loop really is inside one before ending it.
                let holder = TcpStream::connect(addr).await.expect("first connect");
                let mut probe = TcpStream::connect(addr).await.expect("probe connect");
                let read = tokio::time::timeout(Duration::from_secs(5), probe.read(&mut buf))
                    .await
                    .expect("the busy arm must answer")
                    .expect("read on the bounced connection");
                assert_eq!(read, 0, "busy server must close the extra connection");
                drop(holder);

                // The holder was the session; the loop must reach the end
                // boundary for it (session_ended fires) before it can serve
                // anyone else. A loop still wedged inside the holder never
                // increments this, so it distinguishes "drained, moving on"
                // from "still stuck" -- which a bounce alone cannot.
                let drained_by = std::time::Instant::now() + Duration::from_secs(10);
                while ended.load(std::sync::atomic::Ordering::SeqCst) < 1 {
                    assert!(
                        std::time::Instant::now() < drained_by,
                        "the accept loop never left the first session"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }

                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    let successor = TcpStream::connect(addr).await.expect("successor connect");
                    let mut follower = TcpStream::connect(addr).await.expect("follower connect");
                    let bounced =
                        tokio::time::timeout(Duration::from_secs(1), follower.read(&mut buf)).await;
                    if matches!(bounced, Ok(Ok(0))) {
                        // The successor is the session, so the follower was
                        // bounced by the busy arm: the loop is serving again.
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the accept loop never took another client after its session ended"
                    );
                    drop(successor);
                    drop(follower);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
    }

    #[tokio::test]
    async fn serve_on_recovers_from_a_malformed_pre_auth_client() {
        // A client that sends garbage and leaves (43 zero bytes, no valid
        // X.224) must not wedge the loop: run_connection returns an error and
        // the next client is served. This is the maintainer's #79 case on the
        // accept loop that now owns the path.
        let mut server = RdpServer::builder()
            .with_addr(([127, 0, 0, 1], 0))
            .with_no_security()
            .with_no_input()
            .with_no_display()
            .build();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let ended = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let sink = CountingSink(std::sync::Arc::clone(&ended));
                tokio::task::spawn_local(async move {
                    let _ = serve_on(listener, &mut server, None, &sink).await;
                });

                let mut buf = [0u8; 8];

                // Garbage, then gone.
                let mut malformed = TcpStream::connect(addr).await.expect("malformed connect");
                malformed
                    .write_all(&[0u8; 43])
                    .await
                    .expect("write malformed bytes");
                drop(malformed);

                // The malformed client held the session slot; the loop must
                // reach its end boundary (session_ended) and move on. A loop
                // that wedged on the garbage instead never increments this --
                // this is the #79 failure the bounce proof below cannot see on
                // its own, since a stuck-but-alive loop bounces a follower too.
                let drained_by = std::time::Instant::now() + Duration::from_secs(10);
                while ended.load(std::sync::atomic::Ordering::SeqCst) < 1 {
                    assert!(
                        std::time::Instant::now() < drained_by,
                        "a malformed pre-auth client wedged the accept loop"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }

                // And it must serve again: the successor is the session because
                // the follower behind it is bounced.
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    let successor = TcpStream::connect(addr).await.expect("successor connect");
                    let mut follower = TcpStream::connect(addr).await.expect("follower connect");
                    let bounced =
                        tokio::time::timeout(Duration::from_secs(1), follower.read(&mut buf)).await;
                    if matches!(bounced, Ok(Ok(0))) {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "a malformed pre-auth client wedged the accept loop"
                    );
                    drop(successor);
                    drop(follower);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
    }
    #[tokio::test]
    async fn server_lifecycle_quit_exits_after_ephemeral_bind() {
        let mut server = RdpServer::builder()
            .with_addr(([127, 0, 0, 1], 0))
            .with_no_security()
            .with_no_input()
            .with_no_display()
            .build();
        let event_sender = server.event_sender().clone();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let server_task = tokio::task::spawn_local(async move { server.run().await });
                let bound_addr = wait_for_local_addr(&event_sender).await;
                assert_eq!(bound_addr.ip().to_string(), "127.0.0.1");
                assert_ne!(bound_addr.port(), 0);

                event_sender
                    .send(ServerEvent::Quit("test quit".into()))
                    .expect("server event receiver");

                tokio::time::timeout(Duration::from_secs(1), server_task)
                    .await
                    .expect("server quit must be bounded")
                    .expect("server task must not panic")
                    .expect("server run must succeed");
            })
            .await;
    }

    #[tokio::test]
    async fn server_lifecycle_client_abort_returns_to_disconnect_handler() {
        let mut server = RdpServer::builder()
            .with_addr(([127, 0, 0, 1], 0))
            .with_no_security()
            .with_no_input()
            .with_no_display()
            .with_connection_handler(Some(Box::new(StopAfterDisconnects::new(1))))
            .build();
        let event_sender = server.event_sender().clone();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let server_task = tokio::task::spawn_local(async move { server.run().await });
                let bound_addr = wait_for_local_addr(&event_sender).await;
                let stream = TcpStream::connect(bound_addr)
                    .await
                    .expect("connect to server");
                drop(stream);

                tokio::time::timeout(Duration::from_secs(1), server_task)
                    .await
                    .expect("client abort must be bounded")
                    .expect("server task must not panic")
                    .expect("server run must succeed");
            })
            .await;
    }

    #[tokio::test]
    async fn server_lifecycle_malformed_zero_length_pdu_does_not_block_next_client() {
        let mut server = RdpServer::builder()
            .with_addr(([127, 0, 0, 1], 0))
            .with_no_security()
            .with_no_input()
            .with_no_display()
            .with_connection_handler(Some(Box::new(StopAfterDisconnects::new(2))))
            .build();
        let event_sender = server.event_sender().clone();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let server_task = tokio::task::spawn_local(async move { server.run().await });
                let bound_addr = wait_for_local_addr(&event_sender).await;

                let mut malformed = TcpStream::connect(bound_addr)
                    .await
                    .expect("connect malformed client");
                malformed
                    .write_all(&[0; 43])
                    .await
                    .expect("write malformed pre-authentication bytes");
                drop(malformed);

                let second = TcpStream::connect(bound_addr)
                    .await
                    .expect("connect second client");
                drop(second);

                tokio::time::timeout(Duration::from_secs(1), server_task)
                    .await
                    .expect("malformed client must not block the next client")
                    .expect("server task must not panic")
                    .expect("server run must succeed");
            })
            .await;
    }

    struct StopAfterDisconnects {
        remaining: usize,
    }

    impl StopAfterDisconnects {
        fn new(remaining: usize) -> Self {
            Self { remaining }
        }
    }

    impl ConnectionHandler for StopAfterDisconnects {
        fn on_disconnected(
            &mut self,
            _peer: std::net::SocketAddr,
            _duration: Duration,
            error: Option<&ServerError>,
        ) -> PostConnectionAction {
            assert!(error.is_some(), "raw client abort should end with an error");
            self.remaining -= 1;
            if self.remaining == 0 {
                PostConnectionAction::Stop
            } else {
                PostConnectionAction::Continue
            }
        }
    }

    async fn wait_for_local_addr(
        event_sender: &mpsc::UnboundedSender<ServerEvent>,
    ) -> std::net::SocketAddr {
        for _ in 0..100 {
            let (tx, rx) = oneshot::channel();
            event_sender
                .send(ServerEvent::GetLocalAddr(tx))
                .expect("server event receiver");
            if let Some(addr) = rx.await.expect("local addr response") {
                return addr;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        panic!("server did not publish local address");
    }
}
