# Minimal kernel for russel microVMs.
#
# The standard nixpkgs kernel compiles virtio drivers as modules (=m),
# but our minimal initramfs has no module-loading infrastructure.
# This expression overrides the kernel config to build the critical
# virtio/networking/filesystem drivers directly into the kernel (=y).
#
# Build: nix build .#microvm-kernel
# Result: ./result/bzImage  (and $out/bzImage in store)
# Legacy: nix-build -E 'with import <nixpkgs> {}; callPackage ./nix/microvm-kernel.nix {}'
#
# ignoreConfigErrors: nixpkgs common-config emits child options for subsystems
# we didn't explicitly configure. We intentionally don't strip subsystems to
# avoid cascading unused-option errors; size is secondary to a working
# built-in virtio kernel.

{
  pkgs,
  lib ? pkgs.lib,
}:


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

    # ── ACPI + hotplug (future CPU/memory/IO resize) ──────────────
    ACPI              = yes;
    ACPI_HOTPLUG_CPU  = yes;
    HOTPLUG_PCI       = yes;
    HOTPLUG_CPU       = yes;
    MEMORY_HOTPLUG    = yes;

    # ── initramfs support ─────────────────────────────────────────────
    BLK_DEV_INITRD    = yes;

  };
  ignoreConfigErrors = true;
})
