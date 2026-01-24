{
  description = "Xwayland outside your Wayland";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

    # NOTE: This is not necessary for end users
    # You can omit it with `inputs.rust-overlay.follows = ""`
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      cargoPackageVersion = cargoToml.package.version;
      commitHash = self.shortRev or self.dirtyShortRev or "unknown";

      xwayland-satellite-package =
        {
          lib,
          rustPlatform,
          pkg-config,
          makeBinaryWrapper,
          libxcb,
          xcb-util-cursor,
          xwayland,
          withSystemd ? true,
        }:
        rustPlatform.buildRustPackage (finalAttrs: {
          pname = "xwayland-satellite";
          version = "${cargoPackageVersion}-${commitHash}";

          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./OpenSans-Regular.ttf
              ./build.rs
              ./macros
              ./testwl
              ./wl_drm
              ./resources
              ./src
              ./Cargo.toml
              ./Cargo.lock
            ];
          };

          cargoLock = {
            lockFile = ./Cargo.lock;
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
          buildFeatures = lib.optional withSystemd "systemd";

          doCheck = false;

          env.VERGEN_GIT_DESCRIBE = finalAttrs.version;

          postInstall = ''
            wrapProgram $out/bin/xwayland-satellite \
              --prefix PATH : "${lib.makeBinPath [ xwayland ]}"
          ''
          + lib.optionalString withSystemd ''
            install -Dm0644 resources/xwayland-satellite.service -t $out/lib/systemd/user
          '';

          postFixup = lib.optionalString withSystemd ''
            substituteInPlace $out/lib/systemd/user/xwayland-satellite.service \
              --replace-fail /usr/local/bin $out/bin
          '';

          meta = with lib; {
            description = "Xwayland outside your Wayland";
            homepage = "https://github.com/Supreeeme/xwayland-satellite";
            license = licenses.mpl20;
            mainProgram = "xwayland-satellite";
            platforms = platforms.linux;
          };
        });

      inherit (nixpkgs) lib;

      # Support all Linux systems that the nixpkgs flake exposes
      systems = lib.intersectLists lib.systems.flakeExposed lib.platforms.linux;

      forAllSystems = lib.genAttrs systems;
      nixpkgsFor = forAllSystems (system: nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgsFor.${system};
          rust-bin = rust-overlay.lib.mkRustBin { } pkgs;
          inherit (self.packages.${system}) xwayland-satellite;
        in
        {
          default = pkgs.mkShell {
            packages = [
              (rust-bin.stable.latest.default.override {
                extensions = [
                  "rust-analyzer"
                  "rust-src"
                ];
              })
            ];

            nativeBuildInputs = [
              pkgs.rustPlatform.bindgenHook
              pkgs.pkg-config
              pkgs.makeBinaryWrapper
            ];
            buildInputs = xwayland-satellite.buildInputs ++ [ pkgs.xwayland ];
          };
        }
      );

      formatter = forAllSystems (system: nixpkgsFor.${system}.nixfmt);

      packages = forAllSystems (
        system:
        let
          xwayland-satellite = nixpkgsFor.${system}.callPackage xwayland-satellite-package { };
        in
        {
          inherit xwayland-satellite;
          default = xwayland-satellite;
        }
      );

      overlays.default = final: _: {
        xwayland-satellite = final.callPackage xwayland-satellite-package { };
      };
    };
}
