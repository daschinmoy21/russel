# Corresponding source for the microVM kernel we ship as a release asset.
#
# The bzImage is GPL-2.0. Every release that ships it also ships this tar
# (contrib/release-kernel.sh uploads both), so the source is offered from the
# same place as the binary (GPL-2.0 section 3). It holds:
#
#   linux-<version>.tar.xz   the upstream tarball the build unpacks (kernel.src)
#   patches/NN-<name>.patch  nixpkgs kernelPatches, in the order they apply
#   config                   the generated .config the bzImage was built with
#   build/russel/            flake.nix, flake.lock, nix/microvm-kernel.nix
#   build/nixpkgs-kernel/    nixpkgs pkgs/os-specific/linux/kernel at the
#                            pinned rev (config generation and build scripts)
#   README                   versions, nixpkgs rev, and how to rebuild
#
# Build: nix build .#packages.x86_64-linux.microvm-kernel-source
# Result: a single uncompressed tar (the kernel tarball inside is already xz).

{
  pkgs,
  lib ? pkgs.lib,
  kernel,
  nixpkgsRev,
}:

let
  patches = lib.filter (p: (p.patch or null) != null) kernel.kernelPatches;
  patchName = i: p: "${lib.fixedWidthNumber 2 i}-${p.name}.patch";
  dir = "russel-kernel-source-${kernel.version}";

  readme = pkgs.writeText "README" ''
    Russel microVM kernel: corresponding source
    ===========================================

    This archive is the complete corresponding source for the Linux kernel
    image shipped with Russel releases (russel-kernel-<tag>-x86_64.bzImage).
    The kernel is licensed under the GNU General Public License v2.0; see
    COPYING inside ${kernel.src.name}. Russel itself is Apache-2.0.

    Kernel version: ${kernel.version}
    Upstream tarball: ${kernel.src.name} (${toString (kernel.src.urls or [ kernel.src.url ])})
    nixpkgs: github:NixOS/nixpkgs/${nixpkgsRev}
    Nix attribute: packages.x86_64-linux.microvm-kernel

    Contents
    --------
    ${kernel.src.name}  unmodified upstream source
    patches/            patches applied on top, in file-name order:
    ${lib.concatStringsSep "\n" (lib.imap1 (i: p: "                      ${patchName i p}") patches)}
    config              the kernel .config used for the build
    build/russel/       Russel's flake.nix, flake.lock and nix/microvm-kernel.nix
                        (the config override on top of nixpkgs linuxPackages_latest)
    build/nixpkgs-kernel/
                        nixpkgs pkgs/os-specific/linux/kernel at the rev above:
                        the scripts that generate the config and drive the build

    Rebuilding
    ----------
    With Nix, from the Russel release tag:

      nix build github:daschinmoy21/russel/<tag>#packages.x86_64-linux.microvm-kernel

    By hand (equivalent configuration; nixpkgs also rewrites a few script
    shebangs and paths before building, see build/nixpkgs-kernel):

      tar -xf ${kernel.src.name}
      cd linux-${kernel.version}
      for p in ../patches/*.patch; do patch -p1 <"$p"; done
      cp ../config .config
      make olddefconfig
      make bzImage
  '';
in
pkgs.runCommand "russel-kernel-source-${kernel.version}.tar" { } ''
  d=${dir}
  mkdir -p "$d/patches" "$d/build/russel/nix" "$d/build/nixpkgs-kernel"
  cp ${kernel.src} "$d/${kernel.src.name}"
  cp ${kernel.configfile} "$d/config"
  ${lib.concatStrings (
    lib.imap1 (i: p: ''
      cp ${p.patch} "$d/patches/${patchName i p}"
    '') patches
  )}
  cp ${../flake.nix} "$d/build/russel/flake.nix"
  cp ${../flake.lock} "$d/build/russel/flake.lock"
  cp ${./microvm-kernel.nix} "$d/build/russel/nix/microvm-kernel.nix"
  cp -r ${pkgs.path}/pkgs/os-specific/linux/kernel/. "$d/build/nixpkgs-kernel/"
  cp ${readme} "$d/README"
  chmod -R u+w,go-w "$d"
  tar --sort=name --mtime=@1 --owner=0 --group=0 --numeric-owner \
    -cf "$out" "$d"
''
