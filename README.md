# hypr-rdp

Native RDP server for Hyprland.

- H.264 video with VA-API acceleration and automatic software fallback
- PipeWire audio forwarding
- Keyboard and mouse input
- Bidirectional text and image clipboard sync
- Clipboard file transfer from the desktop to the client
- TLS certificates and optional session hooks

Requires **Hyprland 0.54+**. AVC420 is the default codec; AVC444 is experimental.

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

Runtime dependencies: `ffmpeg`/`libavcodec`, `libva`, `pipewire`, `libxkbcommon`,
and `pactl` for the default audio routing mode. Hardware encoding also needs
an appropriate VA-API driver.

### Build from source

Install a current stable Rust toolchain and development headers for FFmpeg,
libva, PipeWire, libxkbcommon, Wayland, and GBM.

```sh
git clone https://github.com/MuNeNICK/hypr-rdp.git
cd hypr-rdp
cargo build --release --locked
sudo install -Dm755 target/release/hypr-rdp /usr/local/bin/hypr-rdp
```

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

### File transfer

Copy files or folders in a Hyprland file manager, then paste into the client's
file manager. The client must support clipboard file streaming. Transfer from
the client to the desktop is not supported yet.

- Cut acts as copy; source files are never deleted.
- Names are adjusted for Windows compatibility. Unreadable files and overlong
  paths are skipped; large selections may be truncated at the entry limit.
- Keep source files unchanged until the transfer finishes.

| Config key | Default | Purpose |
| --- | --- | --- |
| `file_transfer_mode` | `"to-client"` | Set to `"off"` to disable file contents transfer |
| `file_transfer_max_entries` | `10000` | Selection entry budget, including skipped entries; maximum `100000` |
| `file_transfer_max_chunk_bytes` | `8388608` | Maximum bytes per read request |

These settings also have CLI flags, such as `--file-transfer-mode off`.
Disabling file transfer leaves text/image sync enabled; copied file paths may
still appear on the client's clipboard as text.

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
