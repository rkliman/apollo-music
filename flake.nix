{
  description = "Apollo music library CLI tool";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "apollo";
          version = "0.1.0";
          src = ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
          };
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.sqlite ];
          meta = with pkgs.lib; {
            description = "Apollo music library CLI tool";
            license = licenses.mit;
            maintainers = with maintainers; [ ];
          };
        };
        # Optional: devShell for development
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            pkg-config
            libclang           # Needed by some crates using FFI (e.g., if lofty uses taglib under the hood)
            openssl            # Just in case some dependency pulls it in
            cargo-watch        # Optional: for live-reloading builds
            rust-analyzer      # Optional: for editor support
            sqlite            # If using SQLite for metadata storage
            ffmpeg            # If processing audio files
            exiftool          # If handling metadata in media files
            git               # For version control
            jq                # For JSON processing in scripts
          ];
        };
      });
}