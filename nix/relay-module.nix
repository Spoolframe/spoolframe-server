# A relay of your own (P6-04): `spoolframe-server relay`, for devices that
# cannot reach the server directly, in place of n0's public relays. Its
# key (`spoolframe-server relay id`, run as the service's user with its
# state directory) goes in the main server's `ownRelays`, and its URL in
# the main server's `relays`.
{
  config,
  lib,
  pkgs,
  utils,
  ...
}: let
  cfg = config.services.spoolframe-relay;
  quote = builtins.toJSON;
  credential = name: "/run/credentials/spoolframe-relay.service/${name}";

  tls =
    if cfg.tls.mode == "files"
    then ''
      tls {
        mode = "files";
        https = ${quote cfg.tls.https};
        cert_file = ${quote (credential "cert")};
        key_file = ${quote (credential "key")};
      }
    ''
    else if cfg.tls.mode == "acme"
    then ''
      tls {
        mode = "acme";
        https = ${quote cfg.tls.https};
        hostname = ${quote cfg.tls.hostname};
        contact = ${quote cfg.tls.contact};
        directory = ${quote cfg.tls.directory};
      }
    ''
    else ''tls { mode = "proxy"; }'';

  configFile = pkgs.writeText "spoolframe-relay.ucl" ''
    schema = "spoolframe.relay.v1";
    server {
      key = ${quote cfg.server.key};
      addrs = [${lib.concatMapStringsSep ", " quote cfg.server.addrs}];
    }
    access {
      owner = ${quote cfg.access.owner};
      friends = ${quote cfg.access.friends};
      everyone = ${quote cfg.access.everyone};
    }
    ${tls}
    http = ${quote cfg.http};
    ${lib.optionalString (cfg.quic != null) "quic = ${quote cfg.quic};"}
  '';

  # A port under 1024 needs the capability to bind it.
  port = listen: lib.toInt (lib.last (lib.splitString ":" listen));
  privileged =
    port cfg.http
    < 1024
    || (cfg.tls.mode != "proxy" && port cfg.tls.https < 1024);
  tier = lib.types.enum ["allow" "deny"];
in {
  options.services.spoolframe-relay = {
    enable = lib.mkEnableOption "a Spoolframe relay of your own";

    package = lib.mkOption {
      type = lib.types.package;
      default = config.services.spoolframe.package or (throw "services.spoolframe-relay.package must be set");
      defaultText = lib.literalExpression "config.services.spoolframe.package";
      description = "The spoolframe-server package; the relay is a mode of it.";
    };

    stateDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/spoolframe-relay";
      description = "Where the relay keeps its key and an ACME certificate.";
    };

    server = {
      key = lib.mkOption {
        type = lib.types.str;
        description = "The main server's key (`spoolframe-server id`'s endpoint id).";
      };
      addrs = lib.mkOption {
        type = lib.types.nonEmptyListOf lib.types.str;
        example = ["10.10.0.6:7420"];
        description = "Where the relay dials the main server, as `ip:port`: at least one.";
      };
    };

    access = {
      owner = lib.mkOption {
        type = tier;
        default = "allow";
        description = "The server and its users' devices.";
      };
      friends = lib.mkOption {
        type = tier;
        default = "allow";
        description = "Paired servers, their devices, granted portable users (none until federation).";
      };
      everyone = lib.mkOption {
        type = tier;
        default = "deny";
        description = "Anyone else: your bandwidth for anyone's traffic.";
      };
    };

    tls = {
      mode = lib.mkOption {
        type = lib.types.enum ["files" "acme" "proxy"];
        default = "proxy";
        description = ''
          `files`: your certificate and key. `acme`: a certificate got with
          TLS-ALPN-01 on `https` (port 443 as the Internet sees it).
          `proxy`: TLS ends at a proxy in front (nginx), which forwards
          `/relay` with the WebSocket upgrade to `http`.
        '';
      };
      https = lib.mkOption {
        type = lib.types.str;
        default = "0.0.0.0:443";
        description = "Where HTTPS is served (not under `proxy`).";
      };
      certFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "The certificate chain, PEM (`files`); passed as a credential. Read once, at start: restart the relay when it is renewed (a certbot or ACME client deploy hook).";
      };
      keyFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "The certificate's key, PEM (`files`); passed as a credential.";
      };
      hostname = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "The relay's public name (`acme`).";
      };
      contact = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "The ACME account's email (`acme`).";
      };
      directory = lib.mkOption {
        type = lib.types.str;
        default = "letsencrypt";
        example = "letsencrypt-staging";
        description = "`letsencrypt`, `letsencrypt-staging`, or an ACME directory URL (`acme`).";
      };
    };

    http = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0:80";
      example = "127.0.0.1:3340";
      description = "Plain HTTP: the captive-portal probe, and the relay itself under `proxy`.";
    };

    quic = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "0.0.0.0:7842";
      description = "QUIC address discovery, if wanted (not under `proxy`).";
    };

    # The relay faces the Internet: bound it as the server is (finding 34).
    resources = {
      memoryMax = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = "1G";
        description = "systemd's MemoryMax: past it the kernel kills in the service. Null leaves it unset.";
      };
      tasksMax = lib.mkOption {
        type = lib.types.nullOr lib.types.ints.positive;
        default = 512;
        description = "systemd's TasksMax: the relay's threads. Null leaves it unset.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.tls.mode != "files" || (cfg.tls.certFile != null && cfg.tls.keyFile != null);
        message = "services.spoolframe-relay: tls.mode = \"files\" needs tls.certFile and tls.keyFile.";
      }
      {
        assertion = cfg.tls.mode != "acme" || (cfg.tls.hostname != null && cfg.tls.contact != null);
        message = "services.spoolframe-relay: tls.mode = \"acme\" needs tls.hostname and tls.contact.";
      }
      {
        assertion = cfg.tls.mode != "proxy" || cfg.quic == null;
        message = "services.spoolframe-relay: quic needs the relay's own TLS, not tls.mode = \"proxy\".";
      }
    ];

    # P6-07: shapes that work but are likely not meant; the relay warns
    # of the same at start.
    warnings =
      lib.optional (cfg.tls.mode == "proxy" && !(lib.hasPrefix "127." cfg.http || lib.hasPrefix "[::1]" cfg.http))
      "services.spoolframe-relay: tls.mode = \"proxy\" serves the relay in plain HTTP on http = ${cfg.http}, not loopback: it is reachable around the proxy meant to hold its TLS. Set http to 127.0.0.1:<port> unless the proxy is on another host."
      ++ lib.optional (cfg.access.everyone == "allow")
      "services.spoolframe-relay: access.everyone = \"allow\" lets anyone relay through it, on your bandwidth, without asking the main server.";

    systemd.services.spoolframe-relay = {
      description = "Spoolframe relay";
      wantedBy = ["multi-user.target"];
      after = ["network-online.target"];
      wants = ["network-online.target"];
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/spoolframe-server relay run --config ${configFile} --state-dir ${utils.escapeSystemdExecArg cfg.stateDir}";
        DynamicUser = true;
        StateDirectory = lib.removePrefix "/var/lib/" cfg.stateDir;
        StateDirectoryMode = "0700";
        LoadCredential = lib.optionals (cfg.tls.mode == "files") [
          "cert:${cfg.tls.certFile}"
          "key:${cfg.tls.keyFile}"
        ];
        Restart = "on-failure";
        RestartSec = 2;
        UMask = "0077";
        AmbientCapabilities = lib.optional privileged "CAP_NET_BIND_SERVICE";
        CapabilityBoundingSet = lib.optional privileged "CAP_NET_BIND_SERVICE";
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        DevicePolicy = "closed";
        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        RemoveIPC = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        # Netlink: its endpoint watches the host's interfaces.
        RestrictAddressFamilies = ["AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK"];
        SystemCallArchitectures = "native";
        SystemCallFilter = ["@system-service" "~@privileged @resources"];
        SystemCallErrorNumber = "EPERM";
        MemoryMax = lib.mkIf (cfg.resources.memoryMax != null) cfg.resources.memoryMax;
        TasksMax = lib.mkIf (cfg.resources.tasksMax != null) cfg.resources.tasksMax;
      };
    };
  };
}
