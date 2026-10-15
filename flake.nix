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
      nixosModules.default = { pkgs, lib, ... }: {
        imports = [ ./nix/module.nix ];
        services.prek.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      };
      nixosModules.prek = self.nixosModules.default;

      packages = forSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          cage-touch-test = import ./tests/cage-touch.nix { inherit pkgs; };
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
                ./Cargo.toml ./Cargo.lock ./src ./examples ./docs ./udev ./nix ./README.md ./LICENSE
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
              mkdir -p "$out/share/prek/lua"
              cp ${./docs/lua/prek.lua} "$out/share/prek/lua/prek.lua"
              mkdir -p "$out/share/prek/examples"
              cp -r ${./examples/lua} "$out/share/prek/examples/lua"
              mkdir -p "$out/share/prek/nixos" "$out/share/prek/presets"
              cp ${./nix/module.nix} "$out/share/prek/nixos/module.nix"
              cp ${./docs/nixos.md} "$out/share/doc/prek/nixos.md"
              cp ${./docs/morf.md} "$out/share/doc/prek/morf.md"
              for preset in minimal desktop phone; do
                "$out/bin/prek" init --preset "$preset" \
                  --output "$out/share/prek/presets/$preset.lua"
              done
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
        cage-touch-test = {
          type = "app";
          program = "${self.packages.${system}.cage-touch-test}/bin/prek-cage-touch-test";
        };
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/prek";
        };
        cage-test = {
          type = "app";
          program = "${self.packages.${system}.cage-test}/bin/prek-cage-host-test";
        };
      });

      checks = forSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          package = self.packages.${system}.default;
        in {
          wayland-proxy = pkgs.runCommand "prek-wayland-proxy-check" {
            nativeBuildInputs = [ pkgs.python3 ];
          } ''
            mkdir proxy
            cp ${./tests/support/wlproxy.py} proxy/wlproxy.py
            cp ${./tests/support/test_wlproxy.py} proxy/test_wlproxy.py
            python3 -m unittest discover -s proxy -p test_wlproxy.py -v > "$out" 2>&1
          '';
          nixos-module = import ./tests/nixos-module.nix {
            inherit pkgs package;
            module = self.nixosModules.default;
          };
          lua-editor = pkgs.runCommand "prek-lua-editor-check" {
            nativeBuildInputs = [ pkgs.python3 pkgs.lua-language-server ];
          } ''
            python3 ${./tests/lua-editor.py} \
              --binary ${package}/bin/prek \
              --definitions ${package}/share/prek/lua/prek.lua \
              --examples ${package}/share/prek/examples/lua \
              --language-server ${pkgs.lua-language-server}/bin/lua-language-server \
              --workdir "$TMPDIR/editor" > "$out"
          '';
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
            # Prek needs no pkg-config libraries; keep host paths out, as in Mold.
            PKG_CONFIG_LIBDIR = "";
            PKG_CONFIG_PATH = "";
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
