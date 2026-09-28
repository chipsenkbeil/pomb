# ChipFlow NixOS module (draft for the homelab repo).
#
# Drop this file into the homelab repo as `modules/chipflow.nix`, register it
# in `modules/default.nix`, and enable it on green-box with:
#
#   homelab.chipflow.enable = true;
#
# Then add one Caddy route in `modules/proxy.nix`:
#
#   chipflow = simple "127.0.0.1:3000";
#
# which exposes it as https://chipflow.chip.network (wildcard cert already
# covers it; no DNS work needed).
#
# The GitHub `rev` below must be a pinned commit SHA from the chipflow repo.
# On the first build, nix will print the correct `sha256-...` hash for
# `src.hash` — paste it in. Bump `rev` + hash together on updates.

{ config, lib, pkgs, ... }:

let
  cfg = config.homelab.chipflow;

  chipflowSrc = pkgs.fetchFromGitHub {
    owner = "chipsenkbeil";
    repo = "chipflow";
    rev = "CHIPFLOW_PINNED_SHA"; # TODO: pin a commit SHA
    hash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="; # TODO: nix will print the real one
  };

  chipflowPkg = pkgs.rustPlatform.buildRustPackage {
    pname = "chipflow";
    version = "0.1.0";

    src = chipflowSrc;

    cargoLock = {
      lockFile = "${chipflowSrc}/Cargo.lock";
    };
  };
in
{
  options.homelab.chipflow = {
    enable = lib.mkEnableOption "ChipFlow kanban + pomodoro service";

    port = lib.mkOption {
      type = lib.types.port;
      default = 3000;
      description = "Localhost port ChipFlow listens on (Caddy proxies to it).";
    };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/chipflow";
      description = "Directory holding the redb database file.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.chipflow = {
      isSystemUser = true;
      group = "chipflow";
      description = "ChipFlow service user";
    };
    users.groups.chipflow = { };

    systemd.tmpfiles.rules = [
      "d ${cfg.dataDir} 0750 chipflow chipflow -"
    ];

    systemd.services.chipflow = {
      description = "ChipFlow kanban + pomodoro server";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];

      environment = {
        HOST = "127.0.0.1"; # Caddy terminates TLS and proxies in
        PORT = toString cfg.port;
        DATABASE_PATH = "${cfg.dataDir}/chipflow.redb";
      };

      serviceConfig = {
        Type = "simple";
        User = "chipflow";
        Group = "chipflow";
        ExecStart = "${lib.getExe chipflowPkg}";
        Restart = "always";
        RestartSec = "5s";

        # Hardening: single-purpose service, no need for more.
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ReadWritePaths = [ cfg.dataDir ];
      };
    };
  };
}
