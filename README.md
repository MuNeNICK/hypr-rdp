# hypr-rdp

Native RDP server for Hyprland.

- H.264 video with VA-API acceleration and automatic software fallback
- PipeWire audio forwarding
- Keyboard and mouse input
- Bidirectional text and image clipboard sync
- Bidirectional clipboard file transfer
- TLS certificates and optional session hooks

Requires **Hyprland 0.54+**. AVC420 is the default codec; AVC444 is experimental and currently uses software encoding.

## Installation

### Arch Linux (AUR)

```sh
yay -S hypr-rdp       # Stable release
yay -S hypr-rdp-git   # Latest git build
```

### Nix

```sh
nix run github:MuNeNICK/hypr-rdp#hypr-rdp -- --help
nix build github:MuNeNICK/hypr-rdp#hypr-rdp
```

Use `nix develop github:MuNeNICK/hypr-rdp#hypr-rdp` for a development shell.

### Prebuilt binary

Download from [GitHub Releases](https://github.com/MuNeNICK/hypr-rdp/releases):

```sh
tar xzf hypr-rdp-v*.tar.gz
sudo install -Dm755 hypr-rdp /usr/local/bin/hypr-rdp
```

Runtime dependencies: `libva`, `pipewire`, `libxkbcommon`,
and `pactl` for the default audio routing mode. Hardware encoding also needs
an appropriate VA-API driver. The software encoder uses bundled OpenH264.
Receiving clipboard files on the desktop also needs FUSE and `fusermount3`
(Arch: `fuse3`).

### Build from source

Install a current stable Rust toolchain, a C/C++ compiler, and development headers
for libva, PipeWire, libxkbcommon, Wayland, and GBM.

```sh
git clone https://github.com/MuNeNICK/hypr-rdp.git
cd hypr-rdp
cargo build --release --locked
sudo install -Dm755 target/release/hypr-rdp /usr/local/bin/hypr-rdp
```

Inbound clipboard file transfer requires the `client-to-server` Cargo feature,
enabled by default. To use software encoding with inbound transfer, build with
`--no-default-features --features client-to-server`.

## Quick start

Run as the same user as Hyprland, inside its session:

```sh
hypr-rdp -u user -p pass --bind 0.0.0.0:3389
```

Connect your RDP client to the machine's address on port 3389. Without `--bind`,
hypr-rdp listens on `127.0.0.1:3389`. A self-signed TLS certificate is generated
on first start; use `--cert` and `--key` to supply your own.

By default, hypr-rdp creates a headless output sized for the client. To capture
an existing monitor or set a fixed resolution:

```sh
hypr-rdp -u user -p pass --output DP-1
hypr-rdp -u user -p pass --resolution 2560x1440 --fps 60
hypr-rdp -u user -p pass --resolution 3024x1896 --scale 2
```

`--scale` applies only to the headless output. When starting outside the desktop
session, set `WAYLAND_DISPLAY`; set `HYPRLAND_INSTANCE_SIGNATURE` too if instance
discovery is ambiguous.

With credentials configured, a new authenticated connection replaces the current
one. Without credentials, connections are unauthenticated and served one at a time.

## Configuration

Create `~/.config/hypr-rdp/config.toml`:

```toml
bind = "0.0.0.0:3389"
username = "user"
password = "pass"
fps = 30
# resolution = "1920x1080"
# scale = 2
# output = "DP-1"
# audio_mode = "mirror"
# keyboard_layout_policy = "compositor"
```

CLI arguments override the config file. Use `password_file` instead of `password`
to read a password from a file, or `--password-file` on the command line.

Common settings are listed below. Config keys use underscores in place of hyphens.
Run `hypr-rdp --help` for all options.

| Option | Values / purpose | Default |
| --- | --- | --- |
| `--capture-mode` | `wlr` or `ext` capture protocol | `wlr` |
| `--egfx-codec` | `avc420`, experimental `avc444`, or `auto` | `avc420` |
| `--h264-backend` | `auto`, `software`, or `vaapi` | `auto` |
| `--bitrate` | Video bitrate in bits/s | `10000000` |
| `--quality` | H.264 quality, 0–51 (lower is better) | `23` |
| `--rate-control` | `vbr` or `cqp` | `vbr` |
| `--fps` | Maximum frame rate | `30` |
| `--audio-mode` | `redirect` to RDP, `mirror` local playback, or `off` | `redirect` |
| `--keyboard-layout-policy` | `client` layout or existing `compositor` keymap | `client` |
| `--config` | Config file path | `~/.config/hypr-rdp/config.toml` |
| `--file-transfer-mode` | `both`, `to-client`, `to-server`, or `off`; directions are relative to the RDP client | `both` |
| `--file-transfer-max-entries` | Selection entry budget, including skipped entries; maximum `100000` | `10000` |
| `--file-transfer-max-chunk-bytes` | Maximum bytes per read request | `8388608` |

Clipboard file transfer copies files or folders in either file manager and
pastes them into the other; the RDP client must support clipboard file
streaming. Cut acts as copy and source files are never deleted, so keep them
unchanged until the transfer finishes. Wait for a transfer to finish before
copying another selection.

### Session hooks

Run commands when a session starts and ends:

```toml
on_session_start = "hyprctl dispatch dpms off eDP-1"
on_session_end = "hyprctl dispatch dpms on eDP-1"
```

Commands run as the hypr-rdp user through `/bin/sh -c`, after session establishment
and on disconnect (including service stop). They run in order, waiting up to
10 seconds for the previous command; timed-out commands are not killed.

For screen blanking, name a local monitor that is not being captured. Blanking
all outputs or the captured output also interrupts the remote display.

## License

MIT
