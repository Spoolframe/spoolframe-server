# The NixOS module in a VM: the service starts without a reachable Plex,
# prints a setup code, takes its first admin and users through the CLI,
# listens on its UDP port, finds only software encoders without a GPU,
# and keeps its key over a restart.
{
  pkgs,
  spoolframePackage,
  spoolframeModule,
}:
pkgs.testers.runNixOSTest {
  name = "spoolframe-module";

  nodes.server = {
    imports = [spoolframeModule];
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
    };
  };

  testScript = ''
    import re

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
  '';
}
