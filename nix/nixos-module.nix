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

  # Each media tool confined by Landlock and seccomp (PX-11 finding 34).
  sandboxed = cfg.media.sandbox != "off";

  # A loopback listen address: the proxy in front of it is on this host.
  loopback = listen: listen != null && (lib.hasPrefix "127." listen || lib.hasPrefix "[::1]:" listen);

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
      ${lib.optionalString cfg.plexDiscovery.enable "plex_discovery_port = ${toString cfg.plexDiscovery.port};"}
    }
    admin {
      ${lib.optionalString (cfg.admin.listen != null) "listen = ${quote cfg.admin.listen};"}
      ${lib.optionalString (!cfg.admin.allowInsecureCookies) "insecure_cookies = false;"}
      ${lib.optionalString (cfg.admin.publicUrl != null) "public_url = ${quote cfg.admin.publicUrl};"}
      ${lib.optionalString (cfg.admin.trustedProxies != []) "trusted_proxies = ${quote (lib.concatStringsSep ", " cfg.admin.trustedProxies)};"}
      ${lib.optionalString cfg.admin.notProxied "not_proxied = true;"}
    }
    transcode {
      hardware = ${quote hardware};
      vaapi_device = ${quote cfg.transcode.vaapiDevice};
      scratch_dir = "/var/cache/spoolframe/transcode";
    }
    media {
      tesseract = ${quote "${tesseract}/bin/tesseract"};
      sandbox = ${quote cfg.media.sandbox};
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

    plexDiscovery = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Let the setup wizard find Plex servers on the LAN through this
          host's firewall. Plex's discovery (GDM) is a search sent to a
          multicast group and the broadcast address; each Plex server
          answers to the port it came from, and the firewall drops that
          answer, since it cannot match it to a multicast search. This sends
          the search from {option}`plexDiscovery.port` and opens that UDP
          port when the firewall is on. Off, neither: the server's own
          setting applies, and with a firewall on, discovery finds nothing
          (a server's address can still be typed).
        '';
      };

      port = lib.mkOption {
        type = lib.types.port;
        default = 32415;
        description = ''
          The UDP port the search is sent from and answered on. Not one of
          Plex's own (32410-32414), which a Plex server on this host holds.
        '';
      };

      interfaces = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [];
        example = ["eno1"];
        description = ''
          The interfaces {option}`plexDiscovery.port` is opened on when
          {option}`openFirewall` is off. With neither, the port stays
          closed and discovery finds nothing through the firewall (an
          address can still be typed). The server takes an answer only
          from a network of the interface it came in on.
        '';
      };
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Open {option}`port`, with {option}`mdns` UDP 5353, and with
        {option}`plexDiscovery.enable` the discovery port, on every
        interface. Leave it off and open them per interface when the host
        has more networks than the LAN
        ({option}`plexDiscovery.interfaces` for the discovery port).
      '';
    };

    admin = {
      listen = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        example = "0.0.0.0:7421";
        default = "127.0.0.1:7421";
        description = ''
          Where the web UI and the admin API (JSON-RPC over WebSocket, the
          `spoolframe` CLI's) listen. Loopback by default (PX-11): reach it
          through a TLS proxy (nginx, Caddy) on this host, with
          {option}`admin.publicUrl` and {option}`admin.trustedProxies`, or
          through an SSH tunnel. Passwords cross it in clear, so give it
          another address only with a proxy in front; null is the server's
          own default, every IPv4 interface. Over plain HTTP on the LAN
          (the firewall's TCP port opened by hand) the setup code and the
          wizard work, but signing in sticks only with insecure cookies
          ({option}`admin.allowInsecureCookies`).
        '';
      };

      publicUrl = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "https://media.example.org";
        description = ''
          Where browsers reach the web UI through a TLS reverse proxy: the
          only origin then allowed to sign in and use it. Left unset, a
          browser must reach the listener by its address or as `localhost`
          (directly, or through an SSH tunnel), never by a name. Its host
          is also what passkeys are bound to (`localhost` when unset, at the
          listener's own port, so a tunnel keeps the port): they need HTTPS,
          and changing the host orphans the ones already made, which the
          log names at start.
        '';
      };

      allowInsecureCookies = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Leave the server's insecure cookies setting to the web UI. Off,
          the module locks it off: an admin, or the setup code before the
          first admin, cannot send the session cookie without `Secure`.
          On is only for signing in over plain HTTP on the LAN; the server
          still refuses it with an `https` {option}`admin.publicUrl`.
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
          under none. With {option}`admin.listen` on loopback and
          {option}`admin.publicUrl` set, a proxy on this host is in front,
          so this must name it (`["127.0.0.1"]`), or
          {option}`admin.notProxied` must say there is none: the module
          does not evaluate otherwise, and the server does not start.
        '';
      };

      notProxied = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Nothing proxies to {option}`admin.listen`: its loopback callers
          are on this host, or come through an SSH tunnel, and share one
          limit on failed logins. Only needed for a loopback listener with
          {option}`admin.publicUrl` set, which otherwise means a proxy on
          this host that {option}`admin.trustedProxies` must name (PX-11
          finding 38).
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

    media.sandbox = lib.mkOption {
      type = lib.types.enum ["required" "auto" "off"];
      default = "required";
      description = ''
        How ffmpeg, ffprobe and tesseract run (PX-11 finding 34). They
        parse files anyone who can write to a library folder made, so by
        default each run confines itself, inside the service's sandbox,
        with Landlock and a seccomp filter: no network, no state directory
        (database, key, secrets), the one media file read-only and its own
        scratch directory the only place it writes, no new processes. The
        server shows it at start: a confined ffprobe must be refused the
        state directory and loopback. `required` refuses to start
        otherwise; `auto` runs them with the seccomp filter alone, with an
        error in the log, on a kernel without Landlock (before Linux 6.2,
        or not in `lsm=`); `off` never confines them. Their environment is
        cleared and their memory, CPU time and file sizes are limited
        either way. Confining them needs no namespace and nothing the unit
        otherwise refuses, but the three `landlock_*` calls.
      '';
    };

    mediaUnderHome = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Whether media is under /home. Off, the service sees no /home at
        all (ProtectHome=true); on, it sees /home read-only.
      '';
    };

    resources = {
      memoryHigh = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = "6G";
        description = ''
          systemd's MemoryHigh: past it the service is slowed and its
          memory reclaimed. Transcodes, previews and OCR are what use it.
          Null leaves it unset.
        '';
      };
      memoryMax = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = "8G";
        description = "systemd's MemoryMax: past it the kernel kills in the service. Null leaves it unset.";
      };
      tasksMax = lib.mkOption {
        type = lib.types.nullOr lib.types.ints.positive;
        default = 4096;
        description = ''
          systemd's TasksMax: threads and processes, ffmpeg's and
          tesseract's included. Null leaves it unset.
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
      # PX-11 finding 38 (PX-09 finding 29): behind a proxy on this host
      # every caller looks like loopback unless the proxy is named. The
      # server refuses to start in this shape too.
      {
        assertion = !(loopback cfg.admin.listen && cfg.admin.publicUrl != null && cfg.admin.trustedProxies == [] && !cfg.admin.notProxied);
        message = ''
          services.spoolframe.admin.trustedProxies is empty, but admin.listen is loopback and
          admin.publicUrl is set, so a proxy on this host is in front, and every caller through it
          would look like loopback: failed logins would not be limited per caller, and /metrics,
          when on, would answer whoever the proxy lets in. Set admin.trustedProxies = ["127.0.0.1"];
          or, if nothing proxies to the listener, admin.notProxied = true.'';
      }
    ];

    environment.etc."spoolframe/server.ucl".source = configFile;
    environment.systemPackages = [cli];

    # Answers to the wizard's search for Plex servers are unicast replies
    # to a multicast send, which the firewall cannot track: their port is
    # opened as the others are, everywhere or per interface (PX-11
    # finding 36).
    networking.firewall.allowedUDPPorts = lib.mkIf cfg.openFirewall (
      [cfg.port] ++ lib.optional cfg.mdns 5353 ++ lib.optional cfg.plexDiscovery.enable cfg.plexDiscovery.port
    );
    networking.firewall.interfaces = lib.mkIf (cfg.plexDiscovery.enable && !cfg.openFirewall) (
      lib.genAttrs cfg.plexDiscovery.interfaces (_: {allowedUDPPorts = [cfg.plexDiscovery.port];})
    );

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
        ProtectHome =
          if cfg.mediaUnderHome
          then "read-only"
          else true;
        PrivateTmp = true;
        # What it writes is its own (PX-11 finding 35); the server sets
        # the same umask itself.
        UMask = "0077";
        # No other devices; the GPU's only when hardware encoding may be
        # used: VA-API's one render node, not every DRM node.
        PrivateDevices = !gpu;
        DevicePolicy = "closed";
        DeviceAllow =
          lib.optionals vaapi ["${cfg.transcode.vaapiDevice} rw"]
          ++ lib.optionals nvenc ["char-nvidia-frontend rw" "char-nvidia-uvm rw" "char-nvidia-caps rw"];
        # The GPU's device nodes belong to groups the service's user is in
        # (render), which a user namespace would hide.
        PrivateUsers = !gpu;
        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        # Other processes are not its business; its own children's /proc
        # entries stay (encode jobs are watched through them).
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        RemoveIPC = true;
        LockPersonality = true;
        # Netlink: the endpoint watches the host's interfaces for address
        # changes.
        RestrictAddressFamilies = ["AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK"];
        CapabilityBoundingSet = "";
        SystemCallArchitectures = "native";
        # A service's usual calls, less the privileged ones and those that
        # change resource settings, but for the two previews use to run
        # ffmpeg at the lowest CPU and I/O priority. A refused call fails
        # with EPERM rather than killing the server.
        # And Landlock's three calls (media.sandbox), which no group of
        # @system-service has: the server makes each media tool's ruleset,
        # and the tool confines itself with it before it starts. The
        # tool's seccomp filter is loaded with prctl(2), which is allowed.
        SystemCallFilter =
          ["@system-service" "~@privileged @resources" "setpriority ioprio_set"]
          ++ lib.optional sandboxed "landlock_create_ruleset landlock_add_rule landlock_restrict_self";
        SystemCallErrorNumber = "EPERM";
        # MemoryDenyWriteExecute is left out: GPU drivers JIT shaders, and
        # the VM test has no GPU to show it safe with them.
        MemoryHigh = lib.mkIf (cfg.resources.memoryHigh != null) cfg.resources.memoryHigh;
        MemoryMax = lib.mkIf (cfg.resources.memoryMax != null) cfg.resources.memoryMax;
        TasksMax = lib.mkIf (cfg.resources.tasksMax != null) cfg.resources.tasksMax;
      };
    };
  };
}
