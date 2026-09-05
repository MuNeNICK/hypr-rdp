use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;

use crate::audio::AudioMode;
use crate::capture::CaptureMode;
use crate::egfx::{
    EgfxCodecPolicy, H264BackendPolicy, H264RateControl, DEFAULT_MAX_FRAMES_IN_FLIGHT,
};
use crate::input::KeyboardLayoutPolicy;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileTransferMode {
    Off,
    ToClient,
    ToServer,
    Both,
}

impl FileTransferMode {
    pub(crate) fn permits_to_client(self) -> bool {
        matches!(self, Self::ToClient | Self::Both)
    }
}

#[derive(Parser, Debug)]
#[command(name = "hypr-rdp", version, about = "Native RDP server for Hyprland")]
struct Args {
    /// Address to bind the RDP server
    #[arg(short, long)]
    bind: Option<String>,

    /// TLS certificate file (PEM)
    #[arg(long)]
    cert: Option<String>,

    /// TLS private key file (PEM)
    #[arg(long)]
    key: Option<String>,

    /// Username for RDP authentication
    #[arg(short, long)]
    username: Option<String>,

    /// Password for RDP authentication
    #[arg(short, long)]
    password: Option<String>,

    /// RDP session resolution (WxH), e.g. 1920x1080
    #[arg(short, long)]
    resolution: Option<String>,

    /// Scale of the managed headless output, e.g. 2 for HiDPI clients
    #[arg(long)]
    scale: Option<f64>,

    /// Screen capture protocol: "wlr" (wlr-screencopy-v1) or "ext" (ext-image-copy-capture-v1)
    #[arg(long)]
    capture_mode: Option<String>,

    /// H.264 encoder bitrate in bps
    #[arg(long)]
    bitrate: Option<u32>,

    /// H.264 quality level (0-51, lower = better)
    #[arg(long)]
    quality: Option<u8>,

    /// H.264 rate control mode: "vbr" (default) or "cqp"
    #[arg(long)]
    rate_control: Option<String>,

    /// Maximum capture frame rate
    #[arg(long)]
    fps: Option<u32>,

    /// Maximum unacknowledged EGFX frames in flight
    #[arg(long)]
    max_frames_in_flight: Option<u32>,

    /// EGFX codec policy: "avc420" (default), "avc444" (experimental), or "auto"
    #[arg(long)]
    egfx_codec: Option<String>,

    /// Keyboard layout policy: "client" (default) or "compositor"
    #[arg(long)]
    keyboard_layout_policy: Option<String>,

    /// Audio output policy: "redirect" (default), "mirror", or "off"
    #[arg(long)]
    audio_mode: Option<String>,

    /// H.264 encoder backend: "auto" (default), "software", or "vaapi"
    #[arg(long)]
    h264_backend: Option<String>,

    /// Capture a specific output instead of creating a headless one
    #[arg(long)]
    output: Option<String>,

    /// Shell command to run when an authenticated session starts
    /// (without configured credentials: any established session)
    #[arg(long)]
    on_session_start: Option<String>,

    /// Shell command to run when an authenticated session ends
    /// (also on service stop, and without configured credentials: any
    /// established session)
    #[arg(long)]
    on_session_end: Option<String>,

    /// File transfer policy: "off", "to-client", "to-server", or "both"
    #[arg(long)]
    file_transfer_mode: Option<String>,

    /// Maximum bytes accepted in one clipboard file-content range request
    #[arg(long)]
    file_transfer_max_chunk_bytes: Option<u32>,

    /// Path to config file [default: ~/.config/hypr-rdp/config.toml]
    #[arg(long)]
    config: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ConfigFile {
    bind: Option<String>,
    cert: Option<String>,
    key: Option<String>,
    username: Option<String>,
    password: Option<String>,
    resolution: Option<String>,
    scale: Option<f64>,
    capture_mode: Option<String>,
    bitrate: Option<u32>,
    quality: Option<u8>,
    rate_control: Option<String>,
    fps: Option<u32>,
    max_frames_in_flight: Option<u32>,
    egfx_codec: Option<String>,
    keyboard_layout_policy: Option<String>,
    audio_mode: Option<String>,
    h264_backend: Option<String>,
    output: Option<String>,
    on_session_start: Option<String>,
    on_session_end: Option<String>,
    file_transfer_mode: Option<String>,
    file_transfer_max_chunk_bytes: Option<u32>,
}

impl ConfigFile {
    fn load(path: Option<&str>) -> anyhow::Result<Self> {
        let (config_path, explicit) = match path {
            Some(p) => (PathBuf::from(p), true),
            None => {
                let home = match std::env::var("HOME") {
                    Ok(home) => home,
                    Err(_) => return Ok(Self::default()),
                };
                (
                    PathBuf::from(home)
                        .join(".config")
                        .join("hypr-rdp")
                        .join("config.toml"),
                    false,
                )
            }
        };

        let content = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(error) if !explicit && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                anyhow::bail!("failed to read config {}: {}", config_path.display(), error);
            }
        };

        match toml::from_str(&content) {
            Ok(config) => {
                tracing::info!("Loaded config from {}", config_path.display());
                Ok(config)
            }
            Err(error) => {
                anyhow::bail!(
                    "failed to parse config {}: {}",
                    config_path.display(),
                    error
                );
            }
        }
    }
}

pub struct RuntimeConfig {
    pub bind: SocketAddr,
    pub cert: Option<String>,
    pub key: Option<String>,
    pub credentials: Option<ConfigCredentials>,
    pub resolution: (u32, u32),
    pub headless_scale: f64,
    pub capture_mode: CaptureMode,
    pub bitrate: u32,
    pub quality: u8,
    pub rate_control: H264RateControl,
    pub fps: u32,
    pub max_frames_in_flight: u32,
    pub egfx_codec: EgfxCodecPolicy,
    pub keyboard_layout_policy: KeyboardLayoutPolicy,
    pub audio_mode: AudioMode,
    pub h264_backend: H264BackendPolicy,
    pub resolution_fixed: bool,
    pub output: Option<String>,
    pub on_session_start: Option<String>,
    pub on_session_end: Option<String>,
    pub file_transfer_mode: FileTransferMode,
    pub file_transfer_max_chunk_bytes: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ConfigCredentials {
    pub username: String,
    pub password: String,
}

impl ConfigCredentials {
    fn from_parts(username: String, password: String) -> Option<Self> {
        if username.is_empty() && password.is_empty() {
            None
        } else {
            Some(Self { username, password })
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StartupWarning {
    AuthenticationOff,
    ReachableBeyondLoopback,
    HalfCredentials,
}

fn startup_warnings(
    credentials: Option<&ConfigCredentials>,
    bind: SocketAddr,
) -> Vec<StartupWarning> {
    let mut warnings = Vec::new();

    if credentials.is_none() {
        warnings.push(StartupWarning::AuthenticationOff);
    } else if credentials
        .is_some_and(|value| value.username.is_empty() || value.password.is_empty())
    {
        warnings.push(StartupWarning::HalfCredentials);
    }

    let password_is_empty = credentials.is_none_or(|value| value.password.is_empty());
    if password_is_empty && !bind.ip().to_canonical().is_loopback() {
        warnings.push(StartupWarning::ReachableBeyondLoopback);
    }

    warnings
}

impl RuntimeConfig {
    pub fn load() -> anyhow::Result<Self> {
        let args = Args::parse();
        let config = ConfigFile::load(args.config.as_deref())?;

        let bind = args
            .bind
            .or(config.bind)
            .unwrap_or_else(|| "127.0.0.1:3389".into());
        let bind = parse_bind_addr(&bind)?;
        let cert = args.cert.or(config.cert);
        let key = args.key.or(config.key);
        let username = args.username.or(config.username).unwrap_or_default();
        let password = args.password.or(config.password).unwrap_or_default();
        let credentials = ConfigCredentials::from_parts(username, password);

        for warning in startup_warnings(credentials.as_ref(), bind) {
            match warning {
                StartupWarning::AuthenticationOff => tracing::warn!(
                    "No credentials set (-u/-p). Use -u <user> -p <pass> to require authentication."
                ),
                StartupWarning::ReachableBeyondLoopback => tracing::warn!(
                    bind = %bind,
                    "RDP is reachable beyond loopback without a password."
                ),
                StartupWarning::HalfCredentials => tracing::warn!(
                    "Only one half of the credentials is set; the other one is matched as empty."
                ),
            }
        }

        let resolution_fixed = args.resolution.is_some() || config.resolution.is_some();
        let resolution_str = args
            .resolution
            .or(config.resolution)
            .unwrap_or_else(|| "1920x1080".into());
        let headless_scale = resolve_headless_scale(args.scale, config.scale)?;
        let capture_mode_str = args
            .capture_mode
            .or(config.capture_mode)
            .unwrap_or_else(|| "wlr".into());
        let bitrate = args.bitrate.or(config.bitrate).unwrap_or(10_000_000);
        let quality = args.quality.or(config.quality).unwrap_or(23);
        let rate_control_str = args
            .rate_control
            .or(config.rate_control)
            .unwrap_or_else(|| "vbr".into());
        let rate_control = parse_rate_control(&rate_control_str)?;
        let fps = args.fps.or(config.fps).unwrap_or(30);
        let max_frames_in_flight = args
            .max_frames_in_flight
            .or(config.max_frames_in_flight)
            .unwrap_or(DEFAULT_MAX_FRAMES_IN_FLIGHT);
        let egfx_codec = resolve_egfx_codec_policy(args.egfx_codec, config.egfx_codec)?;
        let keyboard_layout_policy = resolve_keyboard_layout_policy(
            args.keyboard_layout_policy,
            config.keyboard_layout_policy,
        )?;
        let audio_mode = resolve_audio_mode(args.audio_mode, config.audio_mode)?;
        let h264_backend = resolve_h264_backend_policy(args.h264_backend, config.h264_backend)?;
        let output = args.output.or(config.output);
        let on_session_start = args.on_session_start.or(config.on_session_start);
        let on_session_end = args.on_session_end.or(config.on_session_end);
        let file_transfer_mode = parse_file_transfer_mode(
            &args
                .file_transfer_mode
                .or(config.file_transfer_mode)
                .unwrap_or_else(|| "both".into()),
        )?;
        let file_transfer_max_chunk_bytes = args
            .file_transfer_max_chunk_bytes
            .or(config.file_transfer_max_chunk_bytes)
            .unwrap_or(8 * 1024 * 1024);

        let resolution = parse_resolution(&resolution_str)?;
        let capture_mode = parse_capture_mode(&capture_mode_str)?;

        if quality > 51 {
            anyhow::bail!("quality must be 0-51");
        }
        if fps == 0 {
            anyhow::bail!("fps must be > 0");
        }
        if max_frames_in_flight == 0 {
            anyhow::bail!("max-frames-in-flight must be > 0");
        }
        if file_transfer_max_chunk_bytes == 0 {
            anyhow::bail!("file-transfer-max-chunk-bytes must be > 0");
        }

        Ok(Self {
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
        })
    }
}

fn parse_file_transfer_mode(value: &str) -> anyhow::Result<FileTransferMode> {
    match value {
        "off" => Ok(FileTransferMode::Off),
        "to-client" => Ok(FileTransferMode::ToClient),
        "to-server" => Ok(FileTransferMode::ToServer),
        "both" => Ok(FileTransferMode::Both),
        other => anyhow::bail!(
            "unknown file transfer mode '{other}', expected 'off', 'to-client', 'to-server', or 'both'"
        ),
    }
}

fn parse_bind_addr(bind: &str) -> anyhow::Result<SocketAddr> {
    bind.parse()
        .map_err(|error| anyhow::anyhow!("invalid bind address: {error}"))
}

fn parse_h264_backend_policy(s: &str) -> anyhow::Result<H264BackendPolicy> {
    match s {
        "auto" => Ok(H264BackendPolicy::Auto),
        "software" => Ok(H264BackendPolicy::Software),
        "vaapi" => Ok(H264BackendPolicy::Vaapi),
        other => anyhow::bail!(
            "unknown H.264 backend '{}', expected 'auto', 'software', or 'vaapi'",
            other
        ),
    }
}

fn resolve_h264_backend_policy(
    cli_value: Option<String>,
    config_value: Option<String>,
) -> anyhow::Result<H264BackendPolicy> {
    match cli_value.or(config_value) {
        Some(value) => parse_h264_backend_policy(&value),
        None => Ok(H264BackendPolicy::Auto),
    }
}

fn parse_rate_control(s: &str) -> anyhow::Result<H264RateControl> {
    match s {
        "vbr" => Ok(H264RateControl::Vbr),
        "cqp" => Ok(H264RateControl::Cqp),
        other => anyhow::bail!("unknown rate control '{}', expected 'vbr' or 'cqp'", other),
    }
}

fn parse_capture_mode(s: &str) -> anyhow::Result<CaptureMode> {
    match s {
        "ext" => Ok(CaptureMode::Ext),
        "wlr" => Ok(CaptureMode::Wlr),
        other => anyhow::bail!("unknown capture mode '{}', expected 'ext' or 'wlr'", other),
    }
}

fn parse_egfx_codec_policy(s: &str) -> anyhow::Result<EgfxCodecPolicy> {
    match s {
        "auto" => Ok(EgfxCodecPolicy::Auto),
        "avc420" => Ok(EgfxCodecPolicy::Avc420),
        "avc444" => Ok(EgfxCodecPolicy::Avc444),
        other => anyhow::bail!(
            "unknown EGFX codec '{}', expected 'auto', 'avc420', or 'avc444'",
            other
        ),
    }
}

fn parse_keyboard_layout_policy(s: &str) -> anyhow::Result<KeyboardLayoutPolicy> {
    match s {
        "client" => Ok(KeyboardLayoutPolicy::Client),
        "compositor" => Ok(KeyboardLayoutPolicy::Compositor),
        other => anyhow::bail!(
            "unknown keyboard layout policy '{}', expected 'client' or 'compositor'",
            other
        ),
    }
}

fn parse_audio_mode(s: &str) -> anyhow::Result<AudioMode> {
    match s {
        "mirror" => Ok(AudioMode::Mirror),
        "redirect" => Ok(AudioMode::Redirect),
        "off" => Ok(AudioMode::Off),
        other => anyhow::bail!(
            "unknown audio mode '{}', expected 'mirror', 'redirect', or 'off'",
            other
        ),
    }
}

fn default_egfx_codec_policy_name() -> String {
    "avc420".into()
}

fn resolve_egfx_codec_policy(
    cli_value: Option<String>,
    config_value: Option<String>,
) -> anyhow::Result<EgfxCodecPolicy> {
    let value = cli_value
        .or(config_value)
        .unwrap_or_else(default_egfx_codec_policy_name);
    parse_egfx_codec_policy(&value)
}

fn default_keyboard_layout_policy_name() -> String {
    "client".into()
}

fn default_audio_mode_name() -> String {
    "redirect".into()
}

fn resolve_keyboard_layout_policy(
    cli_value: Option<String>,
    config_value: Option<String>,
) -> anyhow::Result<KeyboardLayoutPolicy> {
    let value = cli_value
        .or(config_value)
        .unwrap_or_else(default_keyboard_layout_policy_name);
    parse_keyboard_layout_policy(&value)
}

fn resolve_audio_mode(
    cli_value: Option<String>,
    config_value: Option<String>,
) -> anyhow::Result<AudioMode> {
    let value = cli_value
        .or(config_value)
        .unwrap_or_else(default_audio_mode_name);
    parse_audio_mode(&value)
}

fn resolve_headless_scale(cli: Option<f64>, config: Option<f64>) -> anyhow::Result<f64> {
    let scale = cli.or(config).unwrap_or(1.0);

    if !scale.is_finite() || scale <= 0.0 {
        anyhow::bail!("invalid scale {scale}, expected a positive number (e.g. 1, 1.5, 2)");
    }

    Ok(scale)
}

fn parse_resolution(s: &str) -> anyhow::Result<(u32, u32)> {
    let parts: Vec<&str> = s.split('x').collect();
    if parts.len() != 2 {
        anyhow::bail!("invalid resolution format, expected WxH (e.g. 1920x1080)");
    }
    let w: u32 = parts[0]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid width"))?;
    let h: u32 = parts[1]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid height"))?;
    if w == 0 || h == 0 {
        anyhow::bail!("resolution dimensions must be non-zero");
    }
    if w > u16::MAX as u32 || h > u16::MAX as u32 {
        anyhow::bail!("resolution dimensions must be <= {}", u16::MAX);
    }
    // H.264 requires even dimensions (4:2:0 chroma subsampling)
    let w = w & !1;
    let h = h & !1;
    if w == 0 || h == 0 {
        anyhow::bail!("resolution too small (minimum 2x2 for H.264)");
    }
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::fs;
    use StartupWarning::*;

    #[test]
    fn headless_scale_defaults_and_cli_overrides_config() {
        assert_eq!(resolve_headless_scale(None, None).unwrap(), 1.0);
        assert_eq!(resolve_headless_scale(None, Some(1.5)).unwrap(), 1.5);
        assert_eq!(resolve_headless_scale(Some(2.0), Some(1.5)).unwrap(), 2.0);
    }

    #[test]
    fn headless_scale_rejects_non_positive_and_non_finite_values() {
        for value in [0.0, -2.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(resolve_headless_scale(Some(value), None).is_err());
        }
    }

    #[test]
    fn cli_accepts_headless_scale() {
        let args = Args::try_parse_from(["hypr-rdp", "--scale", "1.5"]).unwrap();

        assert_eq!(args.scale, Some(1.5));
    }

    fn warnings(username: &str, password: &str, bind: &str) -> Vec<StartupWarning> {
        let credentials = ConfigCredentials::from_parts(username.to_owned(), password.to_owned());
        startup_warnings(credentials.as_ref(), parse_bind_addr(bind).unwrap())
    }

    #[test]
    fn credential_classification_preserves_every_configured_shape() {
        assert!(ConfigCredentials::from_parts(String::new(), String::new()).is_none());

        for (username, password) in [("user", "secret"), ("user", ""), ("", "secret")] {
            let credentials =
                ConfigCredentials::from_parts(username.to_owned(), password.to_owned()).unwrap();
            assert_eq!(credentials.username, username);
            assert_eq!(credentials.password, password);
        }
    }

    #[test]
    fn no_password_warns_only_beyond_canonical_loopback() {
        for bind in [
            "0.0.0.0:3389",
            "[::]:3389",
            "[::ffff:0.0.0.0]:3389",
            "192.168.1.5:3389",
        ] {
            assert_eq!(
                warnings("", "", bind),
                vec![AuthenticationOff, ReachableBeyondLoopback],
                "{bind}"
            );
        }

        for bind in ["127.0.0.1:3389", "[::1]:3389", "[::ffff:127.0.0.1]:3389"] {
            assert_eq!(warnings("", "", bind), vec![AuthenticationOff], "{bind}");
        }
    }

    #[test]
    fn half_credentials_follow_the_actual_empty_secret() {
        assert_eq!(
            warnings("user", "", "0.0.0.0:3389"),
            vec![HalfCredentials, ReachableBeyondLoopback]
        );
        assert_eq!(
            warnings("", "secret", "0.0.0.0:3389"),
            vec![HalfCredentials]
        );
        assert_eq!(
            warnings("user", "", "127.0.0.1:3389"),
            vec![HalfCredentials]
        );
        assert!(warnings("user", "secret", "0.0.0.0:3389").is_empty());
    }

    #[test]
    fn invalid_bind_address_is_rejected_by_config() {
        let error = parse_bind_addr("not an address").expect_err("invalid bind must fail");
        assert!(format!("{error:#}").contains("invalid bind address"));
    }

    #[test]
    fn file_transfer_mode_accepts_all_documented_values() {
        assert!(FileTransferMode::Both.permits_to_client());
        assert!(FileTransferMode::ToClient.permits_to_client());
        assert!(!FileTransferMode::Off.permits_to_client());
        assert!(!FileTransferMode::ToServer.permits_to_client());
        assert!(parse_file_transfer_mode("invalid").is_err());
    }

    fn temp_config_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("hypr-rdp-{name}-{}.toml", std::process::id()));
        path
    }

    #[test]
    fn explicit_missing_config_returns_error() {
        let path = temp_config_path("missing");
        let _ = fs::remove_file(&path);

        let error = ConfigFile::load(Some(path.to_str().unwrap()))
            .expect_err("explicit missing config must not fall back to defaults");

        assert!(format!("{error:#}").contains("failed to read config"));
    }

    #[test]
    fn explicit_invalid_config_returns_error() {
        let path = temp_config_path("invalid");
        fs::write(&path, "bind = [").expect("write invalid config");

        let error = ConfigFile::load(Some(path.to_str().unwrap()))
            .expect_err("explicit invalid config must not fall back to defaults");

        assert!(format!("{error:#}").contains("failed to parse config"));
        fs::remove_file(&path).expect("remove invalid config");
    }

    #[test]
    fn explicit_valid_config_loads_values() {
        let path = temp_config_path("valid");
        fs::write(
            &path,
            "bind = '127.0.0.1:3390'\nusername = 'alice'\nscale = 2\nh264_backend = 'software'\n",
        )
        .expect("write valid config");

        let config = ConfigFile::load(Some(path.to_str().unwrap())).expect("config loads");

        assert_eq!(config.bind.as_deref(), Some("127.0.0.1:3390"));
        assert_eq!(config.username.as_deref(), Some("alice"));
        assert_eq!(config.scale, Some(2.0));
        assert_eq!(config.h264_backend.as_deref(), Some("software"));
        fs::remove_file(&path).expect("remove valid config");
    }

    #[test]
    fn parses_egfx_codec_policy_values() {
        assert_eq!(
            parse_egfx_codec_policy("auto").unwrap(),
            EgfxCodecPolicy::Auto
        );
        assert_eq!(
            parse_egfx_codec_policy("avc420").unwrap(),
            EgfxCodecPolicy::Avc420
        );
        assert_eq!(
            parse_egfx_codec_policy("avc444").unwrap(),
            EgfxCodecPolicy::Avc444
        );
        assert!(parse_egfx_codec_policy("h264").is_err());
    }

    #[test]
    fn parses_keyboard_layout_policy_values() {
        assert_eq!(
            parse_keyboard_layout_policy("client").unwrap(),
            KeyboardLayoutPolicy::Client
        );
        assert_eq!(
            parse_keyboard_layout_policy("compositor").unwrap(),
            KeyboardLayoutPolicy::Compositor
        );
        assert!(parse_keyboard_layout_policy("hyprland").is_err());
    }

    #[test]
    fn parses_audio_mode_values() {
        assert_eq!(parse_audio_mode("mirror").unwrap(), AudioMode::Mirror);
        assert_eq!(parse_audio_mode("redirect").unwrap(), AudioMode::Redirect);
        assert_eq!(parse_audio_mode("off").unwrap(), AudioMode::Off);
        assert!(parse_audio_mode("local").is_err());
    }

    #[test]
    fn parses_h264_backend_policy_values() {
        assert_eq!(
            parse_h264_backend_policy("auto").unwrap(),
            H264BackendPolicy::Auto
        );
        assert_eq!(
            parse_h264_backend_policy("software").unwrap(),
            H264BackendPolicy::Software
        );
        assert_eq!(
            parse_h264_backend_policy("vaapi").unwrap(),
            H264BackendPolicy::Vaapi
        );
        assert!(parse_h264_backend_policy("hardware").is_err());
    }

    #[test]
    fn default_egfx_codec_policy_is_avc420() {
        let policy = resolve_egfx_codec_policy(None, None).unwrap();

        assert_eq!(policy, EgfxCodecPolicy::Avc420);
    }

    #[test]
    fn default_keyboard_layout_policy_uses_client_layout() {
        let policy = resolve_keyboard_layout_policy(None, None).unwrap();

        assert_eq!(policy, KeyboardLayoutPolicy::Client);
    }

    #[test]
    fn default_audio_mode_is_redirect() {
        let mode = resolve_audio_mode(None, None).unwrap();

        assert_eq!(mode, AudioMode::Redirect);
    }

    #[test]
    fn default_h264_backend_policy_is_auto() {
        assert_eq!(
            resolve_h264_backend_policy(None, None).unwrap(),
            H264BackendPolicy::Auto
        );
    }

    #[test]
    fn explicit_egfx_codec_policy_overrides_default_and_config() {
        assert_eq!(
            resolve_egfx_codec_policy(None, Some("avc444".into())).unwrap(),
            EgfxCodecPolicy::Avc444
        );
        assert_eq!(
            resolve_egfx_codec_policy(Some("avc420".into()), Some("avc444".into())).unwrap(),
            EgfxCodecPolicy::Avc420
        );
    }

    #[test]
    fn explicit_keyboard_layout_policy_overrides_default_and_config() {
        assert_eq!(
            resolve_keyboard_layout_policy(None, Some("compositor".into())).unwrap(),
            KeyboardLayoutPolicy::Compositor
        );
        assert_eq!(
            resolve_keyboard_layout_policy(Some("client".into()), Some("compositor".into()))
                .unwrap(),
            KeyboardLayoutPolicy::Client
        );
    }

    #[test]
    fn explicit_audio_mode_overrides_default_and_config() {
        assert_eq!(
            resolve_audio_mode(None, Some("redirect".into())).unwrap(),
            AudioMode::Redirect
        );
        assert_eq!(
            resolve_audio_mode(Some("off".into()), Some("redirect".into())).unwrap(),
            AudioMode::Off
        );
    }

    #[test]
    fn explicit_h264_backend_policy_overrides_config() {
        assert_eq!(
            resolve_h264_backend_policy(None, Some("software".into())).unwrap(),
            H264BackendPolicy::Software
        );
        assert_eq!(
            resolve_h264_backend_policy(Some("vaapi".into()), Some("software".into())).unwrap(),
            H264BackendPolicy::Vaapi
        );
    }

    #[test]
    fn parses_capture_mode_values() {
        assert_eq!(parse_capture_mode("wlr").unwrap(), CaptureMode::Wlr);
        assert_eq!(parse_capture_mode("ext").unwrap(), CaptureMode::Ext);
        assert!(parse_capture_mode("invalid").is_err());
    }

    #[test]
    fn cli_accepts_capture_mode_values() {
        let wlr = Args::try_parse_from(["hypr-rdp", "--capture-mode", "wlr"]).unwrap();
        assert_eq!(wlr.capture_mode.as_deref(), Some("wlr"));

        let ext = Args::try_parse_from(["hypr-rdp", "--capture-mode", "ext"]).unwrap();
        assert_eq!(ext.capture_mode.as_deref(), Some("ext"));
    }

    #[test]
    fn cli_accepts_keyboard_layout_policy_values() {
        let compositor =
            Args::try_parse_from(["hypr-rdp", "--keyboard-layout-policy", "compositor"]).unwrap();

        assert_eq!(
            compositor.keyboard_layout_policy.as_deref(),
            Some("compositor")
        );
    }

    #[test]
    fn cli_accepts_audio_mode_values() {
        let redirect = Args::try_parse_from(["hypr-rdp", "--audio-mode", "redirect"]).unwrap();

        assert_eq!(redirect.audio_mode.as_deref(), Some("redirect"));
    }

    #[test]
    fn cli_accepts_h264_backend_option() {
        let args = Args::try_parse_from(["hypr-rdp", "--h264-backend", "software"]).unwrap();

        assert_eq!(args.h264_backend.as_deref(), Some("software"));
    }

    #[test]
    fn cli_accepts_session_hook_commands() {
        let args = Args::try_parse_from([
            "hypr-rdp",
            "--on-session-start",
            "start command",
            "--on-session-end",
            "end command",
        ])
        .unwrap();

        assert_eq!(args.on_session_start.as_deref(), Some("start command"));
        assert_eq!(args.on_session_end.as_deref(), Some("end command"));
    }

    proptest! {
        #[test]
        fn generated_resolution_parser_rounds_even_dimensions_or_rejects_too_small(
            width in 0u32..=u16::MAX as u32,
            height in 0u32..=u16::MAX as u32,
        ) {
            let parsed = parse_resolution(&format!("{width}x{height}"));
            let expected_width = width & !1;
            let expected_height = height & !1;

            if expected_width >= 2 && expected_height >= 2 {
                prop_assert_eq!(parsed.unwrap(), (expected_width, expected_height));
            } else {
                prop_assert!(parsed.is_err());
            }
        }

        #[test]
        fn generated_resolution_parser_rejects_dimensions_above_wire_limit(
            wide in (u16::MAX as u32 + 1)..=u32::MAX,
            high in (u16::MAX as u32 + 1)..=u32::MAX,
            valid in 2u32..=u16::MAX as u32,
        ) {
            let wide_resolution = format!("{wide}x{valid}");
            let high_resolution = format!("{valid}x{high}");

            prop_assert!(parse_resolution(&wide_resolution).is_err());
            prop_assert!(parse_resolution(&high_resolution).is_err());
        }

        #[test]
        fn generated_policy_parsers_accept_only_documented_exact_tokens(
            token in "[a-z0-9_-]{0,16}"
        ) {
            match token.as_str() {
                "auto" | "avc420" | "avc444" => {
                    prop_assert!(parse_egfx_codec_policy(&token).is_ok());
                }
                _ => {
                    prop_assert!(parse_egfx_codec_policy(&token).is_err());
                }
            }

            match token.as_str() {
                "wlr" | "ext" => {
                    prop_assert!(parse_capture_mode(&token).is_ok());
                }
                _ => {
                    prop_assert!(parse_capture_mode(&token).is_err());
                }
            }

            match token.as_str() {
                "vbr" | "cqp" => {
                    prop_assert!(parse_rate_control(&token).is_ok());
                }
                _ => {
                    prop_assert!(parse_rate_control(&token).is_err());
                }
            }

            match token.as_str() {
                "client" | "compositor" => {
                    prop_assert!(parse_keyboard_layout_policy(&token).is_ok());
                }
                _ => {
                    prop_assert!(parse_keyboard_layout_policy(&token).is_err());
                }
            }

            match token.as_str() {
                "mirror" | "redirect" | "off" => {
                    prop_assert!(parse_audio_mode(&token).is_ok());
                }
                _ => {
                    prop_assert!(parse_audio_mode(&token).is_err());
                }
            }

            match token.as_str() {
                "auto" | "software" | "vaapi" => {
                    prop_assert!(parse_h264_backend_policy(&token).is_ok());
                }
                _ => {
                    prop_assert!(parse_h264_backend_policy(&token).is_err());
                }
            }
        }
    }
}
