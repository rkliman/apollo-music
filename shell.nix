{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    rustc
    cargo
    gcc
    pkg-config
    ffmpeg
    sqlite
  ];

  shellHook = ''
    echo "Apollo Music development environment"
    echo "Rust version: $(rustc --version)"
    echo "FFmpeg version: $(ffmpeg -version | head -n1)"
  '';
}
