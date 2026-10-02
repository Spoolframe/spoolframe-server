{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.spoolframe;

  # "auto", "none", or families in order.
  hardware =
    if builtins.isList cfg.transcode.hardware
    then lib.concatStringsSep "," cfg.transcode.hardware
    else cfg.transcode.hardware;
  uses = family: cfg.transcode.hardware == "auto" || (builtins.isList cfg.transcode.hardware && builtins.elem family cfg.transcode.hardware);
  vaapi = uses "vaapi" || uses "qsv";
  nvenc = uses "nvenc";
  gpu = vaapi || nvenc;

  # systemd hands the Plex token to the service as a credential, so the
  # secret file can stay root-only.
  tokenCredential = "/run/credentials/spoolframe.service/plex-token";

  quote = builtins.toJSON;

  pathMapping = m:
    "{ "
    + lib.optionalString (m.library != null) "library = ${quote m.library}; "
    + "from = ${quote m.from}; to = ${quote m.to}; }";

  configFile = pkgs.writeText "spoolframe-server.ucl" ''
    schema = "spoolframe.server.v1";
    name = ${quote cfg.name};
    state_dir = ${quote cfg.stateDir};
    plex {
      url = ${quote cfg.plex.url};
      ${lib.optionalString (cfg.plex.tokenFile != null) "token_file = ${quote tokenCredential};"}
      ${lib.optionalString (cfg.plex.pathMappings != []) ''
      path_mapping [
        ${lib.concatMapStringsSep ",\n    " pathMapping cfg.plex.pathMappings}
      ]
    ''}
    }
    network {
      port = ${toString cfg.port};
      mdns = ${lib.boolToString cfg.mdns};
    }
    admin {
      listen = ${quote cfg.admin.listen};
      allow_remote = ${lib.boolToString cfg.admin.allowRemote};
    }
    transcode {
      hardware = ${quote hardware};
      vaapi_device = ${quote cfg.transcode.vaapiDevice};
      scratch_dir = "/var/cache/spoolframe/transcode";
    }
    ${cfg.extraConfig}
  '';

  # The admin CLI with this host's config, so `spoolframe user list` works
  # without remembering where the config is.
  cli = pkgs.writeShellScriptBin "spoolframe" ''
    exec ${cfg.package}/bin/spoolframe-server --config /etc/spoolframe/server.ucl "$@"
  '';
in {
  options.services.spoolframe = {
    enable = lib.mkEnableOption "the Spoolframe media server";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The spoolframe-server package.";
    };

    name = lib.mkOption {
      type = lib.types.str;
      default = config.networking.hostName;
      description = "What clients show for this server.";
    };

    stateDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/spoolframe";
      description = ''
        The server's key, database and caches. Must be under /var/lib, where
        systemd's StateDirectory creates it. The key in it is the server's
        identity: clients pin it, so keep it in backups.
      '';
    };

    plex = {
      url = lib.mkOption {
        type = lib.types.str;
        example = "http://127.0.0.1:32400";
        description = "The Plex Media Server on the LAN. Never plex.tv.";
      };

      tokenFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = ''
          A file holding the X-Plex-Token; null for a PMS that lets LAN
          clients in without auth. Passed as a systemd credential, so it may
          be readable by root only.
        '';
      };

      pathMappings = lib.mkOption {
        type = lib.types.listOf (lib.types.submodule {
          options = {
            library = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "The Plex library title this rule is for; every library if null.";
            };
            from = lib.mkOption {
              type = lib.types.str;
              description = "The path prefix as Plex reports it.";
            };
            to = lib.mkOption {
              type = lib.types.str;
              description = "The same directory on this host.";
            };
          };
        });
        default = [];
        description = ''
          Rewrites from the paths Plex reports to the paths this host reads.
          None needed when Plex runs on this host and sees the same paths.
        '';
      };
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 7420;
      description = "The UDP port clients connect to (QUIC, IPv4 and IPv6).";
    };

    mdns = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Announce the server on the LAN, so clients list it.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Open {option}`port` and, with {option}`mdns`, UDP 5353, on every
        interface. Leave it off and open them per interface when the host
        has more networks than the LAN.
      '';
    };

    admin = {
      listen = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1:7421";
        description = ''
          The admin API (JSON-RPC over WebSocket) that the `spoolframe` CLI
          uses. Loopback: reach it from elsewhere through an SSH tunnel.
        '';
      };

      allowRemote = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Allow a non-loopback {option}`admin.listen`. Passwords cross the
          admin API in clear, so only behind a TLS reverse proxy.
        '';
      };
    };

    transcode = {
      hardware = lib.mkOption {
        type = lib.types.either (lib.types.enum ["auto" "none"]) (lib.types.nonEmptyListOf (lib.types.enum ["vaapi" "nvenc" "qsv"]));
        default = "auto";
        example = ["nvenc" "vaapi"];
        description = ''
          Which hardware encoders to try. The server test-encodes with each
          at start (and on SIGHUP) and keeps what works; `spoolframe status`
          lists it. A list sets the order per codec: `["nvenc" "vaapi"]`
          encodes on NVIDIA what it can and the rest on VA-API. `none`
          encodes in software only.
        '';
      };

      vaapiDevice = lib.mkOption {
        type = lib.types.str;
        default = "/dev/dri/renderD128";
        example = "/dev/dri/by-path/pci-0000:c8:00.0-render";
        description = ''
          The render node VA-API encodes on. With more than one GPU, name it
          by PCI path: the renderD numbers follow probe order and can swap
          after a reboot.
        '';
      };
    };

    mediaGroups = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      example = ["media"];
      description = "Groups the server joins to read the media files.";
    };

    extraConfig = lib.mkOption {
      type = lib.types.lines;
      default = "";
      example = "artwork { cache_mb = 4096; }";
      description = "More UCL for the server config (`artwork`, `watch`).";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = lib.hasPrefix "/var/lib/" cfg.stateDir;
        message = "services.spoolframe.stateDir (${cfg.stateDir}) must be under /var/lib/.";
      }
    ];

    environment.etc."spoolframe/server.ucl".source = configFile;
    environment.systemPackages = [cli];

    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedUDPPorts = [cfg.port] ++ lib.optional cfg.mdns 5353;
    };

    users.users.spoolframe = {
      isSystemUser = true;
      group = "spoolframe";
      # render owns the DRM render nodes VA-API encodes on. NVIDIA's nodes
      # are world-accessible; the card nodes (group video) are not needed.
      extraGroups = cfg.mediaGroups ++ lib.optional vaapi "render";
      home = cfg.stateDir;
    };
    users.groups.spoolframe = {};

    systemd.services.spoolframe = {
      description = "Spoolframe media server";
      wantedBy = ["multi-user.target"];
      after = ["network-online.target"];
      wants = ["network-online.target"];
      # A new config is a restart: the server reads it once.
      restartTriggers = [configFile];
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/spoolframe-server --config ${configFile}";
        User = "spoolframe";
        Group = "spoolframe";
        StateDirectory = lib.removePrefix "/var/lib/" cfg.stateDir;
        StateDirectoryMode = "0700";
        # Encode output (P2-03): emptied by the server at start.
        CacheDirectory = "spoolframe";
        CacheDirectoryMode = "0700";
        LoadCredential = lib.optional (cfg.plex.tokenFile != null) "plex-token:${cfg.plex.tokenFile}";
        Restart = "on-failure";
        RestartSec = 2;

        # Reads media, writes only its state directory.
        ProtectSystem = "strict";
        ProtectHome = "read-only";
        PrivateTmp = true;
        # No other devices; the GPU's only when hardware encoding may be
        # used.
        PrivateDevices = !gpu;
        DevicePolicy = "closed";
        DeviceAllow =
          lib.optionals vaapi ["char-drm rw"]
          ++ lib.optionals nvenc ["char-nvidia-frontend rw" "char-nvidia-uvm rw" "char-nvidia-caps rw"];
        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictNamespaces = true;
        RestrictRealtime = true;
        LockPersonality = true;
        # Netlink: the endpoint watches the host's interfaces for address
        # changes.
        RestrictAddressFamilies = ["AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK"];
        CapabilityBoundingSet = "";
      };
    };
  };
}
