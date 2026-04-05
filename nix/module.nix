# NixOS module template. Copy into your tool's repo as `nix/module.nix`,
# rename `serviceName`, and import from the host config.
{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.pimsteward;
  serviceName = "pimsteward";
in {
  options.services.pimsteward = {
    enable = lib.mkEnableOption "pimsteward";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The pimsteward package to run.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = serviceName;
      description = "System user to run the service as.";
    };

    configFile = lib.mkOption {
      type = lib.types.path;
      description = "Path to the service's TOML config file.";
    };

    stateDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/${serviceName}";
      description = "Directory for persistent state.";
    };

    secretsDir = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Directory containing dotvault-imported secrets for this service.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.user;
      home = cfg.stateDir;
      createHome = true;
    };
    users.groups.${cfg.user} = {};

    systemd.services.${serviceName} = {
      description = "${serviceName}";
      wantedBy = ["multi-user.target"];
      after = ["network-online.target"];
      wants = ["network-online.target"];
      environment = lib.mkIf (cfg.secretsDir != null) {
        SERVICE_SECRETS_DIR = toString cfg.secretsDir;
      };
      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/${serviceName} --config ${cfg.configFile}";
        Restart = "on-failure";
        RestartSec = "5s";
        User = cfg.user;
        Group = cfg.user;
        StateDirectory = serviceName;
        # Hardening — relax only when something concrete breaks.
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ReadWritePaths = [cfg.stateDir];
        ReadOnlyPaths = lib.mkIf (cfg.secretsDir != null) [cfg.secretsDir];
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictNamespaces = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RestrictRealtime = true;
        SystemCallArchitectures = "native";
      };
    };
  };
}
