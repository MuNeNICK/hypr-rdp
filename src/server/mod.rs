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

use session_hooks::{session_hooks_from_config, SessionHooks};

pub struct ServerContext {
    server: RdpServer,
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
    let input_session_sink: Box<dyn RdpInputSessionSink> = Box::new(input_session_sink);

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
    let security_mode = security_mode_for_credentials(&credentials);
    let secured_builder = match security_mode {
        ServerSecurityMode::Tls => builder.with_tls(acceptor),
        ServerSecurityMode::Hybrid => builder.with_hybrid(acceptor, tls_ctx.pub_key),
    };

    let mut server = secured_builder
        .with_input_handler(input_handler)
        .with_display_handler(display)
        .with_preempt_existing_session(security_mode.allows_authenticated_replacement())
        .with_connection_handler(Some(Box::new(ClientConnectionHandler::new(
            input_session_sink,
            session_hooks,
        ))))
        .with_gfx_factory(Some(Box::new(gfx_factory)))
        .with_cliprdr_factory(Some(Box::new(cliprdr_factory)))
        .with_sound_factory(sound_factory)
        .build();

    server.set_credentials(credentials);

    tracing::info!("RDP server configured for {}", bind);

    Ok(ServerContext {
        server,
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
    ctx.server.run().await.map_err(server_run_error)
}

fn server_run_error(error: ServerError) -> anyhow::Error {
    anyhow::Error::new(error)
}

/// Adapts IronRDP connection boundaries to application-owned policies.
struct ClientConnectionHandler {
    input_session_sink: Box<dyn RdpInputSessionSink>,
    session_hooks: Option<SessionHooks>,
}

impl ClientConnectionHandler {
    fn new(
        input_session_sink: Box<dyn RdpInputSessionSink>,
        session_hooks: Option<SessionHooks>,
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
        if let Some(hooks) = &mut self.session_hooks {
            hooks.session_started();
        }
    }

    /// The server calls this only from its own accept loop, so anything that
    /// takes ownership of the loop and drives `run_connection` directly has to
    /// invoke the session-end path itself.
    fn on_disconnected(
        &mut self,
        _peer: SocketAddr,
        _duration: Duration,
        _error: Option<&ServerError>,
    ) -> PostConnectionAction {
        self.input_session_sink.session_ended();
        if let Some(hooks) = &mut self.session_hooks {
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

impl ServerSecurityMode {
    fn allows_authenticated_replacement(self) -> bool {
        // IronRDP authenticates Hybrid candidates through CredSSP before eviction.
        // TLS alone only proves the handshake, so keep its existing queue policy.
        matches!(self, Self::Hybrid)
    }
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
        echo_start, hook_log_path, test_hooks, wait_for_log, LOG_CEILING,
    };
    use super::*;

    use ironrdp_pdu::gcc::KeyboardType;
    use ironrdp_server::{
        ConnectionHandler, ConnectionInfo, PostConnectionAction, RdpServer, ServerEvent,
    };
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
        let hooks = test_hooks(&log, echo_start(&log, ""), true);
        let mut handler = ClientConnectionHandler::new(Box::new(NoopSink), Some(hooks));

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
            Box::new(ReleaseRecordingSink {
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
        let mut handler = ClientConnectionHandler::new(Box::new(sink), None);

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
    fn replacement_requires_nla_credentials() {
        assert!(!security_mode_for_credentials(&None).allows_authenticated_replacement());
        let credentials = ironrdp_credentials(Some(ConfigCredentials {
            username: "user".into(),
            password: "pass".into(),
        }));
        assert!(security_mode_for_credentials(&credentials).allows_authenticated_replacement());
    }

    #[test]
    fn audio_mode_off_disables_sound_factory_wiring() {
        assert!(sound_factory_for_audio_mode(AudioMode::Mirror).is_some());
        assert!(sound_factory_for_audio_mode(AudioMode::Redirect).is_some());
        assert!(sound_factory_for_audio_mode(AudioMode::Off).is_none());
    }

    // This exercises candidate acceptance and non-eviction, not a completed NLA login.
    #[tokio::test]
    async fn replacement_policy_controls_candidate_acceptance() {
        use tokio::io::AsyncReadExt as _;

        struct Accepted(tokio::sync::mpsc::UnboundedSender<SocketAddr>);
        impl ConnectionHandler for Accepted {
            fn on_accept(&mut self, peer: SocketAddr) -> bool {
                self.0.send(peer).expect("accept observer alive");
                true
            }
        }

        let _ = rustls::crypto::ring::default_provider().install_default();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let tls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));

        for mode in [ServerSecurityMode::Hybrid, ServerSecurityMode::Tls] {
            let builder = RdpServer::builder().with_addr(([127, 0, 0, 1], 0));
            let builder = match mode {
                ServerSecurityMode::Hybrid => {
                    builder.with_hybrid(acceptor.clone(), key.public_key_raw().to_vec())
                }
                ServerSecurityMode::Tls => builder.with_tls(acceptor.clone()),
            };
            let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut server = builder
                .with_no_input()
                .with_no_display()
                .with_preempt_existing_session(mode.allows_authenticated_replacement())
                .with_connection_handler(Some(Box::new(Accepted(accepted_tx))))
                .build();
            if mode == ServerSecurityMode::Hybrid {
                server.set_credentials(ironrdp_credentials(Some(ConfigCredentials {
                    username: "test".into(),
                    password: "test".into(),
                })));
            }
            let events = server.event_sender().clone();
            tokio::task::LocalSet::new()
                .run_until(async move {
                    let task = tokio::task::spawn_local(async move { server.run().await });
                    let addr = wait_for_local_addr(&events).await;
                    let mut incumbent = TcpStream::connect(addr).await.unwrap();
                    let first = tokio::time::timeout(Duration::from_secs(2), accepted_rx.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(first, incumbent.local_addr().unwrap());
                    let mut candidate = TcpStream::connect(addr).await.unwrap();
                    candidate.write_all(&[0; 43]).await.unwrap();
                    let second =
                        tokio::time::timeout(Duration::from_millis(300), accepted_rx.recv()).await;
                    if mode == ServerSecurityMode::Hybrid {
                        assert_eq!(second.unwrap().unwrap(), candidate.local_addr().unwrap());
                        let mut byte = [0];
                        let closed =
                            tokio::time::timeout(Duration::from_secs(2), candidate.read(&mut byte))
                                .await;
                        assert!(matches!(closed, Ok(Ok(0)) | Ok(Err(_))));
                        assert!(
                            tokio::time::timeout(
                                Duration::from_millis(100),
                                incumbent.read(&mut byte)
                            )
                            .await
                            .is_err(),
                            "malformed candidate must not evict the incumbent"
                        );
                    } else {
                        assert!(
                            second.is_err(),
                            "TLS-only connections must retain the queue policy"
                        );
                    }
                    drop(candidate);
                    drop(incumbent);
                    events
                        .send(ServerEvent::Quit("test complete".into()))
                        .unwrap();
                    tokio::time::timeout(Duration::from_secs(3), task)
                        .await
                        .expect("shutdown must remain bounded")
                        .unwrap()
                        .unwrap();
                })
                .await;
        }
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
