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
in
assert failedAssertions default == [ ];
assert defaultSc.User == "russel";
assert defaultSc.Group == "russel";
assert isNo defaultSc.NoNewPrivileges;
assert lib.any (p: p == "/run/user") (lib.toList defaultSc.ReadWritePaths);
assert lib.hasInfix "/run/wrappers/bin" default.config.systemd.services.russel.environment.PATH;
assert default.config.users.users.russel.isSystemUser;
assert default.config.users.users.russel.linger;
assert default.config.users.groups ? russel;
assert default.config.virtualisation.podman.enable;

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

{
  ok = true;
}
