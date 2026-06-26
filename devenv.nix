{ pkgs, ... }:

{
  packages = [
    pkgs.cargo
    pkgs.clippy
    pkgs.git
    pkgs.qemu
    pkgs.rustc
    pkgs.rustfmt
  ];
}
