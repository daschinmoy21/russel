# Minimal kernel for russel microVMs.
#
# The standard nixpkgs kernel compiles virtio drivers as modules (=m),
# but our minimal initramfs has no module-loading infrastructure.
# This expression overrides the kernel config to build the critical
# virtio/networking/filesystem drivers directly into the kernel (=y).
#
# Build with:  nix-build nix/microvm-kernel.nix
# Result:      ./result/bzImage

let
  pkgs = import <nixpkgs> {};
  lib  = pkgs.lib;
in
(pkgs.linuxPackages_latest.kernel.override {
  structuredExtraConfig = with lib.kernel; {
    # ── Virtio transport (must be built-in for PCI device discovery) ───
    VIRTIO            = yes;
    VIRTIO_PCI        = yes;
    VIRTIO_PCI_LIB    = yes;
    VIRTIO_MENU       = yes;
    VIRTIO_BALLOON    = yes;
    VIRTIO_MMIO       = yes;

    # ── Virtio devices ────────────────────────────────────────────────
    VIRTIO_NET        = yes;
    VIRTIO_BLK        = yes;
    VIRTIO_CONSOLE    = yes;

    # ── Filesystem: virtiofs (FUSE-based, used to mount host /nix/store)
    FUSE_FS           = yes;
    VIRTIO_FS         = yes;
    DAX               = yes;
    FS_DAX            = yes;

    # ── Networking basics ─────────────────────────────────────────────
    NET               = yes;
    INET              = yes;
    IPV6              = yes;

    # ── Serial console (cloud-hypervisor uses ttyS0) ──────────────────
    SERIAL_8250         = yes;
    SERIAL_8250_CONSOLE = yes;

    # ── initramfs support ─────────────────────────────────────────────
    BLK_DEV_INITRD    = yes;

    # ── Disable unnecessary subsystems for faster build + smaller image
    SOUND             = no;
    DRM               = no;
    USB_SUPPORT       = lib.mkForce no;
    WLAN              = lib.mkForce no;
    BLUETOOTH         = lib.mkForce no;
    INPUT_JOYSTICK    = no;
    INPUT_TABLET      = no;
    INPUT_TOUCHSCREEN = no;
    WIRELESS          = lib.mkForce no;
    NFC               = lib.mkForce no;
    MEDIA_SUPPORT     = lib.mkForce no;
    STAGING           = lib.mkForce no;
  };
}).dev
