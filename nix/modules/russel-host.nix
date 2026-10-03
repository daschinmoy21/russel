# NixOS module for russel-ctrl on a single trusted operator host.
#
# Defaults:
#   bind 127.0.0.1:7878, RUSSEL_REQUIRE_AUTH=1, RUSSEL_API_TOKEN from a
#   root:russel 0640 EnvironmentFile, /var/lib/russel mode 0700, warm pool off,
#   rootlessPodman = true (NoNewPrivileges off, /run/wrappers on PATH,
#   virtualisation.podman.enable default on).
#
# DynamicUser is not used. Rootless Podman needs a real lingering account;
# microVMs (experimental, `microvms.enable`) need /dev/kvm and passt. For a
# container-only VPS, set `user` to your lingering login user.
#
# TLS is not implemented in ctrl. Put Caddy, nginx, or Traefik in front.
# See docs/security-tls.md and docs/traefik.md.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib)
    mkDefault
    mkEnableOption
    mkIf
    mkOption
    types
    ;
  cfg = config.services.russel;
  userExists = lib.hasAttr cfg.user config.users.users;
  resolvedGroup =
    if cfg.group != null then
      cfg.group
    else if cfg.createUser then
      cfg.user
    else
      null;
  groupExists = resolvedGroup == null || lib.hasAttr resolvedGroup config.users.groups;
  execStart =
    if cfg.bin != null then
      cfg.bin
    else if cfg.package != null then
      "${lib.getExe' cfg.package "russel-ctrl"}"
    else
      "${pkgs.coreutils}/bin/false";
  # uid is often null at eval (allocated at activation). Set XDG_RUNTIME_DIR
  # at start so rootless Podman finds /run/user/<uid>.
  execStartWrapped = pkgs.writeShellScript "russel-ctrl-start" ''
    set -euo pipefail
    export XDG_RUNTIME_DIR="''${XDG_RUNTIME_DIR:-/run/user/$(${pkgs.coreutils}/bin/id -u)}"
    exec ${lib.escapeShellArg execStart} "$@"
  '';
in
{
  options.services.russel = {
    enable = mkEnableOption "Russel control plane (russel-ctrl)";

    package = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = ''
        Package that provides `russel-ctrl` at `$out/bin/russel-ctrl`.
        Set this or `services.russel.bin`.
      '';
    };

    bin = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/usr/local/bin/russel-ctrl";
      description = ''
        Absolute path to a prebuilt `russel-ctrl` (for example
        `cargo build --release`). When set, this is used instead of
        `services.russel.package`.
      '';
    };

    user = mkOption {
      type = types.str;
      default = "russel";
      description = ''
        Unix user that runs russel-ctrl. Do not use DynamicUser: Podman
        and KVM need a real account. On a no-KVM VPS, set this to the
        lingering user that already has rootless Podman.
      '';
    };

    group = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "users";
      description = ''
        Group for the control plane and `/var/lib/russel`.
        Null with createUser=true creates a group named after `user`.
        Null with createUser=false omits systemd Group, so systemd uses
        the user's primary group. Set this when createUser is false and
        you want a specific group:
        `user = "you"; group = "users"; createUser = false`.
      '';
    };

    createUser = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Create `user`/`group` if missing. The account is a lingering
        system user with a subuid/subgid range for rootless Podman.
        Set false when `user` is an existing login account. Linger is
        still set on that account.
      '';
    };

    rootlessPodman = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Documented VPS path is containers via rootless Podman.
        True sets NoNewPrivileges=false so newuidmap/newgidmap can run,
        puts /run/wrappers on PATH, and defaults
        virtualisation.podman.enable on.
        False only keeps NoNewPrivileges=true. It does not configure
        microVMs (see `microvms.enable`). Use that only when this
        unit never starts Podman.
      '';
    };

    bindAddress = mkOption {
      type = types.str;
      default = "127.0.0.1:7878";
      description = ''
        `RUSSEL_CTRL_ADDR`. Stays on loopback. Do not bind a public
        interface. Terminate TLS at a reverse proxy
        (docs/security-tls.md).
      '';
    };

    environmentFile = mkOption {
      type = types.str;
      example = "/etc/russel/env";
      description = ''
        systemd `EnvironmentFile` path. Must contain
        `RUSSEL_API_TOKEN=` plus at least 32 printable ASCII characters.
        systemd reads it as root, so use root:<group> mode 0640 (the
        installer's layout) and add operators to the group. Typed as string
        so Nix does not copy the secret into the store.
      '';
    };

    microvms = {
      enable = mkEnableOption ''
        experimental microVMs (`service.type = "microvm"`). The unit joins
        the `kvm` group and gets cloud-hypervisor, virtiofsd, and passt on
        PATH. ctrl stays unprivileged: passt is the VM NIC and publishes
        its port, as pasta does for rootless Podman (#461)
      '';

      kernel = mkOption {
        type = types.nullOr types.path;
        default = null;
        example = lib.literalExpression ''"''${inputs.russel.packages.''${pkgs.system}.microvm-kernel}/bzImage"'';
        description = ''
          Guest kernel (`RUSSEL_KERNEL_PATH`). Must be Russel's microVM
          kernel (virtio and fuse built in). Null leaves ctrl's own lookup:
          `/var/lib/russel/_pool/kernel/bzImage`.
        '';
      };
    };

    extraEnvironment = mkOption {
      type = types.attrsOf types.str;
      default = { };
      example = {
        RUSSEL_PUBLISH_BIND = "0.0.0.0";
      };
      description = ''
        Extra environment for russel-ctrl. `RUSSEL_WARM_POOL` is forced
        off by this module.
      '';
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.bin != null || cfg.package != null;
        message = "services.russel.enable requires services.russel.package or services.russel.bin";
      }
      {
        assertion = userExists;
        message = "services.russel.user '${cfg.user}' must exist (set createUser = true, or define the user)";
      }
      {
        assertion = groupExists;
        message = "services.russel.group '${resolvedGroup}' must exist (set createUser = true, set group to an existing group, or leave group null to use the user's primary group)";
      }
      {
        assertion =
          lib.hasPrefix "127.0.0.1:" cfg.bindAddress
          || lib.hasPrefix "localhost:" cfg.bindAddress
          || lib.hasPrefix "[::1]:" cfg.bindAddress;
        message = "services.russel.bindAddress must stay on loopback (127.0.0.1, localhost, or [::1]). Put TLS on a reverse proxy; see docs/security-tls.md.";
      }
    ];

    virtualisation.podman.enable = mkIf cfg.rootlessPodman (mkDefault true);

    users.groups = lib.optionalAttrs (cfg.createUser && resolvedGroup != null) {
      ${resolvedGroup} = { };
    };

    users.users.${cfg.user} = {
      linger = true;
    }
    // lib.optionalAttrs cfg.createUser {
      isSystemUser = true;
      group = resolvedGroup;
      home = "/var/lib/russel";
      autoSubUidGidRange = true;
    };

    systemd.services.russel = {
      description = "Russel control plane";
      documentation = [
        "https://github.com/daschinmoy21/russel"
      ];
      # passt copies the host's address and routes when it starts. Under
      # network.target alone it started before DHCP, so microVMs relaunched at
      # boot were unreachable (#420).
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];
      path = [
        "/run/wrappers"
      ]
      ++ (with pkgs; [
        bash
        cacert
        coreutils
        git
        nix
        openssh
        # pkill: microVM stop/destroy falls back to it for CH and passt.
        procps
      ])
      ++ [ config.virtualisation.podman.package ]
      ++ lib.optionals cfg.microvms.enable (
        with pkgs;
        [
          cloud-hypervisor
          passt
          virtiofsd
        ]
      );
      environment = {
        RUSSEL_CTRL_ADDR = cfg.bindAddress;
        RUSSEL_REQUIRE_AUTH = "1";
        HOME = "/var/lib/russel";
        SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        NIX_SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        # Keeps every root under /var/lib/russel (microVM markers included).
        RUSSEL_DATA_DIR = "/var/lib/russel";
        # ctrl builds helpers (busybox, curl, bash) with `nix build -f <nixpkgs>`.
        # Units get no NIX_PATH, so pin the nixpkgs this module was built with.
        NIX_PATH = "nixpkgs=${pkgs.path}";
      }
      // lib.optionalAttrs (cfg.microvms.enable && cfg.microvms.kernel != null) {
        # Interpolate, not toString: a path literal such as ./bzImage is copied
        # into the store and the unit references it, so GC cannot collect it.
        RUSSEL_KERNEL_PATH = "${cfg.microvms.kernel}";
      }
      // cfg.extraEnvironment
      // {
        # Last so extraEnvironment cannot turn the experimental pool on.
        RUSSEL_WARM_POOL = "0";
      };
      unitConfig = {
        RequiresMountsFor = "/run/user/%U";
      };
      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        ExecStart = execStartWrapped;
        EnvironmentFile = cfg.environmentFile;
        WorkingDirectory = "/var/lib/russel";
        StateDirectory = "russel";
        StateDirectoryMode = "0700";
        Restart = "on-failure";
        RestartSec = "5s";
        # false when rootlessPodman: newuidmap needs file caps.
        NoNewPrivileges = !cfg.rootlessPodman;
        # No PrivateTmp / ProtectSystem / ProtectHome: rootless Podman's pause
        # process outlives this unit (KillMode below) and pins the first ctrl's
        # mount namespace, so a PrivateTmp /tmp deleted on restart breaks
        # every later podman call. Isolation is the unprivileged account (#524).
        # cgroup delegation for rootless Podman. Harmless if unused.
        Delegate = true;
        # Workloads (pasta/conmon) are children of this unit. control-group
        # would kill them on restart even though ctrl detaches.
        KillMode = "process";
      }
      // lib.optionalAttrs (resolvedGroup != null) {
        Group = resolvedGroup;
      }
      // lib.optionalAttrs cfg.microvms.enable {
        # /dev/kvm only; no capabilities (passt networking, #461).
        SupplementaryGroups = [ "kvm" ];
      };
    };
  };
}
