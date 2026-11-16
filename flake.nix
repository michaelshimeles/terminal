{
  description = "Bloomberg-style trading terminal with GPU charts and congressional trade tracking";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    # Pin x86_64-darwin to a stable release branch for older macOS Intel
    # compatibility. nixpkgs-unstable dropped x86_64-darwin in 26.11.
    nixpkgs-darwin-legacy.url = "github:NixOS/nixpkgs/nixpkgs-26.05-darwin";
    # Enables the non-flake `nix-build` path via default.nix.
    flake-compat.url = "github:edolstra/flake-compat";
    flake-compat.flake = false;
  };

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs-darwin-legacy,
      ...
    }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor =
        system:
        if system == "x86_64-darwin" then
          import nixpkgs-darwin-legacy {
            inherit system;
            config.allowDeprecatedx86_64Darwin = true;
          }
        else
          nixpkgs.legacyPackages.${system};
    in
    {
      packages = forAllSystems (
        system:
        let
          terminal = (pkgsFor system).callPackage ./package.nix { };
        in
        {
          inherit terminal;
          default = terminal;
          source = terminal;
        }
      );

      apps = forAllSystems (system: {
        terminal = {
          type = "app";
          program = "${self.packages.${system}.terminal}/bin/terminal";
          meta = self.packages.${system}.terminal.meta;
        };
        default = self.apps.${system}.terminal;
      });

      checks = forAllSystems (system: {
        build = self.packages.${system}.default;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.rustc
              pkgs.cargo
              pkgs.clippy
              pkgs.rustfmt
              pkgs.rust-analyzer
              pkgs.pkg-config
              pkgs.cmake # aws-lc-sys
              pkgs.sqlite # inspect the app's SQLite market-data cache
              pkgs.act # run GitHub Actions workflows locally
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              pkgs.libxkbcommon
              pkgs.wayland
              pkgs.vulkan-loader
              pkgs.libGL
              pkgs.libx11
              pkgs.libxcursor
              pkgs.libxi
              pkgs.libxcb
            ];
          };
        }
      );

      overlays.default = final: _prev: {
        terminal = final.callPackage ./package.nix { };
      };
    };
}
