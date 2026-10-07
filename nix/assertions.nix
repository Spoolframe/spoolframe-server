# The module's own assertions, evaluated without building a system or a VM.
# A proxy on this host that the server is not told about (a loopback
# listener with a public URL and no trusted proxies) is a shape the server
# refuses to start in (PX-11 finding 38, PX-09 finding 29), so it must not
# evaluate either; naming the proxy, or saying nothing proxies to the
# listener, lets it through.
{
  pkgs,
  spoolframeModule,
}: let
  inherit (pkgs) lib;
  # The module's failed assertions with `admin` set, as their messages.
  failed = admin: let
    system = import (pkgs.path + "/nixos/lib/eval-config.nix") {
      inherit (pkgs.stdenv.hostPlatform) system;
      modules = [
        spoolframeModule
        {
          nixpkgs.pkgs = pkgs;
          system.stateVersion = lib.trivial.release;
          services.spoolframe = {
            enable = true;
            # Never built: only the assertions are evaluated.
            package = pkgs.emptyDirectory;
            inherit admin;
          };
        }
      ];
    };
  in
    map (a: a.message) (lib.filter (a: !a.assertion && lib.hasPrefix "services.spoolframe" a.message) system.config.assertions);
  refused = admin: lib.any (lib.hasInfix "admin.trustedProxies") (failed admin);
  accepted = admin: failed admin == [];
  check = what: ok: lib.assertMsg ok "spoolframe module assertions: ${what}";
  publicUrl = "https://media.example.org";
in
  assert check "an unnamed proxy in front of a loopback listener is refused" (refused {inherit publicUrl;});
  assert check "an unnamed proxy in front of [::1] is refused" (refused {
    inherit publicUrl;
    listen = "[::1]:7421";
  });
  assert check "a named proxy is accepted" (accepted {
    inherit publicUrl;
    trustedProxies = ["127.0.0.1"];
  });
  assert check "a listener said not to be proxied is accepted" (accepted {
    inherit publicUrl;
    notProxied = true;
  });
  assert check "a listener on every interface is not that shape" (accepted {
    inherit publicUrl;
    listen = "0.0.0.0:7421";
  });
  assert check "the defaults are accepted" (accepted {});
    pkgs.runCommand "spoolframe-module-assertions" {} "touch $out"
