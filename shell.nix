{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
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

  RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
  shellHook = ''
    echo "🚀 Rust dev shell ready for music organizer!"
  '';
}
