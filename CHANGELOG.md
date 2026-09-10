# Changelog

## [0.1.6] - 2026-09-10

### Added

- Added clipboard file transfer in both directions: files from the RDP client are exposed through a bounded FUSE view, and files from the Hyprland desktop can be copied to the client, including directories within the configured entry and protocol path-length limits.
- Added `file_transfer_mode`, `file_transfer_max_entries`, and `file_transfer_max_chunk_bytes` settings, each also a command-line flag.
- Added outbound filename adjustment so names illegal on the client's filesystem still arrive, with collisions disambiguated.
- Added ClearCodec support for clients that do not negotiate AVC.
- Added session hooks that run on authenticated session boundaries.
- Added `--password-file` / `password_file` for supplying NLA credentials from a file.
- Added `--scale` for the managed headless output.
- Enabled upstream authenticated session replacement for connections with configured NLA credentials; TLS-only connections retain their existing queue policy.

### Changed

- Replaced FFmpeg with bundled OpenH264 for software H.264 encoding. AVC420 retains native VA-API acceleration; experimental AVC444 currently uses software encoding.
- Switched the IronRDP dependency from the local fork to upstream IronRDP and adopted the connection-info hook.

### Fixed

- Added bounded capture retries and graphics-pipeline waits, EGFX activation fallback, and validation of implicit DMA-BUF fallback.
- Fixed H.264 level, IDR numbering, and coded-buffer overflow handling.
- Fixed initial-size headless resize handling so resize ownership survives cancellation.
- Fixed server session handling: a second connection is rejected instead of hanging, a dead peer no longer holds the session slot indefinitely, and malformed pre-auth connections recover.
- Fixed input handling: horizontal scroll is no longer inverted, sub-detent scroll deltas accumulate, keys left held by the client are released, repeated modifiers no longer stick, and keyboard state is restored on synchronize.
- Fixed audio teardown and routing so deadlines bound every pactl call and streams that start during redirect are routed to the remote sink.
- Fixed TLS certificate generation to use an RSA key and preserve existing certificate identities.
- Fixed Hyprland instance discovery to align with hyprctl.
- Fixed VA-API VBR quality ceiling, VPP display ownership, pipeline parameter buffer handling, and DMA-BUF import format descriptors.
- Fixed clipboard echo suppression, line-ending normalization, duplicate format request coalescing, offer lifecycle, and pipe transfer deadlines.
- Fixed EGFX surface reinitialization after a capability reset and AVC444 capability refresh without resetting the codec.
- Fixed startup security warnings for reachability and half-set credentials.
- Fixed Rust 1.98 clippy lint compliance.

## [0.1.5] - 2026-08-19

### Fixed

- Fixed PipeWire initialization across repeated RDP sessions so later sessions can create the redirect sink and capture streams.
- Fixed RDPSND Wave2 timestamps to use boot-time milliseconds instead of advancing by audio frames.
- Fixed the compositor keyboard layout policy so startup state and later Hyprland layout switches converge without being reverted by modifier updates.

## [0.1.4] - 2026-08-18

### Added

- Added explicit `auto`, `software`, and `vaapi` H.264 backend selection through `--h264-backend` and `h264_backend`.

### Changed

- Improved presentation downscaling quality with area filtering.
- Updated the FFmpeg binding for FFmpeg 9 compatibility.

### Fixed

- Fixed physical-output size negotiation so the presentation size converges instead of repeatedly requesting the same layout.
- Fixed VA-API packed-header submission for drivers that require sequence, picture, and slice headers.
- Fixed VA-API rate control so zero-bitrate configurations do not emit invalid HRD parameters.
- Fixed clipboard echo suppression so only the matching remote copy is suppressed without dropping later local updates.

## [0.1.3] - 2026-06-25

### Added

- Added physical output downscaling for managed headless RDP sessions.
- Added NLA acceptor support when username/password credentials are configured.
- Added remote audio routing modes so captured audio can be redirected to the RDP client sink.

### Changed

- Changed the default audio routing mode to `redirect` so RDP session audio is not played locally by default.

### Fixed

- Fixed PipeWire audio capture to honor valid chunk offset/size metadata, including wrapped ranges and corrupted chunks.
- Fixed keyboard input handling so layout state is preserved across client layout policy changes.
- Hardened physical output downscaling resize/aspect handling and package runtime dependencies.

## [0.1.2] - 2026-06-16

### Added

- Added `keyboard_layout_policy = "compositor"` / `--keyboard-layout-policy compositor` to keep the compositor/Hyprland keymap instead of applying the RDP client's keyboard layout.
- Added a Nix flake and Nix package definition.

### Fixed

- Fixed Hyprland 0.55+/Lua config parser compatibility by falling back from `keyword monitor` to `eval hl.monitor(...)` when setting managed headless output resolutions.
- Updated runtime dependencies and package metadata after the 0.1.1 release.

### Tests

- Added regression tests for Hyprland Lua monitor command generation and non-legacy parser error detection.
- Added regression tests for the compositor keyboard layout policy and config parsing.

## [0.1.1] - 2026-05-26

### Changed

- Reworked the display encoding path around FFmpeg/libavcodec and improved protocol compliance.
- Added AVC420 VA-API connection validation with Windows Remote Desktop clients.

## [0.1.0] - 2026-03-15

### Added

- Initial public release.

[0.1.6]: https://github.com/MuNeNiCK/hypr-rdp/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/MuNeNiCK/hypr-rdp/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/MuNeNICK/hypr-rdp/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/MuNeNICK/hypr-rdp/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/MuNeNICK/hypr-rdp/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/MuNeNICK/hypr-rdp/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/MuNeNICK/hypr-rdp/releases/tag/v0.1.0
