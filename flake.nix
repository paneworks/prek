{
  description = "Prek: low-level Linux input tools";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs?rev=4c1018dae018162ec878d42fec712642d214fdfa";

  outputs = { self, nixpkgs }:
    let
      forSystems = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ];
    in
    {
      packages = forSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "prek";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml ./Cargo.lock ./src ./examples ./tests ./docs ./udev ./README.md ./LICENSE
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
      });

      checks = forSystems (system: {
        lua = import ./tests/compositor.nix {
          pkgs = nixpkgs.legacyPackages.${system};
          package = self.packages.${system}.default;
          lua = true;
        };
        compositor = import ./tests/compositor.nix {
          pkgs = nixpkgs.legacyPackages.${system};
          package = self.packages.${system}.default;
        };
        routing = import ./tests/routing.nix {
          pkgs = nixpkgs.legacyPackages.${system};
          package = self.packages.${system}.default;
        };
      });

      devShells = forSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [
              rustc cargo rustfmt clippy rust-analyzer git-cliff
            ];
          };
        });
    };
}
