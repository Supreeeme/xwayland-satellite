{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.05";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    naersk.url = "github:nix-community/naersk";
  };

  outputs = { self, nixpkgs, rust-overlay, naersk, flake-utils }:
    let systems = [ "x86_64-linux" "aarch64-linux" ];
    in flake-utils.lib.eachSystem systems (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        naersk' = pkgs.callPackage naersk {
          cargo = pkgs.rust-bin.stable.latest.default;
          rustc = pkgs.rust-bin.stable.latest.default;
        };
      in {
        devShell = (pkgs.mkShell.override { stdenv = pkgs.clangStdenv; }) {
          buildInputs = with pkgs; [
            rustPlatform.bindgenHook
            rust-bin.stable.latest.default
            pkg-config

            xcb-util-cursor
            xorg.libxcb
            xwayland
          ];
        };

        packages = rec {
          xwayland-satellite = naersk'.buildPackage {
            src = ./.;

            nativeBuildInputs = with pkgs; [
              rustPlatform.bindgenHook
              rust-bin.stable.latest.default
              pkg-config

              xcb-util-cursor
              xorg.libxcb
            ];
            buildInputs = [ pkgs.xwayland ];
          };

          default = xwayland-satellite;
        };
      });
}
