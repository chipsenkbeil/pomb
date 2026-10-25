# Pomb NixOS module (draft for the homelab repo).
#
# Drop this file into the homelab repo as `modules/pomb.nix`, register it
# in `modules/default.nix`, and enable it on green-box with:
#
#   homelab.pomb.enable = true;
#
# Then add one Caddy route in `modules/proxy.nix`:
#
#   pomb = simple "127.0.0.1:3000";
#
# which exposes it as https://pomb.chip.network (wildcard cert already
# covers it; no DNS work needed).
#
# The GitHub `rev` below must be a pinned commit SHA from the pomb repo.
# On the first build, nix will print the correct `sha256-...` hash for
# `src.hash` — paste it in. Bump `rev` + hash together on updates.

{ config, lib, pkgs, ... }:

let
  cfg = config.homelab.pomb;

  pombSrc = pkgs.fetchFromGitHub {
    owner = "chipsenkbeil";
    repo = "pomb";
    rev = "POMB_PINNED_SHA"; # TODO: pin a commit SHA
    hash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="; # TODO: nix will print the real one
  };

  pombPkg = pkgs.rustPlatform.buildRustPackage {
    pname = "pomb";
    version = "0.1.0";

    src = pombSrc;

    cargoLock = {
      lockFile = "${pombSrc}/Cargo.lock";
    };
  };
in
{
  options.homelab.pomb = {
    enable = lib.mkEnableOption "Pomb kanban + pomodoro service";

    port = lib.mkOption {
      type = lib.types.port;
      default = 3000;
      description = "Localhost port Pomb listens on (Caddy proxies to it).";
    };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/pomb";
      description = "Directory holding the redb database file.";
    };

    publicUrl = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "https://pomb.example.com";
      description = "Externally visible base URL (scheme, host, optional port, no path). Sets POMB_PUBLIC_URL when non-null.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.pomb = {
      isSystemUser = true;
      group = "pomb";
      description = "Pomb service user";
    };
    users.groups.pomb = { };

    systemd.tmpfiles.rules = [
      "d ${cfg.dataDir} 0750 pomb pomb -"
    ];

    systemd.services.pomb = {
      description = "Pomb kanban + pomodoro server";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];

      environment = {
        HOST = "127.0.0.1"; # Caddy terminates TLS and proxies in
        PORT = toString cfg.port;
        DATABASE_PATH = "${cfg.dataDir}/pomb.redb";
      } // lib.optionalAttrs (cfg.publicUrl != null) { POMB_PUBLIC_URL = cfg.publicUrl; };

      serviceConfig = {
        Type = "simple";
        User = "pomb";
        Group = "pomb";
        ExecStart = "${lib.getExe pombPkg}";
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
