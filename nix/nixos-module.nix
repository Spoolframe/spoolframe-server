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

  # Each sign-in provider's client secret (P3-04), a systemd credential
  # as the Plex token is, so its file can stay root-only.
  oidcCredential = i: "/run/credentials/spoolframe.service/oidc-${toString i}";
  oidcProvider = i: p:
    "{ name = ${quote p.name}; issuer = ${quote p.issuer}; client_id = ${quote p.clientId}; "
    + "client_secret_file = ${quote (oidcCredential i)}; "
    + lib.optionalString (p.scopes != null) "scopes = ${quote p.scopes}; "
    + "}";

  # Reads bitmap subtitles as text (P2-07), with only the languages asked
  # for: all of tesseract's data is over a gigabyte.
  tesseract = pkgs.tesseract.override {enableLanguages = cfg.subtitles.ocrLanguages;};

  pathMapping = m:
    "{ "
    + lib.optionalString (m.library != null) "library = ${quote m.library}; "
    + "from = ${quote m.from}; to = ${quote m.to}; }";

  # The provided layer (P3-10), locked: what it sets, the web UI cannot
  # change, so the host stays declarative. What it leaves out (Plex, when
  # plex.url is null) can be set at run time, and is kept in the state
  # directory's runtime.ucl.
  configFile = pkgs.writeText "spoolframe-server.ucl" ''
    schema = "spoolframe.server.v1";
    locked = true;
    name = ${quote cfg.name};
    ${lib.optionalString (cfg.plex.url != null) ''
      plex {
        url = ${quote cfg.plex.url};
        ${lib.optionalString (cfg.plex.tokenFile != null) "token_file = ${quote tokenCredential};"}
        ${lib.optionalString (cfg.plex.pathMappings != []) ''
        path_mapping [
          ${lib.concatMapStringsSep ",\n    " pathMapping cfg.plex.pathMappings}
        ]
      ''}
      }
    ''}
    network {
      port = ${toString cfg.port};
      mdns = ${lib.boolToString cfg.mdns};
    }
    admin {
      ${lib.optionalString (cfg.admin.listen != null) "listen = ${quote cfg.admin.listen};"}
      ${lib.optionalString (cfg.admin.publicUrl != null) "public_url = ${quote cfg.admin.publicUrl};"}
      ${lib.optionalString (cfg.admin.trustedProxies != []) "trusted_proxies = ${quote (lib.concatStringsSep ", " cfg.admin.trustedProxies)};"}
    }
    transcode {
      hardware = ${quote hardware};
      vaapi_device = ${quote cfg.transcode.vaapiDevice};
      scratch_dir = "/var/cache/spoolframe/transcode";
    }
    media {
      tesseract = ${quote "${tesseract}/bin/tesseract"};
    }
    subtitles {
      ocr = ${lib.boolToString (cfg.subtitles.ocrLanguages != [])};
      cache_dir = "/var/cache/spoolframe/subtitles";
    }
    ${lib.optionalString (cfg.auth.oidc != []) ''
      auth {
        oidc [
          ${lib.concatStringsSep ",\n    " (lib.imap0 oidcProvider cfg.auth.oidc)}
        ]
      }
    ''}
    ${cfg.extraConfig}
  '';

  # The admin CLI with this host's config, so `spoolframe user list` works
  # without remembering where the config is.
  cli = pkgs.writeShellScriptBin "spoolframe" ''
    exec ${cfg.package}/bin/spoolframe-server --config /etc/spoolframe/server.ucl --state-dir ${quote cfg.stateDir} "$@"
  '';
in {
  imports = [
    (lib.mkRemovedOptionModule ["services" "spoolframe" "admin" "allowRemote"] ''
      The admin listener takes any address now (P3-10): set
      services.spoolframe.admin.listen, and put a TLS proxy in front of it.
    '')
  ];

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
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "http://127.0.0.1:32400";
        description = ''
          The Plex Media Server on the LAN. Never plex.tv. Null leaves Plex
          to the setup wizard in the web UI, which keeps it (and a token
          made by signing in with Plex) in the state directory; until then
          the server runs "not set up".
        '';
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
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "127.0.0.1:7421";
        description = ''
          Where the web UI and the admin API (JSON-RPC over WebSocket, the
          `spoolframe` CLI's) listen; null is the server's default, every
          IPv4 interface on TCP 7421. Passwords cross it in clear: put a
          TLS proxy (nginx, Caddy) in front, with {option}`admin.publicUrl`,
          and listen on loopback when the proxy is on this host. Over plain
          HTTP on the LAN (the firewall's TCP port opened by hand) the
          setup code and the wizard work, but signing in sticks only with
          the web UI's insecure cookies setting.
        '';
      };

      publicUrl = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "https://media.example.org";
        description = ''
          Where browsers reach the web UI through a TLS reverse proxy: the
          only origin then allowed to sign in and use it. Left unset, a
          browser must be on the listener's own host (directly, or through
          an SSH tunnel). Its host is also what passkeys are bound to
          (`localhost` when unset): they need HTTPS, and changing the host
          orphans the ones already made, which the log names at start.
        '';
      };

      trustedProxies = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [];
        example = ["10.0.0.5" "172.17.0.0/16"];
        description = ''
          The reverse proxies in front of {option}`admin.listen`, as
          addresses and CIDR ranges. The limit on failed logins counts a
          connection from one of them under the caller its
          `X-Forwarded-For` names (the right-most address that is not a
          trusted proxy; `Forwarded` is not read), and under no address
          if it names none. Left empty, no header is read: every
          connection counts under its own address, and a loopback one
          under none.
        '';
      };
    };

    auth.oidc = lib.mkOption {
      type = lib.types.listOf (lib.types.submodule {
        options = {
          name = lib.mkOption {
            type = lib.types.str;
            example = "Pocket ID";
            description = "What the login page's button says: \"Continue with <name>\".";
          };
          issuer = lib.mkOption {
            type = lib.types.str;
            example = "https://id.example.org";
            description = ''
              The provider's issuer, exactly as its ID tokens and its
              discovery document (<issuer>/.well-known/openid-configuration)
              say it. Everything else about the provider comes from that
              document.
            '';
          };
          clientId = lib.mkOption {
            type = lib.types.str;
            description = "Spoolframe's client id at the provider.";
          };
          clientSecretFile = lib.mkOption {
            type = lib.types.path;
            description = ''
              A file holding the client secret. Passed as a systemd
              credential, so it may be readable by root only.
            '';
          };
          scopes = lib.mkOption {
            type = lib.types.nullOr (lib.types.listOf lib.types.str);
            default = null;
            description = "The scopes asked for; null is openid, profile and email.";
          };
        };
      });
      default = [];
      description = ''
        Single sign-on (P3-04): OpenID Connect providers a user may link to
        their account in My profile, and then sign in with. A provider never
        makes accounts. Needs {option}`admin.publicUrl`: register
        `<publicUrl>/auth/oidc/callback` as the client's redirect URI, a
        confidential client with PKCE.
      '';
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

    subtitles.ocrLanguages = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = ["eng"];
      example = ["eng" "fra" "deu"];
      description = ''
        tesseract languages for reading bitmap (PGS, VobSub, DVB) subtitles
        as text, for clients that cannot draw them or that ask for text. A
        track is read only with its own language's data; tracks in other
        languages are burned in instead. Empty turns reading off.
      '';
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
      description = "More UCL for the server config (`artwork`, `watch`, `previews`).";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = lib.hasPrefix "/var/lib/" cfg.stateDir;
        message = "services.spoolframe.stateDir (${cfg.stateDir}) must be under /var/lib/.";
      }
      {
        assertion = cfg.auth.oidc == [] || cfg.admin.publicUrl != null;
        message = "services.spoolframe.auth.oidc needs services.spoolframe.admin.publicUrl: the provider sends the browser back to <publicUrl>/auth/oidc/callback.";
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
        ExecStart = "${cfg.package}/bin/spoolframe-server --config ${configFile} --state-dir ${cfg.stateDir}";
        User = "spoolframe";
        Group = "spoolframe";
        StateDirectory = lib.removePrefix "/var/lib/" cfg.stateDir;
        StateDirectoryMode = "0700";
        # Encode output (P2-03), emptied by the server at start, and
        # subtitles read as text (P2-07), kept.
        CacheDirectory = "spoolframe";
        CacheDirectoryMode = "0700";
        LoadCredential =
          lib.optional (cfg.plex.tokenFile != null) "plex-token:${cfg.plex.tokenFile}"
          ++ lib.imap0 (i: p: "oidc-${toString i}:${p.clientSecretFile}") cfg.auth.oidc;
        # Also how the web UI's "Restart now" (P3-10) brings the server
        # back: it exits with 75 when asked to restart, and offers it only
        # where SPOOLFRAME_SUPERVISED says something will.
        Restart = "on-failure";
        Environment = ["SPOOLFRAME_SUPERVISED=systemd"];
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
