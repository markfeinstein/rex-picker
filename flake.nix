{
  description = "Fast, reusable terminal picker for Rex workflows";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        rec {
          rex-picker = pkgs.rustPlatform.buildRustPackage {
            pname = "rex-picker";
            version = "0.1.0";
            src = self;

            cargoLock.lockFile = ./Cargo.lock;

            nativeCheckInputs = [ pkgs.clippy ];
            checkPhase = ''
              runHook preCheck
              cargo test --all-targets
              cargo clippy --all-targets -- -D warnings
              runHook postCheck
            '';

            meta = {
              description = "Fast, reusable terminal picker for Rex workflows";
              homepage = "https://github.com/markfeinstein/rex-picker";
              license = pkgs.lib.licenses.mit;
              mainProgram = "rex-picker";
              platforms = pkgs.lib.platforms.unix;
            };
          };

          default = rex-picker;
        }
      );

      checks = forAllSystems (system: {
        inherit (self.packages.${system}) rex-picker;
      });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.rex-picker}/bin/rex-picker";
          meta.description = "Run rex-picker";
        };
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              rustc
              rustfmt
            ];
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
