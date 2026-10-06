# The NixOS module in VMs. `server`, with P3-04's and P3-10's options set
# (a proxy's public URL and trusted proxies, a sign-in provider, the admin
# listener on loopback): the service starts without a reachable Plex,
# prints a setup code, takes its first admin and users through the CLI,
# listens on its UDP port, finds only software encoders without a GPU,
# reads subtitles with the tesseract languages it was given, and keeps its
# key over a restart. `fresh`, the module with nothing but `enable`: not
# set up, its web UI answering, its state where the module says, and the
# firewall opening only the Plex discovery port.
{
  pkgs,
  spoolframePackage,
  spoolframeModule,
}:
pkgs.testers.runNixOSTest {
  name = "spoolframe-module";

  nodes.server = {
    imports = [spoolframeModule];
    environment.systemPackages = [pkgs.curl];
    # Root-only, as a real secret is: the service reads it as a credential.
    environment.etc."spoolframe-oidc-secret" = {
      text = "oidc-client-secret";
      mode = "0400";
    };
    services.spoolframe = {
      enable = true;
      package = spoolframePackage;
      name = "Test server";
      # Nothing listens here: the server only warns, and serves the rest.
      plex.url = "http://127.0.0.1:32400";
      plex.pathMappings = [
        {
          from = "/data";
          to = "/srv/media";
        }
      ];
      openFirewall = true;
      subtitles.ocrLanguages = ["eng" "fra"];
      # P3-10: behind a proxy on this host.
      admin = {
        listen = "127.0.0.1:7421";
        publicUrl = "https://media.example.org";
        trustedProxies = ["127.0.0.1" "10.0.0.0/8"];
      };
      # P3-04: nothing answers at the issuer; it is only asked when a
      # user signs in with it.
      auth.oidc = [
        {
          name = "Pocket ID";
          issuer = "https://id.example.org";
          clientId = "spoolframe";
          clientSecretFile = "/etc/spoolframe-oidc-secret";
        }
      ];
    };
  };

  nodes.fresh = {
    imports = [spoolframeModule];
    environment.systemPackages = [pkgs.curl];
    services.spoolframe = {
      enable = true;
      package = spoolframePackage;
    };
  };

  testScript = ''
    import re

    start_all()

    def firewall_udp(machine):
        rules = machine.succeed("${pkgs.iptables}/bin/iptables -S nixos-fw")
        return set(int(p) for p in re.findall(r"-p udp .*--dport (\d+) ", rules))

    # Nothing but enable: not set up, as the module leaves Plex to the
    # wizard.
    fresh.wait_for_unit("spoolframe.service")
    fresh.wait_until_succeeds("journalctl -u spoolframe.service | grep -q 'setup code is'")
    fresh.succeed("systemctl is-active spoolframe.service")
    site = fresh.wait_until_succeeds("curl -sf http://127.0.0.1:7421/auth/site")
    assert '"name":"fresh"' in site.replace(" ", ""), site
    shown = fresh.succeed("spoolframe config show")
    assert "(locked)" in shown, shown
    assert "plex.url = (not set);" in shown, shown
    assert "network.plex_discovery_port = 32415;  # config file, locked" in shown, shown
    # Its state where the module says, the service's own.
    fresh.succeed("test -s /var/lib/spoolframe/endpoint.key")
    fresh.succeed("test -s /var/lib/spoolframe/spoolframe.db")
    assert fresh.succeed("stat -c '%U %a' /var/lib/spoolframe").strip() == "spoolframe 700"
    endpoint = fresh.succeed("spoolframe id").strip()
    assert re.search(f"endpoint_id={endpoint}", fresh.succeed("journalctl -u spoolframe.service")), endpoint
    # The firewall lets Plex's answers in, and nothing else of ours.
    opened = firewall_udp(fresh)
    assert 32415 in opened, opened
    assert 7420 not in opened and 5353 not in opened, opened
    # The web UI is not opened to the network.
    server.wait_for_unit("multi-user.target")
    server.fail("curl -sf --max-time 5 http://fresh:7421/auth/site")

    server.wait_for_unit("spoolframe.service")
    server.wait_until_succeeds("ss -ulnp | grep -q ':7420 '")

    def journal():
        return server.succeed("journalctl -u spoolframe.service --no-pager")

    server.wait_until_succeeds("journalctl -u spoolframe.service | grep -q 'setup code is'")
    found = re.search(r"setup code is ([A-Z0-9-]+)", journal())
    assert found is not None
    code = found.group(1)
    key = re.findall(r"endpoint_id=([0-9a-f]{64})", journal())[-1]

    server.succeed(f"printf 'admin-pw\n' | spoolframe setup --code {code} --username admin --password-stdin")
    admin = "SPOOLFRAME_ADMIN_USER=admin SPOOLFRAME_ADMIN_PASSWORD=admin-pw"
    server.succeed(f"printf 'alice-pw\n' | {admin} spoolframe user add alice --password-stdin")
    users = server.succeed(f"{admin} spoolframe user list")
    assert "alice" in users, users

    # No GPU in the VM: detection finds software encoders only.
    server.wait_until_succeeds("journalctl -u spoolframe.service | grep -q 'transcoding'")
    status = server.succeed(f"{admin} spoolframe status")
    assert "libx264" in status, status
    assert "vaapi" not in status and "nvenc" not in status, status

    # tesseract with the languages asked for, and the cache for its text.
    ocr = [line for line in journal().splitlines() if "subtitle OCR available" in line]
    assert ocr, journal()
    assert '"eng"' in ocr[-1] and '"fra"' in ocr[-1], ocr[-1]
    server.succeed("test -d /var/cache/spoolframe/subtitles")

    # The state survives a restart: same key, and no new setup code.
    server.succeed("systemctl restart spoolframe.service")
    server.wait_until_succeeds("ss -ulnp | grep -q ':7420 '")
    server.wait_until_succeeds(
        "test $(journalctl -u spoolframe.service | grep -c 'listening.*endpoint_id') -ge 2"
    )
    assert re.findall(r"endpoint_id=([0-9a-f]{64})", journal())[-1] == key
    assert journal().count("setup code is") == 1
    users = server.succeed(f"{admin} spoolframe user list")
    assert "alice" in users, users

    # P3-10's options, as the server took them.
    site = server.succeed("curl -sf http://127.0.0.1:7421/auth/site")
    assert "https://media.example.org" in site, site
    shown = server.succeed("spoolframe config show")
    assert 'admin.public_url = "https://media.example.org";  # config file, locked' in shown, shown
    assert 'admin.listen = "127.0.0.1:7421";  # config file, locked' in shown, shown
    # P3-04's provider is offered for signing in, its secret read from
    # the credential.
    assert "Pocket ID" in site, site
    assert "oidc-client-secret" not in shown, shown
    # openFirewall: the client port and mDNS, as well as Plex's answers.
    opened = firewall_udp(server)
    assert {7420, 5353, 32415} <= opened, opened
  '';
}
