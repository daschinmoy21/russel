# Eval-only checks for nix/modules/russel-host.nix.
# Imported from flake checks. Not a NixOS module.
{
  lib,
  nixosSystem,
  module,
  system,
}:
let
  evalHost =
    extraModules:
    nixosSystem {
      modules = [
        {
          nixpkgs.hostPlatform = system;
          boot.isContainer = true;
          system.stateVersion = "24.11";
          services.russel.enable = true;
          services.russel.bin = "/usr/local/bin/russel-ctrl";
          services.russel.environmentFile = "/etc/russel/env";
        }
        module
      ]
      ++ extraModules;
    };

  failedAssertions = eval: lib.filter (a: !a.assertion) eval.config.assertions;

  isNo = v: v == false || v == "false" || v == "no";
  isYes = v: v == true || v == "true" || v == "yes";

  default = evalHost [ ];
  defaultSc = default.config.systemd.services.russel.serviceConfig;

  existing = evalHost [
    {
      services.russel.user = "alice";
      services.russel.createUser = false;
      users.users.alice.isNormalUser = true;
    }
  ];
  existingSc = existing.config.systemd.services.russel.serviceConfig;

  existingGroup = evalHost [
    {
      services.russel.user = "alice";
      services.russel.group = "staff";
      services.russel.createUser = false;
      users.groups.staff = { };
      users.users.alice = {
        isNormalUser = true;
        group = "staff";
      };
    }
  ];

  noPodman = evalHost [
    { services.russel.rootlessPodman = false; }
  ];

  badBind = evalHost [
    { services.russel.bindAddress = "0.0.0.0:7878"; }
  ];

  extraWarm = evalHost [
    { services.russel.extraEnvironment.RUSSEL_WARM_POOL = "1"; }
  ];

  microvms = evalHost [
    {
      services.russel.microvms.enable = true;
      services.russel.microvms.kernel = "/srv/kernel/bzImage";
    }
  ];
  microvmsSvc = microvms.config.systemd.services.russel;

  microvmsLiteral = evalHost [
    {
      services.russel.microvms.enable = true;
      services.russel.microvms.kernel = ./russel-host-eval.nix;
    }
  ];
  literalKernel = microvmsLiteral.config.systemd.services.russel.environment.RUSSEL_KERNEL_PATH;
in
assert failedAssertions default == [ ];
assert defaultSc.User == "russel";
assert defaultSc.Group == "russel";
assert isNo defaultSc.NoNewPrivileges;
# Rootless Podman's pause process outlives the unit; a private /tmp or a
# read-only root pinned into it breaks every later podman call (#524).
assert !(defaultSc ? PrivateTmp) && !(defaultSc ? ProtectSystem) && !(defaultSc ? ProtectHome);
assert lib.hasInfix "/run/wrappers/bin" default.config.systemd.services.russel.environment.PATH;
assert default.config.users.users.russel.isSystemUser;
assert default.config.users.users.russel.linger;
assert default.config.users.groups ? russel;
assert default.config.virtualisation.podman.enable;
# Found by the #420 NixOS run: helper builds need <nixpkgs>, microVM stop needs
# pkill, and passt must start after the uplink has an address.
assert lib.hasPrefix "nixpkgs=/nix/store/" default.config.systemd.services.russel.environment.NIX_PATH;
assert lib.hasInfix "procps" default.config.systemd.services.russel.environment.PATH;
assert lib.elem "network-online.target" default.config.systemd.services.russel.wants;
assert lib.elem "network-online.target" default.config.systemd.services.russel.after;

assert failedAssertions existing == [ ];
assert !(existingSc ? Group);
assert !(existing.config.users.groups ? russel);
assert existingSc.User == "alice";
assert existing.config.users.users.alice.linger;
assert isNo existingSc.NoNewPrivileges;

assert failedAssertions existingGroup == [ ];
assert existingGroup.config.systemd.services.russel.serviceConfig.Group == "staff";

assert failedAssertions noPodman == [ ];
assert isYes noPodman.config.systemd.services.russel.serviceConfig.NoNewPrivileges;
assert !noPodman.config.virtualisation.podman.enable;

assert lib.any (a: lib.hasInfix "loopback" a.message) (failedAssertions badBind);

assert extraWarm.config.systemd.services.russel.environment.RUSSEL_WARM_POOL == "0";

assert !(defaultSc ? SupplementaryGroups);
assert !(default.config.systemd.services.russel.environment ? RUSSEL_KERNEL_PATH);
assert failedAssertions microvms == [ ];
assert microvmsSvc.serviceConfig.SupplementaryGroups == [ "kvm" ];
assert microvmsSvc.environment.RUSSEL_KERNEL_PATH == "/srv/kernel/bzImage";
# A path literal is copied into the store and kept in the closure (#420).
assert lib.hasPrefix "/nix/store/" literalKernel;
assert builtins.hasContext literalKernel;
assert lib.hasInfix "passt" microvmsSvc.environment.PATH;
assert lib.hasInfix "cloud-hypervisor" microvmsSvc.environment.PATH;
# ctrl stays unprivileged: no capabilities, no root.
assert !(microvmsSvc.serviceConfig ? AmbientCapabilities);
assert microvmsSvc.serviceConfig.User == "russel";

{
  ok = true;
}
