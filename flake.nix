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
        let
          pkgs = nixpkgs.legacyPackages.${system};
          nativeLibraries = pkgs.lib.concatStringsSep ":" [
            "/usr/lib/${pkgs.stdenv.hostPlatform.parsed.cpu.name}-linux-gnu/libsystemd.so.0"
            "/usr/lib64/libsystemd.so.0"
            "/usr/lib/libsystemd.so.0"
            "${pkgs.systemd}/lib/libsystemd.so.0"
          ];
          nativeProbe = pkgs.writeShellScript "prek-native-root-probe" ''
            if [ "$#" -eq 1 ]; then
              case "$1" in
                --check-library) probe=session::native_probe::library_preflight ;;
                --preflight) probe=session::native_probe::session_preflight ;;
                *) echo 'Expected --check-library or --preflight.' >&2; exit 2 ;;
              esac
              exec ${pkgs.coreutils}/bin/env -i PREK_NATIVE_LIBRARIES=${nativeLibraries} \
                ${self.packages.${system}.default.nativeTests}/libexec/prek-native-tests \
                --exact "$probe" --ignored --nocapture --test-threads=1
            fi
            echo 'Expected --check-library or --preflight.' >&2
            exit 2
          '';
        in {
          native-root-probe = nativeProbe;
          root-routing-test = import ./tests/root-routing.nix {
            inherit pkgs;
            package = self.packages.${system}.default;
            managed = self.packages.${system}.native-managed-test;
          };
          native-managed-test = import ./tests/native-managed.nix {
            package = self.packages.${system}.default;
          };
          root-sleep-watch-test = import ./tests/root-sleep-watch.nix {
            inherit pkgs;
            package = self.packages.${system}.default;
          };
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "prek";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            outputs = [ "out" "nativeTests" ];
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
            nativeCheckInputs = [ pkgs.dbus ];
            PREK_TEST_SYSTEMD_LIBRARY = "${pkgs.systemd}/lib/libsystemd.so.0";
            cargoLock.outputHashes = {
              "luna-0.5.1" = "sha256-gCdX/Ni2RbK69KMJFVzK72QSuDFVVQM2deiKXH3AvoI=";
            };
            postInstall = ''
              mkdir -p "$nativeTests/libexec"
              count=0
              for candidate in target/*/release/deps/prek-* target/release/deps/prek-*; do
                if [ -f "$candidate" ] && [ -x "$candidate" ] && \
                  "$candidate" --list --ignored | ${pkgs.ripgrep}/bin/rg -Fx 'session::native_probe::library_preflight: test'; then
                  for probe in library_preflight session_preflight controller_preflight; do
                    "$candidate" --list --ignored | ${pkgs.ripgrep}/bin/rg -Fx "session::native_probe::$probe: test"
                  done
                  "$candidate" --list --ignored | ${pkgs.ripgrep}/bin/rg -Fx \
                    'kernel::tests::repeat::isolated_uinput_repeat_timer_and_consumer_settings_are_not_transparent: test'
                  install -m 755 "$candidate" "$nativeTests/libexec/prek-native-tests"
                  count=$((count + 1))
                fi
              done
              test "$count" -eq 1
              count=0
              for candidate in target/*/release/deps/routing-* target/release/deps/routing-*; do
                if [ -f "$candidate" ] && [ -x "$candidate" ] && \
                  "$candidate" --list --ignored | ${pkgs.ripgrep}/bin/rg -Fx 'evdev_boundary::pregrab_backlog_and_postgrab_input_share_the_reader_queue: test'; then
                  install -m 755 "$candidate" "$nativeTests/libexec/prek-routing-tests"
                  count=$((count + 1))
                fi
              done
              test "$count" -eq 1
              mkdir -p "$out/lib/udev/rules.d"
              count=0
              for candidate in target/*/release/deps/recovery-* target/release/deps/recovery-*; do
                if [ -f "$candidate" ] && [ -x "$candidate" ] && \
                  "$candidate" --inventory | ${pkgs.ripgrep}/bin/rg -Fx 'PREK_KERNEL_RECOVERY_INVENTORY_V1'; then
                  install -m 755 "$candidate" "$nativeTests/libexec/prek-recovery-tests"
                  count=$((count + 1))
                fi
              done
              test "$count" -eq 1
              substitute ${./udev/70-prek.rules} "$out/lib/udev/rules.d/70-prek.rules" \
                --replace-fail '@prek@' "$out/bin/prek"
              mkdir -p "$out/share/doc/prek"
              cp ${./LICENSE} "$out/share/doc/prek/LICENSE"
              ${pkgs.python3}/bin/python3 ${./scripts/vendor-notices.py} \
                --vendor "$cargoDeps" --output "$out/share/doc/prek/dependency-notices"
              cp ${./docs/distribution.md} "$out/share/doc/prek/distribution.md"
              cp ${./docs/lua-api.md} "$out/share/doc/prek/lua-api.md"
              cp ${./docs/lua-api.json} "$out/share/doc/prek/lua-api.json"
              mkdir -p "$out/share/prek/lua"
              cp ${./docs/lua/prek.lua} "$out/share/prek/lua/prek.lua"
              mkdir -p "$out/share/prek/examples"
              cp -r ${./examples/lua} "$out/share/prek/examples/lua"
              mkdir -p "$out/share/prek/nixos" "$out/share/prek/presets"
              cp ${./nix/module.nix} "$out/share/prek/nixos/module.nix"
              cp ${./docs/nixos.md} "$out/share/doc/prek/nixos.md"
              cp ${./docs/root-routing.md} "$out/share/doc/prek/root-routing.md"
              cp ${./docs/recognizers.md} "$out/share/doc/prek/recognizers.md"
              cp ${./docs/routing-chains.md} "$out/share/doc/prek/routing-chains.md"
              cp ${./docs/routing-replay.md} "$out/share/doc/prek/routing-replay.md"
              cp ${./docs/evdev-boundary.md} "$out/share/doc/prek/evdev-boundary.md"
              cp ${./docs/suspend-epoch.md} "$out/share/doc/prek/suspend-epoch.md"
              cp ${./docs/feedback.md} "$out/share/doc/prek/feedback.md"
              for preset in minimal desktop phone; do
                "$out/bin/prek" init --preset "$preset" \
                  --output "$out/share/prek/presets/$preset.lua"
              done
            '';
            meta = {
              outputsToInstall = [ "out" ];
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
        root-routing-test = {
          type = "app";
          program = "${self.packages.${system}.root-routing-test}/bin/prek-root-routing-test";
        };
      });

      checks = forSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          package = self.packages.${system}.default;
        in {
          distribution = pkgs.runCommand "prek-distribution-check" {
            nativeBuildInputs = [ pkgs.python3 pkgs.bash pkgs.shellcheck ];
          } ''
            mkdir -p source/scripts source/tests/support
            cp ${./scripts/nix-dist.py} source/scripts/nix-dist.py
            cp ${./scripts/vendor-notices.py} source/scripts/vendor-notices.py
            cp ${./tests/support/test_nix_dist.py} source/tests/support/test_nix_dist.py
            cp ${./tests/support/test_build_tools.py} source/tests/support/test_build_tools.py
            cp ${./tests/run-nix-gate.sh} source/tests/run-nix-gate.sh
            cp ${./tests/run-aarch64-build.sh} source/tests/run-aarch64-build.sh
            cp ${./tests/run-aarch64-exec.sh} source/tests/run-aarch64-exec.sh
            cd source
            python3 -m unittest discover -s tests/support -p test_nix_dist.py -v > "$out" 2>&1
            python3 -m unittest discover -s tests/support -p test_build_tools.py -v >> "$out" 2>&1
            for script in tests/run-nix-gate.sh tests/run-aarch64-build.sh tests/run-aarch64-exec.sh; do
              bash -n "$script"
              shellcheck "$script"
            done
          '';
          installed-cli = pkgs.runCommand "prek-installed-cli-check" {
            nativeBuildInputs = [ pkgs.python3 ];
          } ''
            python3 ${./scripts/nix-dist.py} smoke --package ${package} \
              --version ${package.version} > "$out"
          '';
          native-routing-helper = pkgs.runCommand "prek-native-routing-helper-check" {} ''
            PREK_TEST_LUA_BINARY=${package}/bin/prek \
              ${package.nativeTests}/libexec/prek-native-tests \
              --exact route_worker::tests::recognizers::actual::supervised_callbacks_cross_the_sole_worker_protocol_with_native_ownership \
              --ignored --test-threads=1 > "$out" 2>&1 || { cat "$out"; exit 1; }
            ${pkgs.gnugrep}/bin/grep -F 'test result: ok. 1 passed; 0 failed;' "$out" || { cat "$out"; exit 1; }
          '';
          native-composition-helper = pkgs.runCommand "prek-native-composition-helper-check" {} ''
            PREK_TEST_LUA_BINARY=${package}/bin/prek \
              ${package.nativeTests}/libexec/prek-native-tests \
              --exact rules::tests::composition::helper::supervised_retained_touch_handoffs_use_one_native_coordinator \
              --ignored --test-threads=1 > "$out" 2>&1 || { cat "$out"; exit 1; }
            ${pkgs.gnugrep}/bin/grep -F 'test result: ok. 1 passed; 0 failed;' "$out" || { cat "$out"; exit 1; }
          '';
          native-chain-helper = pkgs.runCommand "prek-native-chain-helper-check" {} ''
            PREK_TEST_LUA_BINARY=${package}/bin/prek \
              ${package.nativeTests}/libexec/prek-native-tests \
              --exact routing::tests::custom::chains::helper::supervised_custom_steps_use_complete_native_routing_scopes \
              --ignored --test-threads=1 > "$out" 2>&1 || { cat "$out"; exit 1; }
            ${pkgs.gnugrep}/bin/grep -F 'test result: ok. 1 passed; 0 failed;' "$out" || { cat "$out"; exit 1; }
          '';
          native-composed-worker-helper = pkgs.runCommand "prek-native-composed-worker-helper-check" {} ''
            PREK_TEST_LUA_BINARY=${package}/bin/prek \
              ${package.nativeTests}/libexec/prek-native-tests \
              --exact route_worker::tests::recognizers::composed::helper::supervised_composed_plans_survive_real_private_configuration_reconstruction \
              --ignored --test-threads=1 > "$out" 2>&1 || { cat "$out"; exit 1; }
            ${pkgs.gnugrep}/bin/grep -F 'test result: ok. 1 passed; 0 failed;' "$out" || { cat "$out"; exit 1; }
          '';
          native-root-probe = pkgs.runCommand "prek-native-root-probe-check" {
            nativeBuildInputs = [ pkgs.bash pkgs.shellcheck ];
          } ''
            bash -n ${self.packages.${system}.native-root-probe}
            shellcheck ${self.packages.${system}.native-root-probe}
            touch "$out"
          '';
          root-routing-receipts = pkgs.runCommand "prek-root-routing-receipts-check" {
            nativeBuildInputs = [ pkgs.python3 pkgs.bash pkgs.shellcheck pkgs.perl ];
          } ''
            mkdir -p receipts/tests/support receipts/udev
            cp ${./udev/70-prek.rules} receipts/udev/70-prek.rules
            cp ${./tests/support/root_routing.py} receipts/tests/support/root_routing.py
            cp ${./tests/support/test_root_routing.py} receipts/tests/support/test_root_routing.py
            cp ${./tests/support/inject-input.pl} receipts/tests/support/inject-input.pl
            cp ${./tests/run-root-routing.sh} receipts/tests/run-root-routing.sh
            cp ${./tests/run-takeover-trial.sh} receipts/tests/run-takeover-trial.sh
            python3 -m unittest discover -s receipts/tests/support -p test_root_routing.py -v > "$out" 2>&1
            bash -n ${./tests/run-root-routing.sh}
            shellcheck ${./tests/run-root-routing.sh}
            bash -n ${./tests/run-takeover-trial.sh}
            shellcheck ${./tests/run-takeover-trial.sh}
            bash -n ${self.packages.${system}.root-routing-test}/bin/prek-root-routing-test
            shellcheck ${self.packages.${system}.root-routing-test}/bin/prek-root-routing-test
          '';
          nixos-module = import ./tests/nixos-module.nix {
            inherit pkgs package;
            module = self.nixosModules.default;
          };
          root-sleep-watch = pkgs.runCommand "prek-root-sleep-watch-check" {
            nativeBuildInputs = [ pkgs.shellcheck ];
          } ''
            bash -n ${./tests/run-root-sleep-watch.sh}
            shellcheck ${./tests/run-root-sleep-watch.sh}
            bash -n ${self.packages.${system}.root-sleep-watch-test}/bin/prek-root-sleep-watch-test
            shellcheck ${self.packages.${system}.root-sleep-watch-test}/bin/prek-root-sleep-watch-test
            ${package.nativeTests}/libexec/prek-native-tests --list --ignored > inventory
            for probe in library_preflight root_delay_preflight; do
              grep -Fx "sleep_watch::native_probe::$probe: test" inventory >> "$out"
            done
          '';
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
          crossShell = toolchain: root: pkgs.mkShell {
            packages = [
              (toolchain.override { targets = [ crossTriple ]; })
              crossPkgs.stdenv.cc
              pkgs.binutils pkgs.git pkgs.patchelf
            ];
            PREK_CROSS_ROOT = root;
            CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER = crossCc;
            CC_aarch64_unknown_linux_musl = crossCc;
            # Select the dynamic musl target loader.
            CARGO_BUILD_RUSTFLAGS = "-C target-feature=-crt-static -C link-arg=-Wl,--dynamic-linker=/lib/ld-musl-aarch64.so.1";
            NIX_DONT_SET_RPATH = "1";
            NIX_DONT_SET_RPATH_aarch64_unknown_linux_musl = "1";
            shellHook = ''
              unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS
              export NIX_LDFLAGS= NIX_LDFLAGS_BEFORE= NIX_LDFLAGS_AFTER=
              export NIX_LDFLAGS_aarch64_unknown_linux_musl= NIX_LDFLAGS_BEFORE_aarch64_unknown_linux_musl= NIX_LDFLAGS_AFTER_aarch64_unknown_linux_musl=
            '';
            # Prek needs no pkg-config libraries; keep host paths out, as in Mold.
            PKG_CONFIG_LIBDIR = "";
            PKG_CONFIG_PATH = "";
            LD_LIBRARY_PATH = "";
          };
        in {
          msrv = pkgs.mkShell {
            packages = with pkgs; [ rust-bin.stable."1.88.0".default python3 shellcheck dbus ];
            PREK_TEST_SYSTEMD_LIBRARY = "${pkgs.systemd}/lib/libsystemd.so.0";
          };
          cross-aarch64 = crossShell pkgs.rust-bin.stable.latest.default "target/cross-aarch64";
          cross-aarch64-gnu =
            let
              gnuPkgs = import nixpkgs { inherit system; crossSystem.config = "aarch64-unknown-linux-gnu"; };
              gnuCc = "${gnuPkgs.stdenv.cc}/bin/${gnuPkgs.stdenv.cc.targetPrefix}cc";
            in
            pkgs.mkShell {
              packages = [
                (pkgs.rust-bin.stable.latest.default.override { targets = [ "aarch64-unknown-linux-gnu" ]; })
                gnuPkgs.stdenv.cc
                pkgs.patchelf
              ];
              CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER = gnuCc;
              CC_aarch64_unknown_linux_gnu = gnuCc;
            };
          cross-aarch64-msrv = crossShell pkgs.rust-bin.stable."1.88.0".default "target/cross-aarch64-msrv";
          default = pkgs.mkShell {
            PREK_TEST_SYSTEMD_LIBRARY = "${pkgs.systemd}/lib/libsystemd.so.0";
            packages = with pkgs; [
              rustc cargo rustfmt clippy rust-analyzer git-cliff python3 shellcheck actionlint dbus
            ];
          };
        });
    };
}
