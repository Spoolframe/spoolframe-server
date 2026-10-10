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
  relayModule,
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
  # PGPU-05: the service's DeviceAllow, failed assertions and warnings
  # with `transcode` set.
  deviceAllow = transcode: (evaluatedWith {inherit transcode;}).config.systemd.services.spoolframe.serviceConfig.DeviceAllow;
  gpuFailed = transcode: lib.any (lib.hasPrefix "services.spoolframe.transcode.gpus") (map (a: a.message) (lib.filter (a: !a.assertion) (evaluatedWith {inherit transcode;}).config.assertions));
  gpuWarned = transcode: lib.findFirst (lib.hasPrefix "services.spoolframe.transcode.hardware") null (evaluatedWith {inherit transcode;}).config.warnings;
  publicUrl = "https://media.example.org";
  # The relay module evaluated with `settings` set (P6-04, P6-07).
  relayWith = settings:
    (import (pkgs.path + "/nixos/lib/eval-config.nix") {
      inherit (pkgs.stdenv.hostPlatform) system;
      modules = [
        relayModule
        {
          nixpkgs.pkgs = pkgs;
          system.stateVersion = lib.trivial.release;
          services.spoolframe-relay =
            {
              enable = true;
              package = pkgs.emptyDirectory;
              server.key = "0000000000000000000000000000000000000000000000000000000000000000";
            }
            // settings;
        }
      ];
    }).config;
  # Whether the relay module warns, naming `what`, with `settings`.
  relayWarned = what: settings: lib.any (w: lib.hasPrefix "services.spoolframe-relay" w && lib.hasInfix what w) (relayWith settings).warnings;
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
  assert check "IPv6 is left at the server's default unless turned off" (!lib.hasInfix "ipv6 =" (rendered {}));
  assert check "IPv6 turned off is written as false" (lib.hasInfix "ipv6 = false;" (rendered {ipv6 = false;}));
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
  # PGPU-05: transcode.gpus, and the deprecated hardware and vaapiDevice.
  assert check "by default no GPU setting is written" (!lib.hasInfix "hardware =" (rendered {}) && !lib.hasInfix "vaapi_device =" (rendered {}) && !lib.hasInfix "gpus =" (rendered {}));
  assert check "by default every DRM node and NVIDIA's are allowed" (deviceAllow {} == ["char-drm rw" "char-nvidia-frontend rw" "char-nvidia-uvm rw" "char-nvidia-caps rw"]);
  assert check "gpus are written as the server takes them" (lib.hasInfix ''gpus = [{"api":"vaapi","pick":"dgpu"},{"api":"nvenc","device":"1","max_jobs":2}];'' (rendered {
    transcode.gpus = [
      {
        api = "vaapi";
        pick = "dgpu";
      }
      {
        api = "nvenc";
        device = 1;
        maxJobs = 2;
      }
    ];
  }));
  assert check "a device entry allows its node alone" (deviceAllow {
      gpus = [
        {
          api = "vaapi";
          device = "/dev/dri/renderD129";
        }
      ];
    }
    == ["/dev/dri/renderD129 rw"]);
  assert check "no GPU allows no device" (deviceAllow {gpus = [];} == []);
  assert check "the old setting is warned of with the gpus it means" (gpuWarned {
      hardware = ["nvenc" "vaapi"];
      vaapiDevice = "/dev/dri/renderD128";
    }
    == ''
      services.spoolframe.transcode.hardware and vaapiDevice are deprecated and will be removed:
      use services.spoolframe.transcode.gpus = [ { api = "nvenc"; pick = "all"; } { api = "vaapi"; device = "/dev/dri/renderD128"; } ];'');
  assert check "the old setting keeps its one render node" (deviceAllow {
      hardware = ["vaapi"];
      vaapiDevice = "/dev/dri/renderD128";
    }
    == ["/dev/dri/renderD128 rw"]);
  assert check "gpus alone is not warned of" (gpuWarned {gpus = [{api = "vaapi";}];} == null);
  assert check "gpus with the old setting is refused" (gpuFailed {
    gpus = [{api = "vaapi";}];
    hardware = "auto";
  });
  assert check "an entry with pick and device is refused" (gpuFailed {
    gpus = [
      {
        api = "vaapi";
        pick = "all";
        device = "/dev/dri/renderD128";
      }
    ];
  });
  # The comma-separated settings are lists in the config the module writes.
  assert check "relays are written as a list" (lib.hasInfix ''relays = ["https://relay.example.org","default"];'' (rendered {relays = ["https://relay.example.org" "default"];}));
  assert check "trusted proxies are written as a list" (lib.hasInfix ''trusted_proxies = ["127.0.0.1"];'' (rendered {
    admin = {
      listen = "127.0.0.1:7421";
      inherit publicUrl;
      trustedProxies = ["127.0.0.1"];
    };
  }));
  assert check "relays of your own that no device is told about are warned of" (lib.any (lib.hasInfix "services.spoolframe.ownRelays") (evaluatedWith {ownRelays = ["0000000000000000000000000000000000000000000000000000000000000001"];}).config.warnings);
  # P6-07: the relay's shapes that work but are likely not meant.
  assert check "a proxied relay on every interface is warned of" (relayWarned "tls.mode" {});
  assert check "a proxied relay on loopback is not warned of" (!relayWarned "tls.mode" {http = "127.0.0.1:3340";});
  assert check "a relay open to everyone is warned of" (relayWarned "access.everyone" {
    http = "127.0.0.1:3340";
    access.everyone = "allow";
  });
  assert check "the relay's memory is bounded" ((relayWith {}).systemd.services.spoolframe-relay.serviceConfig.MemoryMax == "1G");
    pkgs.runCommand "spoolframe-module-assertions" {} "touch $out"
