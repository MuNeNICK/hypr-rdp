# Changelog

## [Unreleased]

### Added

- Added clipboard file transfer from the Hyprland desktop to the RDP client, carrying whole directory trees.
- Added `file_transfer_mode`, `file_transfer_max_entries`, and `file_transfer_max_chunk_bytes` settings, each also a command-line flag.
- Added outbound filename adjustment so names illegal on the client's filesystem still arrive, with collisions disambiguated.

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

[0.1.5]: https://github.com/MuNeNiCK/hypr-rdp/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/MuNeNICK/hypr-rdp/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/MuNeNICK/hypr-rdp/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/MuNeNICK/hypr-rdp/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/MuNeNICK/hypr-rdp/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/MuNeNICK/hypr-rdp/releases/tag/v0.1.0
