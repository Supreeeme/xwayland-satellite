{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.05";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    let systems = [ "x86_64-linux" "aarch64-linux" ];
    in flake-utils.lib.eachSystem systems (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        lib = pkgs.lib;

        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        cargoPackageVersion = cargoToml.package.version;

        commitHash = self.shortRev or self.dirtyShortRev or "unknown";

        version = "${cargoPackageVersion}-unstable-${commitHash}";

        buildXwaylandSatellite = { withSystemd ? true, ... } @ args:
          pkgs.rustPlatform.buildRustPackage (rec {
            pname = "xwayland-satellite";
            inherit version;

            src = self;

            cargoLock = {
              lockFile = "${src}/Cargo.lock";
              allowBuiltinFetchGit = true;
            };

            nativeBuildInputs = with pkgs; [
              rustPlatform.bindgenHook
              pkg-config
              makeWrapper
            ];

            buildInputs = with pkgs; [
              xorg.libxcb
              xorg.xcbutilcursor
            ];
           buildNoDefaultFeatures = true;
            buildFeatures = lib.optionals withSystemd [ "systemd" ];

            postInstall = ''
              ${lib.optionalString withSystemd ''
                install -Dm0644 resources/xwayland-satellite.service -t $out/lib/systemd/user
                substituteInPlace $out/lib/systemd/user/xwayland-satellite.service \
                  --replace-fail '/usr/local/bin/xwayland-satellite' "$out/bin/xwayland-satellite"
              ''}
              wrapProgram $out/bin/xwayland-satellite \
                --prefix PATH : "${lib.makeBinPath [ pkgs.xwayland ]}"
            '';

            doCheck = false;

            meta = with lib; {
              description = "Xwayland outside your Wayland";
              homepage = "https://github.com/Supreeeme/xwayland-satellite";
              license = licenses.mpl20;
              platforms = platforms.linux;
            };
          });

        xwayland-satellite = pkgs.callPackage buildXwaylandSatellite { };
      in
      {
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

        packages = {
          xwayland-satellite = xwayland-satellite;
          default = xwayland-satellite;
        };
      });
}
