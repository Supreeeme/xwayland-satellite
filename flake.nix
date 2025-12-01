{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
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

        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        cargoPackageVersion = cargoToml.package.version;

        commitHash = self.shortRev or self.dirtyShortRev or "unknown";

        version = "${cargoPackageVersion}-${commitHash}";

        buildXwaylandSatellite =
          { lib
          , rustPlatform
          , pkg-config
          , makeBinaryWrapper
          , libxcb
          , xcb-util-cursor
          , xwayland
          , withSystemd ? true
          }:

          rustPlatform.buildRustPackage rec {
            pname = "xwayland-satellite";
            inherit version;

            src = self;

            cargoLock = {
              lockFile = "${src}/Cargo.lock";
              allowBuiltinFetchGit = true;
            };

            nativeBuildInputs = [
              rustPlatform.bindgenHook
              pkg-config
              makeBinaryWrapper
            ];

            buildInputs = [
              libxcb
              xcb-util-cursor
            ];

            buildNoDefaultFeatures = true;
            buildFeatures = lib.optionals withSystemd [ "systemd" ];

            postPatch = ''
              substituteInPlace resources/xwayland-satellite.service \
                --replace-fail '/usr/local/bin' "$out/bin"
            '';

            postInstall = lib.optionalString withSystemd ''
              install -Dm0644 resources/xwayland-satellite.service -t $out/lib/systemd/user
            '';

            postFixup = ''
              wrapProgram $out/bin/xwayland-satellite \
                --prefix PATH : "${lib.makeBinPath [ xwayland ]}"
            '';

            doCheck = false;

            meta = with lib; {
              description = "Xwayland outside your Wayland";
              homepage = "https://github.com/Supreeeme/xwayland-satellite";
              license = licenses.mpl20;
              mainProgram = "xwayland-satellite";
              platforms = platforms.linux;
            };
          };

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
