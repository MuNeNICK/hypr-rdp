# hypr-rdp

Native RDP server for Hyprland. Connect to your Hyprland desktop from an RDP client.

## Features

- **H.264/EGFX** — AVC420 by default, experimental AVC444 support, and VA-API acceleration with automatic software fallback
- **Screen capture** — `wlr-screencopy-v1` and `ext-image-copy-capture-v1` protocols
- **Audio** — PipeWire audio forwarding via RDPSND
- **Clipboard** — Bidirectional text and image clipboard sync
- **File transfer** — Copy files and folders in a Hyprland file manager and paste them into
  the client's, and paste the client's files back onto the desktop, over the clipboard
  channel. See "File transfer"
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
remote-audio routing mode. Pasting files from the client also needs `fusermount3`
(Arch: `fuse3`); without it that one direction warns and is skipped.

For VA-API hardware encoding, install a VA-API driver such as
`intel-media-driver` for Intel GPUs or `libva-mesa-driver` for AMD GPUs.

### Build from source

Requirements:
- Rust 1.75+
- `ffmpeg`/`libavcodec`, `libva`, `pipewire`, `libxkbcommon` (development headers)

The default-on `client-to-server` feature adds no build-time dependency — its FUSE
requirement is a runtime one. See "Pasting from the client".

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
# file_transfer_mode = "both"              # see "File transfer"
# file_transfer_max_entries = 10000
# file_transfer_max_chunk_bytes = 8388608
```

CLI arguments override config file values.

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

Files move both ways over the clipboard channel the session already negotiates — there is
nothing extra to launch and no port to open. Select files or folders in any Hyprland file
manager, press Ctrl+C, and paste them into the RDP client's file manager; copy files on
the client and paste them onto the Hyprland desktop. Both directions are on by default.

**A cut is always a copy.** Ctrl+X and Ctrl+C behave identically, in both directions: the
files arrive on the other side and the originals stay exactly where they were. hypr-rdp
never deletes a source file and never asks the client to, so a transfer that half-succeeds
can never lose your only copy.

#### Copying to the client

Copying a folder copies its whole tree, empty subdirectories included. Symbolic links are
followed and arrive as the file they point at. Sockets, FIFOs and device nodes are skipped
and logged rather than transferred. A directory tree containing a symlink cycle is
detected and terminates rather than looping.

Filenames are adjusted so they land on a client filesystem that accepts fewer names than
Linux does. Characters Windows forbids (`< > : " / \ | ? *` and C0 control characters)
become underscores, trailing dots and spaces are trimmed, reserved device names like `CON`
and `LPT1` gain a trailing underscore, and names that are not valid UTF-8 are decoded
lossily. Two names in the same directory that collide after adjustment are disambiguated —
`a:b.txt` and `a?b.txt` become `a_b.txt` and `a_b (2).txt` — so neither silently overwrites
the other. Names are compared case-insensitively, as Windows compares them, so `README.txt`
alongside `readme.txt` is also disambiguated even though neither name needed adjusting. No
single awkward name ever fails the rest of the paste. Because the RDP clipboard gives the
server no way to learn the client's operating system, this adjustment is applied for every
client.

Enumeration and reads run on a worker thread, so copying a large tree does not stall
video, audio or input. Files are read in ranges on demand and never buffered whole, so a
multi-gigabyte file does not grow the server's memory.

Paths are only ever taken from a selection the desktop user themselves put on the
clipboard — never from anything the client sends — and a file swapped out between the copy
and the paste is detected and refused rather than served under the old name.

#### Pasting from the client

Files copied on the client paste onto the Hyprland desktop through a read-only FUSE mount
of the client's selection, so the paste completes at once and the bytes stream as the file
is read rather than downloading first. Your file manager copies out of that mount the way
it copies out of any other directory.

The selection is offered to Wayland as both `text/uri-list` and
`x-special/gnome-copied-files`, so GNOME-derived file managers (Nautilus, Nemo, Caja) and
non-GNOME ones (Dolphin, Thunar, PCManFM) all see it. Names arrive as the client sent them
and are not adjusted; entries the client names with `.`, `..`, an embedded NUL, or a path
deeper than the outbound walk would go are dropped and logged.

Because the bytes come from the client on demand, **the mount only works while the session
is connected**. A copy still running when the session ends, or when the client stops
answering, fails with an ordinary I/O error rather than hanging:

- Each range read waits at most 30 seconds for the client before failing that read. A
  large copy has more than one read in flight and the kernel reissues after a failure, so
  the copying process itself sees the error after a small multiple of that.
- A change of the client's clipboard fails every read still waiting at once, without
  waiting out those 30 seconds.
- A session that ends also fails them, but currently by letting them time out rather than
  immediately, so a copy can sit for up to about a minute before it reports the error.
- The mount is unmounted and its directory removed when the session ends. One left behind
  by a server that was killed is swept away the next time hypr-rdp starts, so a bad exit
  does not leave a broken directory in your runtime directory.

The mount is read-only. You paste *out* of it; nothing can be written into it, and it is
not a way to put files onto the client.

**Copying anything on the Hyprland desktop ends the paste you have not finished.** The
client hands its file list over once per copy and discards it as soon as the desktop
announces a clipboard of its own, so files from an earlier client copy start returning I/O
errors even though the mount still lists them. Copy again on the client and the mount
refreshes. Finish a large paste before you copy something else on the desktop.

This direction is the `client-to-server` build feature, **enabled by default**. It is
optional because it needs FUSE at runtime, which not every machine offers:

- The `fusermount3` setuid helper — on Arch, the `fuse3` package, which the AUR recipes
  depend on.
- On NixOS, that helper is a setuid wrapper at `/run/wrappers/bin/fusermount3`, built only
  when **`programs.fuse.enable = true;`** is set. That option is on by default on NixOS
  25.11 but off on `nixos-unstable`, so an upgrade can take the helper away. The Nix
  package points `FUSERMOUNT_PATH` at that wrapper; set it yourself to reach the helper
  anywhere else it lives.
- Nothing in `/etc/fuse.conf` needs changing, and `allow_other` is never used: the mount is
  private to the user running hypr-rdp, under a mode-0700 directory in `XDG_RUNTIME_DIR`.

None of this is a *build* dependency. The FUSE binding is pure Rust, so building needs no
libfuse headers and no pkg-config entry. To build without the direction at all:

```sh
cargo build --release --no-default-features --features vaapi
```

Neither absence stops the server. A `file_transfer_mode` of `to-server` or `both` on a
build without the feature continues as `to-client` — warning if you asked for it by name —
so a stale config file never costs you a session. A mount that cannot be created at
runtime — no kernel module, no helper, no usable runtime directory — is warned about and
the paste is skipped; the session, the other direction, and text and image clipboard sync
all keep working.

#### Settings

| Key | Values | Default | Meaning |
|------|-------------|---------|---------|
| `file_transfer_mode` | `off`, `to-client`, `to-server`, `both` | `both` | Which directions are permitted: `to-client` is desktop to client, `to-server` is client to desktop. Text and image clipboard sync are unaffected by every value |
| `file_transfer_max_entries` | 1 to 100000 | `10000` | How many files and directories one selection may hold, in **both** directions: what a Hyprland copy enumerates, and what a client's file list may describe. Over the limit is truncated and logged rather than failing. 100000 is the clipboard protocol's own ceiling and is rejected if exceeded |
| `file_transfer_max_chunk_bytes` | bytes | `8388608` | Largest read a single client request may ask of the desktop. This is the bound on how much memory one request can make the server allocate; a request above it is refused. It does not apply to the other direction, where the kernel sizes the reads |

Every key is also a command-line flag (`--file-transfer-mode` and so on).

A mode that excludes a direction stops files being *transferred* that way, but not the
selection's paths, in either direction. A file copied in a Hyprland file manager still
reaches the client's clipboard as **text** — the `file://` URI list published as ordinary
text when it cannot be offered as files — and a file copied on the client likewise reaches
the desktop as the client's own path text. Paths cross in both cases even though contents
do not.

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `--bind`, `-b` | Bind address | `127.0.0.1:3389` |
| `--cert` | TLS certificate (PEM) | Auto-generated, RSA-2048 |
| `--key` | TLS private key (PEM) | Auto-generated |
| `-u`, `--username` | RDP username | _(none)_ |
| `-p`, `--password` | RDP password | _(none)_ |
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
| `--file-transfer-mode` | Clipboard file transfer policy: `off`, `to-client`, `to-server`, or `both`. See "File transfer" | `both` |
| `--file-transfer-max-entries` | Maximum files and directories in one clipboard selection, in either direction, at most `100000` | `10000` |
| `--file-transfer-max-chunk-bytes` | Maximum bytes the client may ask for in one file-content range request | `8388608` |
| `--config` | Config file path | `~/.config/hypr-rdp/config.toml` |

## License

MIT
