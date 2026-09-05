{
  lib,
  rustPlatform,
  pkg-config,
  cmake,
  clang,
  makeWrapper,
  ffmpeg,
  libdrm,
  libgbm,
  libva,
  libxkbcommon,
  mesa,
  pipewire,
  pulseaudio,
  wayland,
}:

let
  cargoToml = builtins.fromTOML (builtins.readFile ../../Cargo.toml);
in
rustPlatform.buildRustPackage {
  pname = "hypr-rdp";
  version = cargoToml.package.version;

  src = lib.cleanSource ../..;

  cargoHash = "sha256-2TDPG/FvF9eaAxpdzocVnBZ6KMwKIkHmqq8s50xubCo=";

  nativeBuildInputs = [
    pkg-config
    cmake
    clang
    makeWrapper
    rustPlatform.bindgenHook
  ];

  buildInputs = [
    ffmpeg
    libdrm
    libgbm
    libva
    libxkbcommon
    mesa
    pipewire
    wayland
  ];

  # `fuse3` is deliberately absent from buildInputs and from the wrapper's PATH.
  # Mounting the client's clipboard files goes through `fusermount3`, which must
  # be the setuid wrapper NixOS builds in /run/wrappers/bin; the store's own
  # fusermount3 is not setuid and would shadow it. The FUSE binding is pure Rust,
  # so nothing links against libfuse and nothing probes for it at build time.
  #
  # FUSERMOUNT_PATH names that one wrapper rather than putting all of
  # /run/wrappers/bin on PATH, which every child the server spawns — pactl, the
  # session hooks — would inherit. --set-default leaves an operator who exports
  # their own value in charge.
  #
  # The wrapper exists only when `programs.fuse.enable` is on, which is the
  # default on NixOS 25.11 but not on nixos-unstable. Without it hypr-rdp warns
  # and serves the desktop-to-client direction alone.
  postInstall = ''
    wrapProgram $out/bin/hypr-rdp \
      --prefix PATH : ${lib.makeBinPath [ pulseaudio ]} \
      --set-default FUSERMOUNT_PATH /run/wrappers/bin/fusermount3
  '';

  doCheck = false;

  meta = {
    description = cargoToml.package.description;
    homepage = "https://github.com/MuNeNICK/hypr-rdp";
    license = lib.licenses.mit;
    mainProgram = "hypr-rdp";
    platforms = lib.platforms.linux;
  };
}
