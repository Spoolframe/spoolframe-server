# The NixOS module in VMs. `server`, with P3-04's and P3-10's options set
# (a proxy's public URL and trusted proxies, a sign-in provider, the admin
# listener on loopback): the service starts without a reachable Plex,
# prints a setup code, takes its first admin and users through the CLI,
# listens on its UDP port, finds only software encoders without a GPU,
# reads subtitles with the tesseract languages it was given, and keeps its
# key over a restart, all under the unit's sandbox, which
# `systemd-analyze security` scores; ffmpeg and tesseract run in a
# bubblewrap sandbox of their own inside it. `fresh`, the module with nothing but
# `enable`: not set up, its web UI answering on loopback only, insecure
# cookies locked off, its state where the module says and its own user's
# alone, and the firewall opening none of its ports (PX-11).
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
    # PX-11 findings 32 and 33: loopback, and no insecure cookies.
    assert 'admin.listen = "127.0.0.1:7421";  # config file, locked' in shown, shown
    assert "admin.insecure_cookies = false;  # config file, locked" in shown, shown
    # PX-11 finding 34: the media tools confined, or no server.
    assert 'media.sandbox = "required";  # config file, locked' in shown, shown
    fresh.wait_until_succeeds("journalctl -u spoolframe.service | grep -q 'media tools run confined'")
    # Its state where the module says, the service's own.
    fresh.succeed("test -s /var/lib/spoolframe/endpoint.key")
    fresh.succeed("test -s /var/lib/spoolframe/spoolframe.db")
    assert fresh.succeed("stat -c '%U %a' /var/lib/spoolframe").strip() == "spoolframe 700"
    # PX-11 finding 35: the database and the caches are its user's alone.
    for path in ["spoolframe.db", "spoolframe.db-wal"]:
        mode = fresh.succeed(f"stat -c %a /var/lib/spoolframe/{path}").strip()
        assert mode == "600", f"{path}: {mode}"
    for path in ["cache", "cache/artwork", "cache/previews"]:
        mode = fresh.succeed(f"stat -c %a /var/lib/spoolframe/{path}").strip()
        assert mode == "700", f"{path}: {mode}"
    # `spoolframe id` prints the endpoint id and the server id, a line each.
    ids = re.search(r"endpoint id: (\S+)", fresh.succeed("spoolframe id"))
    assert ids is not None
    endpoint = ids.group(1)
    assert re.search(f"endpoint_id={endpoint}", fresh.succeed("journalctl -u spoolframe.service")), endpoint
    # Without openFirewall nothing of ours is opened, Plex's answers
    # included (PX-11 finding 36).
    opened = firewall_udp(fresh)
    assert not {7420, 5353, 32415} & opened, opened
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

    # PX-11 finding 34: bubblewrap works inside the unit, so the encoder
    # test and tesseract below ran confined; had it not, the service
    # would not have started.
    assert "media tools run confined" in journal(), journal()
    assert "MEDIA TOOLS RUN UNCONFINED" not in journal()
    print(server.succeed("systemctl show spoolframe.service -p RestrictNamespaces"))

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
    # Its proxy is named: no warning about one that is not (PX-11 38).
    assert "trusted_proxies is empty" not in journal()

    # PX-11 finding 34: the unit's sandbox, as systemd scores it (1.6
    # when this was written; 2.5 once the media tools' bubblewrap needed
    # six namespaces, the mount calls, capset and all of /proc; at most
    # 2.5, out of 10, passes). ffmpeg (the encoder test at start) and
    # tesseract (its languages) ran under it above, each confined.
    props = server.succeed(
        "systemctl show spoolframe.service -p UMask -p TasksMax -p MemoryMax -p MemoryHigh "
        "-p ProtectHome -p ProtectProc -p ProcSubset -p SystemCallArchitectures"
    )
    for want in ["UMask=0077", "TasksMax=4096", "MemoryMax=8589934592", "MemoryHigh=6442450944",
                 "ProtectHome=yes", "ProtectProc=invisible", "ProcSubset=all",
                 "SystemCallArchitectures=native"]:
        assert want in props, f"{want}: {props}"
    exposure = server.succeed("systemd-analyze security spoolframe.service --no-pager | tail -n 1")
    print(exposure)
    server.succeed("systemd-analyze security --threshold=25 spoolframe.service --no-pager")
  '';
}
