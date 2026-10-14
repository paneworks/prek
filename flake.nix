{
  description = "Prek: low-level Linux input tools";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs?rev=4c1018dae018162ec878d42fec712642d214fdfa";
  inputs.rust-overlay.url = "github:oxalica/rust-overlay/dbc715a4b7c0ace63b9769a032d1dd34cd89e5bd";
  inputs.rust-overlay.inputs.nixpkgs.follows = "nixpkgs";

  outputs = { self, nixpkgs, rust-overlay }:
    let
      forSystems = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ];
    in
    {
      packages = forSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          cage-test = import ./tests/cage-host.nix {
            inherit pkgs;
            package = self.packages.${system}.default;
          };
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "prek";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml ./Cargo.lock ./src ./examples ./docs ./udev ./README.md ./LICENSE
                # Desktop test controllers and the C receiver are separate
                # derivations; changing them must not recompile Rust packages.
                (pkgs.lib.fileset.fileFilter (file: file.hasExt "rs") ./tests)
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;
            cargoLock.outputHashes = {
              "luna-0.5.1" = "sha256-gCdX/Ni2RbK69KMJFVzK72QSuDFVVQM2deiKXH3AvoI=";
            };
            postInstall = ''
              mkdir -p "$out/lib/udev/rules.d"
              substitute ${./udev/70-prek.rules} "$out/lib/udev/rules.d/70-prek.rules" \
                --replace-fail '@prek@' "$out/bin/prek"
              mkdir -p "$out/share/doc/prek"
              cp ${./docs/lua-api.md} "$out/share/doc/prek/lua-api.md"
              cp ${./docs/lua-api.json} "$out/share/doc/prek/lua-api.json"
              mkdir -p "$out/share/prek/examples"
              cp -r ${./examples/lua} "$out/share/prek/examples/lua"
            '';
            meta = {
              description = "Low-level Linux input tools and gesture recognition";
              license = pkgs.lib.licenses.mit;
              mainProgram = "prek";
              platforms = pkgs.lib.platforms.linux;
            };
          };
        });

      apps = forSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/prek";
        };
        cage-test = {
          type = "app";
          program = "${self.packages.${system}.cage-test}/bin/prek-cage-host-test";
        };
      });

      devShells = forSystems (system:
        let
          pkgs = import nixpkgs { inherit system; overlays = [ rust-overlay.overlays.default ]; };
          crossTriple = "aarch64-unknown-linux-musl";
          crossPkgs = import nixpkgs { inherit system; crossSystem.config = crossTriple; };
          crossCc = "${crossPkgs.stdenv.cc}/bin/${crossPkgs.stdenv.cc.targetPrefix}cc";
        in {
          cross-aarch64 = pkgs.mkShell {
            packages = [
              (pkgs.rust-bin.stable.latest.default.override { targets = [ crossTriple ]; })
              crossPkgs.stdenv.cc
            ];
            CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER = crossCc;
            CC_aarch64_unknown_linux_musl = crossCc;
            # Use the phone's loader so runtime systemd loading remains possible.
            CARGO_BUILD_RUSTFLAGS = "-C target-feature=-crt-static -C link-arg=-Wl,--dynamic-linker=/lib/ld-musl-aarch64.so.1";
            LD_LIBRARY_PATH = "";
          };
          default = pkgs.mkShell {
            packages = with pkgs; [
              rustc cargo rustfmt clippy rust-analyzer git-cliff
            ];
          };
        });
    };
}
