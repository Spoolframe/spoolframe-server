# The module's own assertions, evaluated without building a system or a VM.
# A proxy on this host that the server is not told about (a loopback
# listener with a public URL and no trusted proxies) is a shape the server
# refuses to start in (PX-11 finding 38, PX-09 finding 29), so it must not
# evaluate either; naming the proxy, or saying nothing proxies to the
# listener, lets it through. On every interface with a public URL there
# may be such a proxy (finding 58): warned of, not refused.
{
  pkgs,
  spoolframeModule,
}: let
  inherit (pkgs) lib;
  # The module evaluated with `settings` set.
  evaluatedWith = settings:
    import (pkgs.path + "/nixos/lib/eval-config.nix") {
      inherit (pkgs.stdenv.hostPlatform) system;
      modules = [
        spoolframeModule
        {
          nixpkgs.pkgs = pkgs;
          system.stateVersion = lib.trivial.release;
          services.spoolframe =
            {
              enable = true;
              # Never built: only the assertions are evaluated.
              package = pkgs.emptyDirectory;
            }
            // settings;
        }
      ];
    };
  # The module evaluated with `admin` set.
  evaluated = admin: evaluatedWith {inherit admin;};
  # The service's command line with `stateDir` set.
  execStart = stateDir: (evaluatedWith {inherit stateDir;}).config.systemd.services.spoolframe.serviceConfig.ExecStart;
  # The module's failed assertions with `admin` set, as their messages.
  failed = admin: map (a: a.message) (lib.filter (a: !a.assertion && lib.hasPrefix "services.spoolframe" a.message) (evaluated admin).config.assertions);
  # Its warnings, the same way.
  warned = admin: lib.any (w: lib.hasPrefix "services.spoolframe" w && lib.hasInfix "admin.trustedProxies" w && lib.hasInfix "admin.notProxied" w) (evaluated admin).config.warnings;
  refused = admin: lib.any (lib.hasInfix "admin.trustedProxies") (failed admin);
  # Refused for a listen address the server cannot parse.
  unparsed = admin: lib.any (lib.hasInfix "admin.listen (") (failed admin);
  accepted = admin: failed admin == [];
  check = what: ok: lib.assertMsg ok "spoolframe module assertions: ${what}";
  # P6-02: the module's failed assertions and warnings with `wan` set.
  wanFailed = wan: map (a: a.message) (lib.filter (a: !a.assertion && lib.hasPrefix "services.spoolframe" a.message) (evaluatedWith {inherit wan;}).config.assertions);
  wanRefused = wan: lib.any (lib.hasInfix "wan.mappedPort.publicIp") (wanFailed wan);
  wanWarned = settings: lib.any (w: lib.hasInfix "services.spoolframe.wan is set but openFirewall is off" w) (evaluatedWith settings).config.warnings;
  # The UCL the module writes for the server.
  rendered = settings: (evaluatedWith settings).config.environment.etc."spoolframe/server.ucl".source.text;
  publicUrl = "https://media.example.org";
in
  assert check "an unnamed proxy in front of a loopback listener is refused" (refused {inherit publicUrl;});
  assert check "an unnamed proxy in front of [::1] is refused" (refused {
    inherit publicUrl;
    listen = "[::1]:7421";
  });
  # G-12: the server's loopback check is on the parsed address, so every
  # spelling of ::1 is loopback, and a name or a bracketed IPv4 address is
  # no address at all.
  assert check "an unnamed proxy in front of ::1 spelt out is refused" (refused {
    inherit publicUrl;
    listen = "[0:0:0:0:0:0:0:1]:7421";
  });
  assert check "a listen address that is a name is refused" (unparsed {
    listen = "localhost:7421";
    trustedProxies = ["127.0.0.1"];
  });
  assert check "a bracketed IPv4 listen address is refused" (unparsed {
    listen = "[127.0.0.1]:7421";
    trustedProxies = ["127.0.0.1"];
  });
  assert check "an IPv4 listen address is not refused as unparsed" (!unparsed {
    listen = "192.168.1.20:7421";
  });
  assert check "an IPv6 listen address is not refused as unparsed" (!unparsed {
    listen = "[fd00::20]:7421";
  });
  assert check "a named proxy is accepted" (accepted {
    inherit publicUrl;
    trustedProxies = ["127.0.0.1"];
  });
  assert check "a listener said not to be proxied is accepted" (accepted {
    inherit publicUrl;
    notProxied = true;
  });
  # Finding 58: on every interface there may be a proxy on this host, or
  # browsers may come straight in (the dev server's shape): warned of,
  # not refused.
  assert check "a listener on every interface is not refused" (accepted {
    inherit publicUrl;
    listen = "0.0.0.0:7421";
  });
  assert check "a listener on every interface with a public URL is warned of" (warned {
    inherit publicUrl;
    listen = "0.0.0.0:7421";
  });
  assert check "the server's own default listener is warned of" (warned {
    inherit publicUrl;
    listen = null;
  });
  assert check "a listener on every interface over [::] is warned of" (warned {
    inherit publicUrl;
    listen = "[::]:7421";
  });
  assert check "a listener on every interface over [::] spelt out is warned of" (warned {
    inherit publicUrl;
    listen = "[0:0:0:0:0:0:0:0]:7421";
  });
  assert check "a listener on every interface with its proxy named is not warned of" (!warned {
    inherit publicUrl;
    listen = "0.0.0.0:7421";
    trustedProxies = ["127.0.0.1"];
  });
  assert check "a listener on every interface said not to be proxied is not warned of" (!warned {
    inherit publicUrl;
    listen = "0.0.0.0:7421";
    notProxied = true;
  });
  assert check "a listener on every interface without a public URL is not warned of" (!warned {
    listen = "0.0.0.0:7421";
  });
  assert check "the defaults are accepted" (accepted {});
  assert check "a lookup with lookups forbidden is refused" (wanRefused {mappedPort.lookupVia = null;});
  assert check "a written public IP without lookups is accepted" (!wanRefused {
    mappedPort = {
      publicIp = "203.0.113.7";
      lookupVia = null;
    };
  });
  assert check "the defaults with wan set are accepted" (wanFailed {} == []);
  assert check "wan without openFirewall is warned of" (wanWarned {wan = {};});
  assert check "wan with openFirewall is not warned of" (!wanWarned {
    wan = {};
    openFirewall = true;
  });
  assert check "without wan the config has no wan table" (!lib.hasInfix "wan {" (rendered {}));
  assert check "wan's defaults are written out" (lib.hasInfix ''mode = "auto";'' (rendered {wan = {};}) && lib.hasInfix ''lookup_via = "wtfismyip";'' (rendered {wan = {};}));
  assert check "a static-only wan turns the mapped port off" (lib.hasInfix "mapped_port = false;" (rendered {
      wan = {
        static = ["203.0.113.7:7420"];
        mappedPort = null;
      };
    })
    && lib.hasInfix ''static = ["203.0.113.7:7420"];'' (rendered {
      wan = {
        static = ["203.0.113.7:7420"];
        mappedPort = null;
      };
    }));
  assert check "lookups forbidden are written as false" (lib.hasInfix "lookup_via = false;" (rendered {
    wan.mappedPort = {
      mode = "manual";
      port = 443;
      publicIp = "203.0.113.7";
      lookupVia = null;
    };
  }));
  # G-13: systemd splits ExecStart on whitespace, so the state directory
  # is one quoted argument.
  assert check "the state directory is one argument" (lib.hasInfix ''--state-dir "/var/lib/spool frame"'' (execStart "/var/lib/spool frame"));
  assert check "the defaults are not warned of" (!warned {});
    pkgs.runCommand "spoolframe-module-assertions" {} "touch $out"
