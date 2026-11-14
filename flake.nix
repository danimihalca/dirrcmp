{
  description = "dirrcmp - terminal UI for comparing two directories (Rust)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      manifest = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package;

      mkDirrcmp = pkgs: { features ? [ ] }:
        pkgs.rustPlatform.buildRustPackage {
          pname = manifest.name;
          version = manifest.version;
          # Only the Rust sources: editing the flake or README does not rebuild.
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
            ];
          };
          # Commit Cargo.lock (run `cargo generate-lockfile` once, then `git add Cargo.lock`).
          cargoLock.lockFile = ./Cargo.lock;
          buildFeatures = features;
          # The tests create temp dirs and files only; they need no network or terminal.
          # git tests build throwaway repositories
          nativeCheckInputs = [ pkgs.git ];
          doCheck = true;
          meta = {
            description = manifest.description;
            license = pkgs.lib.licenses.mit;
            mainProgram = "dirrcmp";
            platforms = pkgs.lib.platforms.unix;
          };
        };
    in
    {
      packages = forAll (pkgs: rec {
        dirrcmp = mkDirrcmp pkgs { };
        # More languages for syntax highlighting (pulls in the syntect crate).
        dirrcmp-syntect = mkDirrcmp pkgs { features = [ "syntect" ]; };
        default = dirrcmp;
      });

      apps = forAll (pkgs: rec {
        dirrcmp = {
          type = "app";
          program = "${self.packages.${pkgs.system}.dirrcmp}/bin/dirrcmp";
        };
        default = dirrcmp;
      });

      overlays.default = final: _prev: {
        dirrcmp = mkDirrcmp final { };
      };

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rustfmt clippy rust-analyzer ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      formatter = forAll (pkgs: pkgs.nixfmt-rfc-style);
    };
}
