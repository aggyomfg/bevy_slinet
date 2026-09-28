# Based on https://github.com/bevyengine/bevy/blob/main/docs/linux_dependencies.md#flakenix
{
  description = "bevy_slinet dev shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
      in
      {
        devShells.default =
          with pkgs;
          mkShell rec {
            nativeBuildInputs = [
              (rust-bin.stable.latest.default.override {
                extensions = [
                  "rust-src"
                  "rust-analyzer"
                ];
              })
              pkg-config
              cargo-audit
            ];

            # Not needed by the current minimal bevy features, but required once rendering/audio/input features are enabled.
            buildInputs = lib.optionals stdenv.isLinux [
              udev
              alsa-lib-with-plugins
              vulkan-loader
              vulkan-tools
              libx11
              libxcursor
              libxi
              libxrandr
              libxkbcommon
              wayland
            ];

            LD_LIBRARY_PATH = lib.makeLibraryPath buildInputs;
          };
      }
    );
}
