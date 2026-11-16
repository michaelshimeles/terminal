{
  lib,
  rustPlatform,
  pkg-config,
  cmake,
  makeWrapper,
  stdenv,
  libxkbcommon,
  wayland,
  vulkan-loader,
  libGL,
  libx11,
  libxcursor,
  libxi,
  libxcb,
}:

let
  version = (lib.importTOML ./Cargo.toml).workspace.package.version;
in
rustPlatform.buildRustPackage {
  pname = "terminal";
  inherit version;

  src = lib.cleanSource ./.;
  cargoLock.lockFile = ./Cargo.lock;
  # The binary crate lives in app/ inside a virtual workspace manifest.
  buildAndTestSubdir = "app";

  nativeBuildInputs = [
    pkg-config
    cmake # aws-lc-sys builds its C sources with cmake
    makeWrapper
  ];

  buildInputs = lib.optionals stdenv.hostPlatform.isLinux [
    libxkbcommon
    wayland
    vulkan-loader
    libGL
    libx11
    libxcursor
    libxi
    libxcb
  ];

  postInstall = ''
    mv $out/bin/terminal $out/bin/.terminal-unwrapped
    makeWrapper $out/bin/.terminal-unwrapped $out/bin/terminal \
      --run 'case "''${1:-}" in --version|-V) echo "terminal ${version}"; exit 0;; --help|-h|-?) printf "%s\n" "terminal ${version} — keyboard-driven market terminal" "" "usage: terminal [--version] [--help]" "" "Launches the native GUI (eframe/wgpu). Environment:" "  TR_PROVIDER=sample    run on offline sample data" "  TR_DATA_DIR=<dir>     override the data directory" "  TR_SMOKE_SECS=<n>     render for N seconds, log frame stats and quit" "  TR_CONGRESS_REFRESH_MIN=0   disable congress refresh"; exit 0;; esac'
  '';

  # wgpu, xkbcommon-dl and wayland-backend dlopen() these libraries at
  # runtime. They are not NEEDED entries, so extend the ELF rpath so the
  # binary finds them on non-NixOS Linux too.
  postFixup = lib.optionalString stdenv.hostPlatform.isLinux ''
    rpath=$(patchelf --print-rpath "$out/bin/.terminal-unwrapped")
    patchelf --set-rpath "$rpath:${
      lib.makeLibraryPath [
        libxkbcommon
        vulkan-loader
        wayland
        libGL
      ]
    }" "$out/bin/.terminal-unwrapped"
  '';

  # The test suite needs a display/GPU and network access; the project's own
  # CI runs it on macOS runners and under Xvfb on Linux.
  doCheck = false;

  meta = {
    description = "Bloomberg-style trading terminal with GPU charts and congressional trade tracking";
    homepage = "https://github.com/michaelshimeles/terminal";
    mainProgram = "terminal";
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
