# hypr-rdp

Native RDP server for Hyprland. Connect to your Hyprland desktop from an RDP client.

## Features

- **H.264/EGFX** — AVC420 by default, experimental AVC444 support, and VA-API acceleration with automatic software fallback
- **Screen capture** — `wlr-screencopy-v1` and `ext-image-copy-capture-v1` protocols
- **Audio** — PipeWire audio forwarding via RDPSND
- **Clipboard** — Bidirectional text and image clipboard sync
- **File transfer** — Copy files and folders in a Hyprland file manager and paste them
  into the client's, over the clipboard channel. See "File transfer"
- **Input** — Full keyboard and mouse support via virtual keyboard/pointer protocols
- **Session hooks** — Run a command when a client session starts and ends
- **TLS** — Auto-generated self-signed RSA-2048 certificates, or bring your own. Existing
  `~/.config/hypr-rdp/cert.pem` and `key.pem` files are reused; delete both to regenerate them.
- **Config file** — `~/.config/hypr-rdp/config.toml`

## Installation

### AUR (Arch Linux)

```sh
# Stable release
yay -S hypr-rdp

# Latest git build
yay -S hypr-rdp-git
```

### Nix

```sh
# Run from GitHub
nix run github:MuNeNICK/hypr-rdp#hypr-rdp -- --help

# Build from GitHub
nix build github:MuNeNICK/hypr-rdp#hypr-rdp

# Development shell
nix develop github:MuNeNICK/hypr-rdp#hypr-rdp
```

### Prebuilt binary

Download from [GitHub Releases](https://github.com/MuNeNICK/hypr-rdp/releases):

```sh
tar xzf hypr-rdp-v*.tar.gz
sudo install -Dm755 hypr-rdp /usr/local/bin/hypr-rdp
```

Runtime dependencies: `ffmpeg`/`libavcodec`, `libva`, `pipewire`, `libxkbcommon`,
and `pactl` through PipeWire's PulseAudio compatibility layer for the default
remote-audio routing mode.

For VA-API hardware encoding, install a VA-API driver such as
`intel-media-driver` for Intel GPUs or `libva-mesa-driver` for AMD GPUs.

### Build from source

Requirements:
- Rust 1.75+
- `ffmpeg`/`libavcodec`, `libva`, `pipewire`, `libxkbcommon` (development headers)

```sh
git clone https://github.com/MuNeNICK/hypr-rdp.git
cd hypr-rdp
cargo build --release
sudo install -Dm755 target/release/hypr-rdp /usr/local/bin/hypr-rdp
```

## Usage

Requires **Hyprland 0.54+**.
VA-API is included in the standard build and falls back to software encoding
automatically when unavailable.

Run hypr-rdp as the same user as Hyprland with `WAYLAND_DISPLAY` set. When
`HYPRLAND_INSTANCE_SIGNATURE` is absent, hypr-rdp selects the live Hyprland
instance whose lock file names that Wayland display. Set the signature
explicitly if discovery reports no unique match.

```sh
# Basic (auto-generates TLS cert, binds to 127.0.0.1:3389)
hypr-rdp -u <username> -p <password>

# Bind to all interfaces
hypr-rdp -u user -p pass --bind 0.0.0.0:3389

# Custom resolution and framerate
hypr-rdp -u user -p pass --resolution 2560x1440 --fps 60

# HiDPI client: native panel pixels, scaled so the UI stays legible
hypr-rdp -u user -p pass --resolution 3024x1896 --scale 2

# Capture a specific output
hypr-rdp -u user -p pass --output DP-1

# Use ext-image-copy-capture protocol
hypr-rdp -u user -p pass --capture-mode ext
```

### Config file

`~/.config/hypr-rdp/config.toml`:

```toml
bind = "0.0.0.0:3389"
username = "user"
password = "pass"
# password_file = "/run/secrets/hypr-rdp-password"  # conflicts with `password`
# resolution = "1920x1080"
# scale = 1
capture_mode = "wlr"
bitrate = 10000000
quality = 23
fps = 30
egfx_codec = "avc420"
# h264_backend = "auto" # auto, software, or vaapi
# audio_mode = "redirect"
# keyboard_layout_policy = "client"
# output = "DP-1"
# on_session_start = "hyprctl dispatch dpms off eDP-1"  # see "Session hooks"
# on_session_end = "hyprctl dispatch dpms on eDP-1"
# file_transfer_mode = "to-client"         # see "File transfer"
# file_transfer_max_entries = 10000
# file_transfer_max_chunk_bytes = 8388608
```

CLI arguments override config file values. `--password`/`--password-file` and
`password`/`password_file` are each mutually exclusive within their source;
supplying either one on the CLI overrides whichever password source (literal
or file) is set in the config file. A selected password file is read once at
startup, with exactly one trailing line ending (`\n` or `\r\n`) removed; if it
is missing, unreadable, or empty after that, hypr-rdp fails to start rather
than falling back to unauthenticated operation.

Setting only `-u`/`--username`/`username` or only
`-p`/`--password`/`--password-file`/`password`/`password_file` (not both) is
accepted: the missing half is treated as empty, hypr-rdp logs a warning, and
the client must match that empty value to authenticate.

### Session hooks

`on_session_start` and `on_session_end` run a shell command when a client
session begins and ends:

```toml
on_session_start = "hyprctl dispatch dpms off eDP-1"
on_session_end = "hyprctl dispatch dpms on eDP-1"
```

- Only a fully established session runs a command: port probes, TLS scanners
  and rejected logins never do, and a client resize does not re-run the start
  command. With `-u`/`-p` set the client must pass NLA first; without
  credentials there is no authentication step, so any client that completes
  the RDP handshake runs the command.
- Configured commands run in session order: each waits for the previous one to
  finish, for up to 10 seconds. An unconfigured boundary does not release a
  running command, so a fast reconnect cannot overtake it. Past the deadline
  the previous command is left running, the next one starts alongside it, and
  a warning is logged.
- Stopping hypr-rdp during a session attempts to run the end command and waits
  for it within the same 10-second budget. A command that fails to start,
  exits unsuccessfully, or outlives the deadline cannot guarantee that the
  corresponding start action is undone.
- Commands run through `/bin/sh -c` as the same user as hypr-rdp, with its
  environment and working directory. The selected
  `HYPRLAND_INSTANCE_SIGNATURE` is supplied so `hyprctl` targets the same
  compositor. Shell profiles are not read — use absolute paths for anything
  outside the inherited `PATH`.
  hypr-rdp never kills a command; one still running at exit is left to the
  service manager. Hook command text is not written to hypr-rdp's logs.

Name the monitor when blanking a screen. A bare `hyprctl dispatch dpms off`
also blanks the `hypr-rdp-*` headless output the session is rendered on, which
blanks the session itself. Blanking the captured output is worse still: while
it is off the compositor stops committing frames, so the session freezes, and
remote input wakes it again unless `misc:mouse_move_enables_dpms` and
`misc:key_press_enables_dpms` are disabled.

### File transfer

Files move over the clipboard channel the session already negotiates — there is nothing
extra to launch and no port to open. Select files or folders in any Hyprland file manager,
press Ctrl+C, and paste them into the RDP client's file manager. It is on by default
when the client negotiates file streaming; clients without that capability retain
text and image clipboard sync. Files larger than 4 GiB minus one byte additionally
require the client's huge-file capability.

**A cut is always a copy.** Ctrl+X and Ctrl+C behave identically: the files arrive on the
client and the originals stay exactly where they were. hypr-rdp never deletes a source
file, so a transfer that half-succeeds can never lose your only copy.

Copying a folder includes its subdirectories, including empty ones, within the limits
below. Symbolic links are followed and arrive as the file they point at. Sockets, FIFOs and device nodes are skipped
and logged rather than transferred. A directory tree containing a symlink cycle is
detected and terminates rather than looping.

Filenames are adjusted so they land on a client filesystem that accepts fewer names than
Linux does. Characters Windows forbids (`< > : " / \ | ? *` and C0 control characters)
become underscores, trailing dots and spaces are trimmed, reserved device names like `CON`
and `LPT1` gain a trailing underscore, and names that are not valid UTF-8 are decoded
lossily. Two names in the same directory that collide after adjustment are disambiguated —
`a:b.txt` and `a?b.txt` become `a_b.txt` and `a_b (2).txt` — so neither silently overwrites
the other. Names are compared case-insensitively, as Windows compares them, so `README.txt`
alongside `readme.txt` is also disambiguated even though neither name needed adjusting.
Relative paths longer than 259 UTF-16 code units cannot fit the protocol's file descriptor;
those entries are skipped with a warning (including the subtree of an overlong directory).
Other valid entries keep their names and contents paired correctly. Because the RDP
clipboard gives the server no way to learn the client's operating system, this adjustment is applied for every
client.

Enumeration and reads run on a worker thread, so copying a large tree does not stall
video, audio or input. Files are read in ranges on demand and never buffered whole, so a
multi-gigabyte file does not grow the server's memory. A client that asks for reads faster
than the filesystem answers is refused the excess rather than allowed to queue without
limit.

Paths are only ever taken from a selection the desktop user themselves put on the
clipboard — never from anything the client sends — and an open handle retains each
selected file's identity until the selection and its accepted reads are released. A path swapped out between the copy and the paste is refused
rather than served under the old name. This does not snapshot the file's bytes: edits to
the same underlying file may be visible during transfer. Files that cannot be opened,
including when the process reaches its open-file limit, are skipped with a warning.

#### Settings

| Key | Values | Default | Meaning |
|------|-------------|---------|---------|
| `file_transfer_mode` | `off`, `to-client` | `to-client` | Whether files a Hyprland file manager copies may be transferred to the client. Text and image clipboard sync are unaffected by either value |
| `file_transfer_max_entries` | 1 to 100000 | `10000` | How many files and directories one Hyprland copy may enumerate. Over the limit is truncated and logged rather than failing the copy. 100000 is the clipboard protocol's own ceiling and is rejected if exceeded |
| `file_transfer_max_chunk_bytes` | bytes | `8388608` | Largest read a single client request may ask of the desktop. This is the bound on how much memory one request can make the server allocate; a request above it is refused |

Every key is also a command-line flag (`--file-transfer-mode` and so on).

The entry limit bounds inspection as well as the advertised list. Skipped entries
(including special files, cycles, and unreadable entries) consume that budget too, so a
truncated offer can contain fewer files than the limit. A directory is read only up to the
remaining budget plus one overflow check; the subset retained at the limit depends on
filesystem order.

`off` stops files being *transferred*, but not the selection's paths: a file copied in a
Hyprland file manager still reaches the client's clipboard as **text** — the `file://` URI
list published as ordinary text when it cannot be offered as files. Paths cross even
though contents do not.

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `--bind`, `-b` | Bind address | `127.0.0.1:3389` |
| `--cert` | TLS certificate (PEM) | Auto-generated, RSA-2048 |
| `--key` | TLS private key (PEM) | Auto-generated |
| `-u`, `--username` | RDP username | _(none)_ |
| `-p`, `--password` | RDP password. Conflicts with `--password-file`. | _(none)_ |
| `--password-file` | Read the RDP password from a file, with one trailing line ending removed. Conflicts with `--password`. | _(none)_ |
| `--resolution`, `-r` | Fixed session resolution, used as-is including above a captured output's size. When omitted for a managed headless output, the session starts at `1920x1080` and may resize to the client-requested size. | Auto client size |
| `--scale` | Scale of the managed headless output, e.g. `2` for a HiDPI client. Hyprland only accepts scales that divide the mode into whole logical pixels. Ignored when `--output` captures an existing monitor. | `1` |
| `--capture-mode` | `wlr` or `ext` | `wlr` |
| `--bitrate` | H.264 bitrate (bps) | `10000000` |
| `--quality` | H.264 quality (0-51) | `23` |
| `--rate-control` | H.264 rate control: `vbr` or `cqp` | `vbr` |
| `--fps` | Max framerate | `30` |
| `--max-frames-in-flight` | Max unacknowledged EGFX frames | `3` |
| `--egfx-codec` | EGFX codec policy: `avc420`, experimental `avc444`, or `auto` | `avc420` |
| `--h264-backend` | H.264 backend: `auto` tries VA-API then software, `software` avoids VA-API, `vaapi` never substitutes software H.264 | `auto` |
| `--audio-mode` | Audio policy: `redirect` routes playback to a temporary RDP sink while connected, `mirror` captures the current sink audio, `off` disables RDPSND | `redirect` |
| `--keyboard-layout-policy` | Keyboard layout policy: `client` applies the RDP client layout; `compositor` keeps the compositor/Hyprland keymap | `client` |
| `--output` | Specific output name. Automatic sizing does not magnify the captured content; one presentation axis may remain larger for letterboxing. | _(headless)_ |
| `--on-session-start` | Shell command run when an authenticated session starts | _(none)_ |
| `--on-session-end` | Shell command run when the session ends | _(none)_ |
| `--file-transfer-mode` | Clipboard file transfer policy: `off` or `to-client`. See "File transfer" | `to-client` |
| `--file-transfer-max-entries` | Maximum files and directories one Hyprland copy may enumerate, at most `100000` | `10000` |
| `--file-transfer-max-chunk-bytes` | Maximum bytes the client may ask for in one file-content range request | `8388608` |
| `--config` | Config file path | `~/.config/hypr-rdp/config.toml` |

## License

MIT
