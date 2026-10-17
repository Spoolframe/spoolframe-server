{
  config,
  lib,
  pkgs,
  utils,
  ...
}: let
  cfg = config.services.spoolframe;

  # The GPUs the server may use (PGPU-05): transcode.gpus, or what the
  # deprecated hardware and vaapiDevice meant (each family every card of
  # it, VA-API and QSV on vaapiDevice alone when that is set), or every
  # card of every API. The server stopped reading hardware and
  # vaapi_device in 0.12.0, so this module writes them as gpus.
  oldFamilies =
    if cfg.transcode.hardware == null || cfg.transcode.hardware == "auto"
    then ["nvenc" "vaapi" "qsv"]
    else if cfg.transcode.hardware == "none"
    then []
    else cfg.transcode.hardware;
  oldGpus = map (api:
    if api != "nvenc" && cfg.transcode.vaapiDevice != null
    then {
      inherit api;
      pick = null;
      device = cfg.transcode.vaapiDevice;
      maxJobs = null;
    }
    else {
      inherit api;
      pick = "all";
      device = null;
      maxJobs = null;
    })
  oldFamilies;
  gpus =
    if cfg.transcode.gpus != null
    then cfg.transcode.gpus
    else oldGpus;
  oldSet = cfg.transcode.hardware != null || cfg.transcode.vaapiDevice != null;
  # An entry as the server's config takes it.
  gpuEntry = e:
    {inherit (e) api;}
    // lib.optionalAttrs (e.pick != null) {inherit (e) pick;}
    // lib.optionalAttrs (e.device != null) {device = toString e.device;}
    // lib.optionalAttrs (e.maxJobs != null) {max_jobs = e.maxJobs;};
  # An entry as this module's option is written, for the warning.
  gpuNix = e:
    "{ api = ${quote e.api}; "
    + lib.optionalString (e.pick != null) "pick = ${quote e.pick}; "
    + lib.optionalString (e.device != null) "device = ${quote (toString e.device)}; "
    + "}";
  onDrm = e: e.api == "vaapi" || e.api == "qsv";
  vaapi = lib.any onDrm gpus;
  nvenc = lib.any (e: e.api == "nvenc") gpus;
  gpu = vaapi || nvenc;
  # A pick cannot name its render nodes before the server lists the cards:
  # every DRM node then. A device entry names its own.
  vaapiPicks = lib.any (e: onDrm e && e.device == null) gpus;
  vaapiDevices = map (e: toString e.device) (lib.filter (e: onDrm e && e.device != null) gpus);

  # Each media tool confined by Landlock and seccomp (PX-11 finding 34).
  sandboxed = cfg.media.sandbox != "off";

  # The server parses admin.listen as an IP address and port, and judges
  # it by the address (G-12): IPv4 dotted, or IPv6 in brackets, never a
  # name such as localhost or a bracketed IPv4 address, which it refuses.
  matches = re: listen: listen != null && builtins.match re listen != null;
  parsed = listen: matches "[0-9]+\\.[0-9]+\\.[0-9]+\\.[0-9]+:[0-9]+" listen || matches "[[][0-9a-fA-F.]*:[0-9a-fA-F:.%]*[]]:[0-9]+" listen;

  # A loopback listen address: the proxy in front of it is on this host.
  # 127.0.0.0/8, or ::1 however it is spelt (every group but the last 0).
  loopback = listen: matches "127\\.[0-9.]+:[0-9]+" listen || matches "[[][0:]*:0*1[]]:[0-9]+" listen;

  # A listen address on every interface (null is the server's default,
  # 0.0.0.0): a proxy on this host may be in front, or may not.
  everywhere = listen: listen == null || lib.hasPrefix "0.0.0.0:" listen || matches "[[][0:]*:[0:]*[]]:[0-9]+" listen;

  # `network.wan` (P6-02): absent unless set, and then each part as set.
  wanConfig = let
    mapped = cfg.wan.mappedPort;
  in ''
    wan {
      static = [${lib.concatMapStringsSep ", " quote cfg.wan.static}];
      ${
      if mapped == null
      then "mapped_port = false;"
      else ''
        mapped_port {
          ${lib.optionalString (mapped.port != null) "port = ${toString mapped.port};"}
          mode = ${quote mapped.mode};
          public_ip = ${quote mapped.publicIp};
          lookup_via = ${
          if mapped.lookupVia == null
          then "false"
          else quote mapped.lookupVia
        };
        }
      ''
    }
    }
  '';

  # systemd hands the Plex token to the service as a credential, so the
  # secret file can stay root-only.
  tokenCredential = "/run/credentials/spoolframe.service/plex-token";

  quote = builtins.toJSON;
  # The HTTP playback port (PX-49): the port of its listen address, when
  # it is one.
  httpPort =
    if parsed cfg.httpPlayback.listen
    then lib.toInt (lib.last (lib.splitString ":" cfg.httpPlayback.listen))
    else null;

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
      ${lib.optionalString (!cfg.ipv6) "ipv6 = false;"}
      ${lib.optionalString cfg.plexDiscovery.enable "plex_discovery_port = ${toString cfg.plexDiscovery.port};"}
      ${lib.optionalString (cfg.relays != []) "relays = ${quote cfg.relays};"}
      ${lib.optionalString (cfg.pkarr != "off") "pkarr = ${quote cfg.pkarr};"}
      ${lib.optionalString (cfg.ownRelays != []) "own_relays = ${quote cfg.ownRelays};"}
      ${lib.optionalString (cfg.wan != null) wanConfig}
      ${lib.optionalString (cfg.lanPrefixes != []) "lan_prefixes = ${quote cfg.lanPrefixes};"}
      ${lib.optionalString (cfg.congestionControl != null) "congestion_control = ${quote cfg.congestionControl};"}
      ${lib.optionalString (cfg.uplink.budgetMbps != null || cfg.uplink.maxStreams != null || cfg.uplink.ownerSharePercent != null) ''
      uplink {
        ${lib.optionalString (cfg.uplink.budgetMbps != null) "budget_mbps = ${toString cfg.uplink.budgetMbps};"}
        ${lib.optionalString (cfg.uplink.maxStreams != null) "max_streams = ${toString cfg.uplink.maxStreams};"}
        ${lib.optionalString (cfg.uplink.ownerSharePercent != null) "owner_share_percent = ${toString cfg.uplink.ownerSharePercent};"}
      }
    ''}
    }
    admin {
      ${lib.optionalString (cfg.admin.listen != null) "listen = ${quote cfg.admin.listen};"}
      ${lib.optionalString (!cfg.admin.allowInsecureCookies) "insecure_cookies = false;"}
      ${lib.optionalString (cfg.admin.publicUrl != null) "public_url = ${quote cfg.admin.publicUrl};"}
      ${lib.optionalString (cfg.admin.trustedProxies != []) "trusted_proxies = ${quote cfg.admin.trustedProxies};"}
      ${lib.optionalString cfg.admin.notProxied "not_proxied = true;"}
    }
    transcode {
      ${lib.optionalString (cfg.transcode.gpus != null || oldSet) "gpus = ${quote (map gpuEntry gpus)};"}
      scratch_dir = "/var/cache/spoolframe/transcode";
      ${lib.optionalString (cfg.transcode.toneMap != null) "tone_map = ${quote cfg.transcode.toneMap};"}
      ${lib.optionalString (cfg.transcode.videoCodecs != null) "video_codecs = [${lib.concatMapStringsSep ", " quote cfg.transcode.videoCodecs}];"}
    }
    ${lib.optionalString (cfg.httpPlayback.listen != null) ''
      http_playback {
        listen = ${quote cfg.httpPlayback.listen};
        ${lib.optionalString (cfg.httpPlayback.address != null) "address = ${quote cfg.httpPlayback.address};"}
      }
    ''}
    ${lib.optionalString cfg.experimental.libplaceboSharedOnAmd ''
      experimental {
        libplacebo_shared_on_amd = true;
      }
    ''}
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
      description = "The UDP port clients connect to (QUIC, IPv4 and, unless {option}`ipv6` is off, IPv6).";
    };

    ipv6 = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Listen on IPv6 too (`network.ipv6`). Off, the server listens on
        IPv4 only, so no device reaches it over IPv6, directly or by hole
        punching.
      '';
    };

    mdns = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Announce the server on the LAN, so clients list it.";
    };

    relays = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      example = ["https://relay.example.org"];
      description = ''
        The relays devices that cannot dial the server directly reach it
        through (`network.relays`, P6-03): `"default"` for n0's public
        ones, and relay URLs, such as a relay of your own
        (`services.spoolframe-relay`). None: relays off.
      '';
    };

    pkarr = lib.mkOption {
      type = lib.types.str;
      default = "off";
      example = "n0";
      description = ''
        Optional, and only for one case (`network.pkarr`, P6-05): a device
        that knows the server's identity but no longer an address that
        reaches it, as after the public IP changed. `"off"`, `"n0"`
        (dns.iroh.link), or your own pkarr relay's URL. Not needed with
        relays on.
      '';
    };

    ownRelays = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      description = ''
        The keys of relays of your own (`spoolframe-server relay id`) the
        server tells who its devices are, so they let them through
        (`network.own_relays`, P6-04).
      '';
    };

    lanPrefixes = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      example = ["10.10.0.0/24"];
      description = ''
        Address ranges whose devices count as home besides the host's own
        networks (`network.lan_prefixes`, PX-46): a WireGuard network,
        another VLAN. A device at home is not counted against the uplink
        budget, and Auto plays the original for it.
      '';
    };

    congestionControl = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["cubic" "bbr"]);
      default = null;
      example = "bbr";
      description = ''
        How the server paces what it sends (`network.congestion_control`):
        "cubic" backs off on every lost packet; "bbr" paces by the speed it
        measures and keeps it on a lossy link such as cellular. One choice
        for every connection. Null leaves it to the web UI (cubic until set
        there). Applies after a restart.
      '';
    };

    uplink = {
      budgetMbps = lib.mkOption {
        type = lib.types.nullOr lib.types.ints.unsigned;
        default = null;
        example = 20;
        description = ''
          What playbacks outside the home may take of its upload
          together, in Mbit/s (`network.uplink.budget_mbps`, PX-46); 0 is
          no limit. Null leaves it to Settings › Remote access, which can
          measure the upload (no limit until set there).
        '';
      };
      maxStreams = lib.mkOption {
        type = lib.types.nullOr lib.types.ints.unsigned;
        default = null;
        description = ''
          The most playbacks outside the home at once
          (`network.uplink.max_streams`); 0 is no limit. Null leaves it
          to the web UI.
        '';
      };
      ownerSharePercent = lib.mkOption {
        type = lib.types.nullOr (lib.types.ints.between 0 100);
        default = null;
        description = ''
          The admins' part of the budget while one of them plays outside
          the home beside guests (`network.uplink.owner_share_percent`).
          Null leaves it to the web UI (50 until set there).
        '';
      };
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

    wan = lib.mkOption {
      default = null;
      example = lib.literalExpression ''{ static = ["203.0.113.7:7420"]; mappedPort = null; }'';
      description = ''
        The server's public addresses, the ones a device off the LAN dials
        (`network.wan`). Null, the default: none is advertised, and devices
        reach the server on the LAN. Set, the port mapping is on with its
        defaults (ask the router, look the public IP up) unless
        {option}`wan.mappedPort` is null. Each address is listed on the
        status page with its source. Devices reach the server at
        {option}`port`, UDP: open it on the interface the router forwards to
        ({option}`openFirewall` opens it everywhere). The router's UPnP
        answers to the server's search arrive from a multicast send the
        firewall cannot match; NAT-PMP and PCP get through.
      '';
      type = lib.types.nullOr (lib.types.submodule {
        options = {
          static = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [];
            example = ["203.0.113.7:7420" "[2001:db8::1]:7420"];
            description = "Public addresses with their port, advertised as written. The forward is yours to set up.";
          };
          mappedPort = lib.mkOption {
            default = {};
            description = "One address made of the port forwarded on the router and the public IP; null for none.";
            type = lib.types.nullOr (lib.types.submodule {
              options = {
                port = lib.mkOption {
                  type = lib.types.nullOr lib.types.port;
                  default = null;
                  description = "The public port forwarded in manual mode; {option}`port` if null.";
                };
                mode = lib.mkOption {
                  type = lib.types.enum ["auto" "manual"];
                  default = "auto";
                  description = ''
                    "auto": the server asks the router (UPnP, NAT-PMP or PCP) to forward
                    {option}`port` and renews it. "manual": you forwarded it.
                  '';
                };
                publicIp = lib.mkOption {
                  type = lib.types.str;
                  default = "lookup";
                  example = "203.0.113.7";
                  description = ''
                    This network's public IP address, or "lookup": asked of
                    {option}`wan.mappedPort.lookupVia` once at start.
                  '';
                };
                lookupVia = lib.mkOption {
                  type = lib.types.nullOr (lib.types.enum ["wtfismyip"]);
                  default = "wtfismyip";
                  description = "Where \"lookup\" asks (https://ipv4.wtfismyip.com/text); null never asks.";
                };
              };
            });
          };
        };
      });
    };

    httpPlayback = {
      listen = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "[::]:7422";
        description = ''
          Where the HTTP playback port listens (`http_playback.listen`,
          PX-49): an IP address and a TCP port. `[::]` is every address,
          IPv6 and IPv4: an app that reached the server over IPv6 gets a
          URL that names IPv6. It serves a playback's HLS,
          or its file, over plain HTTP to a player with no Iroh (an Apple
          TV that a phone AirPlays to), under a token a signed-in app asks
          for; nothing without one. Experimental: apps use it only when
          they and the server both name it. Null, the default: off, and
          nothing listens. Plain HTTP with no sign-in of its own: open it
          to the LAN only ({option}`httpPlayback.interfaces`).
        '';
      };

      address = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "media.home.arpa";
        description = ''
          The host a URL names for an app that reached the server through a
          relay or over the Internet (`http_playback.address`): an IP
          address or a name, no port. Null: such an app gets no URL. An app
          on the LAN gets the address it reached the server at.
        '';
      };

      openFirewall = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Open {option}`httpPlayback.listen`'s TCP port on every interface.
          Leave it off and use {option}`httpPlayback.interfaces` when the
          host has more networks than the LAN.
        '';
      };

      interfaces = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [];
        example = ["eno1"];
        description = "The interfaces {option}`httpPlayback.listen`'s TCP port is opened on.";
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
          `spoolframe` CLI's) listen: an IP address and a port, never a
          name (`localhost`). Loopback by default (PX-11): reach it
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
          does not evaluate otherwise, and the server does not start. On
          every interface with {option}`admin.publicUrl` set, there may be
          one, and both warn without either.
        '';
      };

      notProxied = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Nothing proxies to {option}`admin.listen`: its loopback callers
          are on this host, or come through an SSH tunnel, and share one
          limit on failed logins. Only needed with {option}`admin.publicUrl`
          set: a loopback listener otherwise means a proxy on this host that
          {option}`admin.trustedProxies` must name (PX-11 finding 38), and
          one on every interface is warned of (finding 58).
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
      gpus = lib.mkOption {
        type = lib.types.nullOr (lib.types.listOf (lib.types.submodule {
          options = {
            api = lib.mkOption {
              type = lib.types.enum ["nvenc" "vaapi" "qsv"];
              description = "The API: NVIDIA's NVENC, VA-API (AMD, Intel), or Intel's Quick Sync.";
            };
            pick = lib.mkOption {
              type = lib.types.nullOr (lib.types.enum ["all" "best" "dgpu" "igpu"]);
              default = null;
              description = ''
                Which of that API's cards: `all` (every one, the most
                capable first, a job going to the next when one is full),
                `best` (the most capable alone), `dgpu` (discrete only) or
                `igpu` (integrated only). Neither this nor `device`: `all`.
              '';
            };
            device = lib.mkOption {
              type = lib.types.nullOr (lib.types.either lib.types.str lib.types.ints.unsigned);
              default = null;
              example = "/dev/dri/by-path/pci-0000:c8:00.0-render";
              description = ''
                Exactly one card, in place of `pick`: a CUDA index for
                NVIDIA (0, 1, …, as nvidia-smi numbers them), a render node
                for VA-API and QSV; name it by PCI path, as the renderD
                numbers follow probe order.
              '';
            };
            maxJobs = lib.mkOption {
              type = lib.types.nullOr lib.types.ints.positive;
              default = null;
              description = "GPU video encodes at once on each of the entry's cards; transcode.max_gpu_jobs (Settings › Playback) if null.";
            };
          };
        }));
        default = null;
        example = [
          {
            api = "nvenc";
            pick = "all";
          }
          {
            api = "vaapi";
            pick = "dgpu";
          }
          {
            api = "vaapi";
            device = "/dev/dri/by-path/pci-0000:c8:00.0-render";
          }
        ];
        description = ''
          The GPUs encodes run on (PGPU-05), in order of preference. The
          server lists the host's cards at start, test-encodes on each
          card an entry takes (and again on `systemctl reload`), and puts each video
          encode on the card that does the most of it (encodes the
          output, decodes the source, runs its tone map, burn-in and
          scale), the entries' order then each card's rank deciding
          between equals, and the next card when one runs its most jobs.
          `spoolframe status` and the log list what each card does.
          `null`: every card of every API, NVIDIA first; `[]`: software
          only. A `pick` entry for VA-API or QSV lets the service open
          every DRM node (DeviceAllow `char-drm`), as the cards are only
          known at start; a `device` entry, that node alone.
        '';
      };

      hardware = lib.mkOption {
        type = lib.types.nullOr (lib.types.either (lib.types.enum ["auto" "none"]) (lib.types.nonEmptyListOf (lib.types.enum ["vaapi" "nvenc" "qsv"])));
        default = null;
        example = ["nvenc" "vaapi"];
        description = ''
          Deprecated, to be removed in a later release: use
          {option}`services.spoolframe.transcode.gpus`. The server no
          longer reads it (0.12.0); this module writes the gpus it means:
          each family named every card of it, in that order (`auto` every
          API, `none` none), VA-API and QSV on
          {option}`services.spoolframe.transcode.vaapiDevice` alone when
          that is set. Not with gpus.
        '';
      };

      vaapiDevice = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "/dev/dri/by-path/pci-0000:c8:00.0-render";
        description = ''
          Deprecated, to be removed in a later release: use a
          {option}`services.spoolframe.transcode.gpus` entry with
          `device`. The one render node VA-API encodes on, written as
          the gpus it means. Not with gpus.
        '';
      };

      toneMap = lib.mkOption {
        type = lib.types.nullOr (lib.types.enum ["auto" "cpu" "gpu" "libplacebo"]);
        default = null;
        example = "libplacebo";
        description = ''
          How HDR is tone mapped to SDR for a screen that shows no HDR
          (PGPU-04). `auto` uses the job's GPU's own tone map
          (`tonemap_vaapi`, Intel), else libplacebo on the job's GPU
          through Vulkan (AMD, NVIDIA), else the CPU; and the CPU while
          `transcode.gpu_filters` is off. `cpu`, `gpu` (the driver's own)
          or `libplacebo` forces one; a job whose GPU cannot run it is tone
          mapped on the CPU. libplacebo needs the GPU's Vulkan driver:
          {option}`hardware.graphics.enable` (RADV for AMD; NVIDIA's own).
          Null leaves it to Settings › Playback in the web UI (`auto`
          until set there).
        '';
      };

      videoCodecs = lib.mkOption {
        type = lib.types.nullOr (lib.types.listOf (lib.types.enum ["h264" "hevc" "av1"]));
        default = null;
        example = ["hevc" "h264"];
        description = ''
          The codecs a GPU encodes to, the first preferred (PGPU-06): a
          conversion takes the first the client plays (decodes in
          hardware, where it says) and a GPU here encodes with room, and an
          HDR film for a screen that shows HDR keeps it in HEVC or AV1 at
          10 bits rather than being tone mapped. With none left, the
          conversion is encoded in software, which is H.264 only. Each at
          most once; `[]` keeps video off the GPUs. Null leaves it to
          Settings › Playback in the web UI (`["av1" "hevc" "h264"]` until
          set there).
        '';
      };
    };

    experimental = {
      libplaceboSharedOnAmd = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Experimental: may change or go away in any release, without
          notice. On an AMD GPU, libplacebo's tone map uses ffmpeg's
          Vulkan device, so the frames stay on the GPU, rather than one of
          its own, which takes them through system memory. Tried at start,
          and its own used where that fails, as it does with Mesa's RADV
          today. NVIDIA always shares.
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
        assertion = cfg.admin.listen == null || parsed cfg.admin.listen;
        message = "services.spoolframe.admin.listen (${cfg.admin.listen}) must be an IP address and a port, such as 127.0.0.1:7421 or [::1]:7421: the server does not look up names.";
      }
      # The server refuses this shape at load too (P6-02).
      {
        assertion = cfg.wan == null || cfg.wan.mappedPort == null || !(cfg.wan.mappedPort.publicIp == "lookup" && cfg.wan.mappedPort.lookupVia == null);
        message = ''services.spoolframe.wan.mappedPort.publicIp is "lookup", but wan.mappedPort.lookupVia = null forbids looking the address up: set publicIp to this network's public address, or lookupVia to "wtfismyip".'';
      }
      # Which would win is a guess (PGPU-05).
      {
        assertion = cfg.transcode.gpus == null || !oldSet;
        message = "services.spoolframe.transcode.gpus is set, and so is the deprecated transcode.hardware or vaapiDevice: gpus says everything they did, so set gpus alone.";
      }
      {
        assertion = lib.all (e: e.pick == null || e.device == null) gpus;
        message = "services.spoolframe.transcode.gpus: an entry has pick or device, not both: pick chooses among the cards found, device names one.";
      }
      # PX-49: the server parses it as admin.listen.
      {
        assertion = cfg.httpPlayback.listen == null || parsed cfg.httpPlayback.listen;
        message = "services.spoolframe.httpPlayback.listen (${cfg.httpPlayback.listen}) must be an IP address and a port, such as [::]:7422 or 0.0.0.0:7422: the server does not look up names.";
      }
      {
        assertion = cfg.httpPlayback.listen != null || !(cfg.httpPlayback.openFirewall || cfg.httpPlayback.interfaces != []);
        message = "services.spoolframe.httpPlayback.openFirewall or interfaces is set, but httpPlayback.listen is null: there is no port to open. Set httpPlayback.listen.";
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

    warnings =
      # PGPU-05: written as the gpus they mean.
      lib.optional oldSet ''
        services.spoolframe.transcode.hardware and vaapiDevice are deprecated and will be removed:
        use services.spoolframe.transcode.gpus = [ ${lib.concatMapStringsSep " " gpuNix oldGpus} ];''
      # P6-07: relays of your own that no device is told about.
      ++ lib.optional (cfg.ownRelays != [] && cfg.relays == []) ''
        services.spoolframe.ownRelays names relays of your own, but services.spoolframe.relays is
        empty: the server answers them, yet tells no device about a relay. Add each relay's URL
        to relays.''
      # P6-02: the public addresses reach the server only through its port.
      ++ lib.optional (cfg.wan != null && !cfg.openFirewall) ''
        services.spoolframe.wan is set but openFirewall is off: devices off the LAN reach the server at
        UDP ${toString cfg.port}, so open it on the interface the router forwards to
        (networking.firewall.interfaces.<name>.allowedUDPPorts).''
      # Finding 58 (the third review's G-9): on every interface, a proxy on
      # this host is as likely as browsers coming straight in, so the server
      # warns at start rather than refuse, and so does the module.
      ++ lib.optional (everywhere cfg.admin.listen && cfg.admin.publicUrl != null && cfg.admin.trustedProxies == [] && !cfg.admin.notProxied) ''
        services.spoolframe.admin.trustedProxies is empty, but admin.publicUrl is set and admin.listen is
        on every interface: if a proxy on this host is in front, every caller through it looks like
        loopback, so failed logins are not limited per caller, and /metrics, when on, answers whoever
        the proxy lets in. Put admin.listen on loopback and set admin.trustedProxies = ["127.0.0.1"];
        or, if nothing proxies to the listener, set admin.notProxied = true.'';

    environment.etc."spoolframe/server.ucl".source = configFile;
    environment.systemPackages = [cli];

    # Answers to the wizard's search for Plex servers are unicast replies
    # to a multicast send, which the firewall cannot track: their port is
    # opened as the others are, everywhere or per interface (PX-11
    # finding 36).
    networking.firewall.allowedUDPPorts = lib.mkIf cfg.openFirewall (
      [cfg.port] ++ lib.optional cfg.mdns 5353 ++ lib.optional cfg.plexDiscovery.enable cfg.plexDiscovery.port
    );
    networking.firewall.interfaces = lib.mkMerge [
      (lib.mkIf (cfg.plexDiscovery.enable && !cfg.openFirewall) (
        lib.genAttrs cfg.plexDiscovery.interfaces (_: {allowedUDPPorts = [cfg.plexDiscovery.port];})
      ))
      # PX-49: plain HTTP, opened where it is asked to be and nowhere else.
      (lib.mkIf (httpPort != null) (
        lib.genAttrs cfg.httpPlayback.interfaces (_: {allowedTCPPorts = [httpPort];})
      ))
    ];
    networking.firewall.allowedTCPPorts = lib.mkIf (httpPort != null && cfg.httpPlayback.openFirewall) [httpPort];

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
      # A new config is a restart: it is a new file in the store, and the
      # running server reads again (SIGHUP, PX-33) only the one it was
      # started with.
      restartTriggers = [configFile];
      serviceConfig = {
        # The state directory quoted (G-13), as the CLI wrapper's is:
        # systemd splits on whitespace, and expands % and $.
        ExecStart = "${cfg.package}/bin/spoolframe-server --config ${configFile} --state-dir ${utils.escapeSystemdExecArg cfg.stateDir}";
        # `systemctl reload spoolframe` (PX-33): the server reads its
        # config file and runtime settings again, applies the live
        # settings, and detects the encoders again.
        ExecReload = "${pkgs.coreutils}/bin/kill -HUP $MAINPID";
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
        # back: with SPOOLFRAME_SUPERVISED it exits with 75 when asked to
        # restart, for systemd to start it again. Without it, the server
        # would run itself again in place, under a unit that no longer
        # knows what it runs.
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
        # No other devices; the GPUs' only when hardware encoding may be
        # used: a VA-API or QSV entry's render node, or every DRM node for
        # an entry that picks among the cards (they are listed at start);
        # NVIDIA's character devices for an NVIDIA entry. Each run is then
        # confined to its own card's nodes by the server (PGPU-05).
        PrivateDevices = !gpu;
        DevicePolicy = "closed";
        DeviceAllow =
          lib.optionals vaapiPicks ["char-drm rw"]
          ++ map (node: "${node} rw") vaapiDevices
          # NVIDIA's major 195 (nvidia0, nvidiactl, nvidia-modeset) is
          # `nvidia` in /proc/devices on current drivers (580 on NixOS) and
          # `nvidia-frontend` on older ones; systemd matches the name, so
          # both are allowed, and one the driver lacks matches nothing.
          ++ lib.optionals nvenc ["char-nvidia rw" "char-nvidia-frontend rw" "char-nvidia-uvm rw" "char-nvidia-caps rw"];
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
        # The server loads a deny-list of its own as it starts (PX-31),
        # which stacks on this one: it also refuses the keyrings,
        # io_uring, userfaultfd, process_vm_* and kcmp, which
        # @system-service allows. It also makes itself not dumpable and
        # sets no new privileges and a core limit of 0 itself.
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
