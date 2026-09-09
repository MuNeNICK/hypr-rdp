{
  lib,
  rustPlatform,
  pkg-config,
  cmake,
  clang,
  makeWrapper,
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

  cargoHash = "sha256-CnpUVqFJoNuqQC8qBSbZtmzWD220BAmng5hAyowseMo=";

  nativeBuildInputs = [
    pkg-config
    cmake
    clang
    makeWrapper
    rustPlatform.bindgenHook
  ];

  buildInputs = [
    libdrm
    libgbm
    libva
    libxkbcommon
    mesa
    pipewire
    wayland
  ];

  postInstall = ''
    install -Dm644 LICENSE $out/share/licenses/hypr-rdp/LICENSE
    wrapProgram $out/bin/hypr-rdp \
      --prefix PATH : ${lib.makeBinPath [ pulseaudio ]}
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
