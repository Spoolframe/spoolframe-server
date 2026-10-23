# Federation end to end (P8-01 to P8-07), in VMs, with nothing reaching
# the Internet: no relay, no pkarr, no mDNS, no port mapping or address
# lookup on the servers, and none of them on the phone's SDK.
#
#   phone ── VLAN 1 ── sa ── VLAN 2 ── sb ── VLAN 3 ── phone
#
# `sa` (Alice's) serves a fake Plex's Movies and TV Shows. `sb` (Bob's)
# has Bob's account and a Plex of his own Movies: a copy of Alice's first
# film (its TMDB id and year), and another film of that title from another
# year. `phone` is Bob's device, signed in to `sb` only, on VLAN 3; while
# its VLAN 1 is up it can also dial `sa` itself. The phone is
# `spoolframe-probe federation`, an SDK app that prints what it did as
# JSON lines.
#
# a. Both servers start and take their first admin.
# b. SA's admin makes a federation invite; SB's redeems it; each lists the
#    other, with the key the other has.
# c. SA shares Movies with SB; SB syncs; Bob's phone lists SA's films
#    under SA's server, and no TV Shows. Search (P8-05) finds Alice's
#    first film once, as Bob's copy also on SA, and the other year's film
#    apart; played from SA by choice, the title's copy is SA's.
# d. Direct: the phone plays SA's film from SA. Its bytes are the file's;
#    SA counts a direct play, SB carried none. It downloads SA's second
#    film (P8-07), prepared by SB and read from SA: the file's bytes.
# e. Proxy: the phone's VLAN 1 goes down; the film plays through SB, three
#    times, at the protocol's largest reads. Its bytes are the file's;
#    both count them proxied, and SB's connection never fails. The
#    download is read through SB, the file's bytes again. SB keeps no copy
#    of either, and no record of the downloads the phone finished; one it
#    made and the phone has not fetched is kept, Ready.
# f. Bob's progress is on SB, and SA keeps no watch state at all.
# g. SA stops sharing Movies: after SB's sync the phone lists nothing of
#    SA's, a play of the film by its id is refused, and the download made
#    but not fetched stays Ready, no longer available.
# h. SB unpairs; both sides list the pair as unpaired.
{
  pkgs,
  spoolframePackage,
  spoolframeModule,
  probePackage,
  fakePlex,
}: let
  inherit (pkgs) lib;
  port = 7420;

  # A server node: the module, federation on, a fake Plex on loopback.
  server = {
    name,
    vlans,
    plexArgs ? "",
  }: {
    imports = [spoolframeModule];
    virtualisation.vlans = vlans;
    virtualisation.memorySize = 2048;
    environment.systemPackages = [pkgs.curl pkgs.sqlite];
    systemd.services.fake-plex = {
      wantedBy = ["multi-user.target"];
      before = ["spoolframe.service"];
      serviceConfig = {
        ExecStart = "${fakePlex.server}/bin/fake-plex --port 32400 ${plexArgs}";
        DynamicUser = true;
      };
    };
    systemd.services.spoolframe = {
      wants = ["fake-plex.service"];
      after = ["fake-plex.service"];
    };
    services.spoolframe = {
      enable = true;
      package = spoolframePackage;
      inherit name port;
      plex.url = "http://127.0.0.1:32400";
      plex.pathMappings = [
        {
          from = "/data";
          to = "${fakePlex.media}";
        }
      ];
      openFirewall = true;
      # Nothing found or announced on the LAN, and IPv4 only: each server
      # is reached at the addresses its pair and its invite name.
      mdns = false;
      ipv6 = false;
      plexDiscovery.enable = false;
      # The module's defaults already reach nothing outside: relays = [],
      # pkarr = "off", wan = null (no port mapping, no address lookup).
      # The films are ten seconds long: a position counts as started
      # from a second in, not the default minute, so a client sees it.
      extraConfig = ''
        experimental {
          federation = true;
        }
        watch {
          started_after_secs = 1;
        }
      '';
    };
  };
in
  pkgs.testers.runNixOSTest {
    name = "spoolframe-federation-e2e";

    nodes.sa = server {
      name = "Alice Server";
      vlans = [1 2];
    };
    nodes.sb = server {
      name = "Bob Server";
      vlans = [2 3];
      plexArgs = "--library bob";
    };
    nodes.phone = {
      virtualisation.vlans = [1 3];
      # Room for the downloads: the SDK keeps a margin free besides the
      # file, which the default disk does not leave.
      virtualisation.diskSize = 4096;
      environment.systemPackages = [probePackage];
      environment.etc."bob-password".text = "bob-password-1\n";
    };

    testScript = {nodes, ...}: let
      # SB as the phone dials it: its VLAN 3 address.
      sbAddr = "${(lib.head nodes.sb.networking.interfaces.eth2.ipv4.addresses).address}:${toString port}";
    in ''
      import base64
      import json
      import re
      import shlex
      import time
      from pathlib import Path

      FILM = "Federation Test Film"
      FILM_FILE = "${fakePlex.film}"
      SECOND_FILE = "${fakePlex.secondFilm}"
      ORIGIN = "http://localhost:7421"

      start_all()

      def journal(m):
          return m.succeed("journalctl -u spoolframe.service --no-pager")

      def found(pattern, text):
          match = re.search(pattern, text)
          assert match is not None, f"{pattern!r} not in {text!r}"
          return match.group(1)

      def set_up(m):
          """The first admin, by the setup code in the log."""
          m.wait_for_unit("spoolframe.service")
          m.wait_until_succeeds("journalctl -u spoolframe.service | grep -q 'setup code is'")
          code = found(r"setup code is ([A-Z0-9-]+)", journal(m))
          m.succeed(f"printf 'admin-pw\n' | spoolframe setup --code {code} --username admin --password-stdin")
          m.wait_until_succeeds(f"curl -sf {ORIGIN}/auth/site")

      def rpc(m, method, params, token=None):
          """One admin JSON-RPC call, as the web UI makes it: from its own
          origin, so an invite records who made it. Its result."""
          body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
          auth = f"-H 'Authorization: Bearer {token}'" if token else ""
          out = m.succeed(
              f"echo {base64.b64encode(body.encode()).decode()} | base64 -d | "
              f"curl -sS --max-time 120 -H 'Content-Type: application/json' -H 'Origin: {ORIGIN}' "
              f"{auth} --data-binary @- {ORIGIN}/jsonrpc"
          )
          reply = json.loads(out)
          assert "error" not in reply, f"{method}: {reply}"
          return reply["result"]

      def login(m):
          return rpc(m, "me_login", [{"username": "admin", "password": "admin-pw"}])["token"]

      def endpoint(m):
          return found(r"endpoint id: (\S+)", m.succeed("spoolframe id"))

      def probe(*args, ok=True):
          """Bob's phone: the federation run, its JSON lines as events."""
          argv = ["spoolframe-probe", "federation", "--endpoint", sb_key, "--addr", "${sbAddr}",
                  "--username", "bob", "--password-file", "/etc/bob-password", *args]
          status, out = phone.execute(" ".join(shlex.quote(a) for a in argv) + " 2>/tmp/probe.log")
          print(out)
          events = [json.loads(line) for line in out.splitlines() if line.startswith("{")]
          assert events and events[-1] == {"event": "done", "ok": ok}, (status, out, phone.succeed("cat /tmp/probe.log"))
          assert (status == 0) == ok, status
          return events

      def only(events, name):
          named = [e for e in events if e["event"] == name]
          assert len(named) == 1, (name, events)
          return named[0]

      def peers(m, token):
          return rpc(m, "federation_peerList", [], token)

      def sha256(m, path):
          return m.succeed(f"sha256sum {shlex.quote(path)}").split()[0]

      def peer_until(m, token, done, what):
          """The one pair `m` lists, once `done` says it is as expected."""
          for _ in range(60):
              [peer] = peers(m, token)
              if done(peer):
                  return peer
              time.sleep(1)
          raise AssertionError(f"{what} never came: {peer}")

      def counted(m, token, want, fields):
          """The pair's counts on `m` once they are `want`: a play is
          counted beside its token's redemption, not before it starts."""
          return peer_until(m, token, lambda p: tuple(p[f] for f in fields) == want, f"{fields} = {want}")

      def pieces(path):
          """Some 64-byte pieces of the file at `path`, one a line for
          `grep -F -f`: ten, spread over it, each without a newline or NUL."""
          data = Path(path).read_bytes()
          taken = []
          step = (len(data) - 8192) // 10
          for at in range(4096, len(data) - 4096, step):
              piece = next(data[i:i + 64] for i in range(at, at + 4096)
                           if b"\n" not in data[i:i + 64] and b"\0" not in data[i:i + 64])
              taken.append(piece)
          assert len(taken) >= 8, len(taken)
          return b"\n".join(taken) + b"\n"

      film_size = int(sa.succeed(f"stat -c %s {shlex.quote(FILM_FILE)}"))
      film_sha = sha256(sa, FILM_FILE)
      second_size = int(sa.succeed(f"stat -c %s {shlex.quote(SECOND_FILE)}"))
      second_sha = sha256(sa, SECOND_FILE)
      media_pieces = base64.b64encode(pieces(FILM_FILE) + pieces(SECOND_FILE)).decode()

      def downloaded(events, size):
          """The download the run finished: whole, done, its file's path."""
          done = only(events, "download")
          assert done["phase"] == {"type": "done"}, done
          assert done["size"] == size and done["received"] == size, done
          return done["file"]

      def device_redeemed_downloads():
          """How many download tokens a device redeemed on SA itself: SB's
          own, which prepare and relay a download, are proxied."""
          lines = sa.succeed("journalctl -u spoolframe.service --no-pager | grep \"a paired server's token is redeemed\" || true")
          return len([l for l in lines.splitlines() if re.search(r'proxied=false scope="?download', l)])

      with subtest("a. both servers start and take their first admin"):
          set_up(sa)
          set_up(sb)
          sa_token = login(sa)
          sb_token = login(sb)
          sb.succeed("printf 'bob-password-1\n' | SPOOLFRAME_ADMIN_USER=admin SPOOLFRAME_ADMIN_PASSWORD=admin-pw spoolframe user add bob --password-stdin")
          sa_key = endpoint(sa)
          sb_key = endpoint(sb)
          sa_print = rpc(sa, "admin_serverStatus", [], sa_token)["fingerprint"]
          sb_print = rpc(sb, "admin_serverStatus", [], sb_token)["fingerprint"]
          # Nothing reaches out: no relay is used, nothing is published.
          for m in (sa, sb):
              shown = m.succeed("spoolframe config show")
              assert "network.relays = [];" in shown, shown
              assert 'network.pkarr = "off";' in shown, shown

      with subtest("b. SB redeems SA's federation invite, and both keep the pair"):
          invite = rpc(sa, "admin_inviteCreate",
                       [{"kind": "federation", "note": None, "uses": None, "expires": None, "role": None}],
                       sa_token)
          assert invite["invite"]["kind"] == "federation", invite
          assert invite["invite"]["createdBy"] == "admin", invite
          paired = rpc(sb, "federation_redeem", [invite["link"]], sb_token)
          assert paired["endpointId"] == sa_key, paired
          assert paired["fingerprint"] == sa_print, (paired, sa_print)
          sa_on_sb = paired["id"]
          [on_sa] = peers(sa, sa_token)
          assert on_sa["endpointId"] == sb_key, on_sa
          assert on_sa["fingerprint"] == sb_print, (on_sa, sb_print)
          assert on_sa["inviteId"] == invite["invite"]["id"], on_sa
          assert on_sa["revokedAt"] is None, on_sa
          sb_on_sa = on_sa["id"]
          [on_sb] = peers(sb, sb_token)
          assert on_sb["fingerprint"] == sa_print and on_sb["revokedAt"] is None, on_sb

      with subtest("c. SA shares Movies, SB syncs, the phone lists SA's films, and search merges the title"):
          libraries = rpc(sa, "admin_libraries", [], sa_token)["libraries"]
          assert sorted(l["name"] for l in libraries) == ["Movies", "TV Shows"], libraries
          movies_id = next(l["id"] for l in libraries if l["name"] == "Movies")
          rpc(sa, "federation_grantAdd", [sb_on_sa, movies_id], sa_token)
          rpc(sb, "federation_refresh", [sa_on_sb], sb_token)
          events = probe("--list-items")
          assert only(events, "signed_in")["federation"] is True, events
          remote = [e["library"] for e in events if e["event"] == "library" and e["library"].get("server")]
          assert [l["title"] for l in remote] == ["Movies"], remote
          assert remote[0]["server"]["name"] == "Alice Server", remote
          assert remote[0]["server"]["online"] is True, remote
          sa_server_id = remote[0]["server"]["server_id"]
          titles = sorted(e["title"] for e in events if e["event"] == "item")
          assert titles == [FILM, "Second Test Film"], titles
          film_id = next(e["id"] for e in events if e["event"] == "item" and e["title"] == FILM)
          second_id = next(e["id"] for e in events if e["event"] == "item" and e["title"] == "Second Test Film")
          assert film_id.startswith("fed:") and second_id.startswith("fed:"), (film_id, second_id)
          # P8-05: Alice's first film is Bob's 2001 copy too (its TMDB id
          # and year): one hit, Bob's, also on Alice's. His 1987 film of
          # the same title is another title, a hit of its own.
          hits = [e for e in probe("--search", FILM) if e["event"] == "hit"]
          assert sorted(h["year"] for h in hits) == [1987, 2001], hits
          shared = next(h for h in hits if h["year"] == 2001)
          other = next(h for h in hits if h["year"] == 1987)
          assert shared["origin"] is None and not shared["id"].startswith("fed:"), shared
          assert shared["also_on"] == [{"id": film_id, "server": "Alice Server"}], shared
          assert shared["play_from"] == shared["id"], shared
          assert other["origin"] is None and other["also_on"] == [], other
          # Played from Alice's by choice: the title's copy is hers.
          copies = only(probe("--play-from", shared["id"], "--from", sa_server_id), "copies")
          assert copies["play_from"] == film_id, copies

      with subtest("d. direct: the phone plays SA's film from SA, and downloads its second"):
          direct = probe("--play-title", FILM, "--out", "/tmp/direct.mkv", "--progress-ms", "4000")
          playing = only(direct, "playing")
          assert playing["mode"] == "direct" and playing["route"] == "direct", playing
          assert playing["path"] == "lan", playing
          assert playing["session_server"] == sa_key, playing
          assert playing["plan"] == "direct" and playing["size"] == film_size, playing
          read = only(direct, "read")
          assert read["complete"] and read["bytes"] == film_size, read
          assert sha256(phone, "/tmp/direct.mkv") == film_sha
          counted(sa, sa_token, (1, 1, 0), ("playsHere", "directHere", "proxiedHere"))
          counted(sb, sb_token, (1, 0), ("playsThere", "proxiedThere"))
          # P8-07: prepared by SB, read from SA under a token for the
          # phone, every track as it is: the file's own bytes.
          before = device_redeemed_downloads()
          file = downloaded(probe("--download-title", "Second Test Film", "--downloads-dir", "/root/dl-direct"), second_size)
          assert sha256(phone, file) == second_sha
          assert device_redeemed_downloads() == before + 1, "the phone did not read it from SA"

      with subtest("e. proxy: without VLAN 1 the film plays, and downloads, through SB"):
          phone.succeed("ip link set eth1 down")
          phone.fail("ping -c 1 -W 2 ${nodes.sa.networking.primaryIPAddress}")
          # At the protocol's largest reads (16 MiB, MAX_READ), as the
          # direct play reads, three times.
          for n in range(1, 4):
              out = f"/tmp/proxied-{n}.mkv"
              events = probe("--play-title", FILM, "--out", out)
              playing = only(events, "playing")
              assert playing["mode"] == "proxied" and playing["route"] == "proxied", playing
              assert playing["session_server"] == sb_key, playing
              read = only(events, "read")
              assert read["complete"] and read["bytes"] == film_size, read
              assert sha256(phone, out) == film_sha
          counted(sa, sa_token, (4, 1, 3), ("playsHere", "directHere", "proxiedHere"))
          counted(sb, sb_token, (4, 3), ("playsThere", "proxiedThere"))
          # Not one connection lost to a fragmented stream on the way.
          sb.fail("journalctl -u spoolframe.service --no-pager | grep -q 'too many gaps'")
          # The download, again, from a phone that cannot reach SA: read
          # through SB, and no token redeemed by the phone.
          before = device_redeemed_downloads()
          file = downloaded(probe("--download-title", "Second Test Film", "--downloads-dir", "/root/dl-proxied"), second_size)
          assert sha256(phone, file) == second_sha
          assert device_redeemed_downloads() == before, "the phone read it from SA"
          # A device that has a file whole lets it go (CancelDownload, as
          # done): SB keeps no record of either.
          finished = sb.succeed("sqlite3 -readonly /var/lib/spoolframe/spoolframe.db 'SELECT count(*) FROM federation_downloads'").strip()
          assert finished == "0", finished
          # One made and not fetched yet, as for a device away: SB keeps it.
          status = only(probe("--prepare-id", second_id), "download_status")
          [waiting] = status["downloads"]
          assert waiting["state"]["type"] == "ready" and not waiting.get("no_longer_available"), waiting
          # SB passed the bytes on and kept none: no file in its state,
          # cache or private temp holds any of a few pieces of either film.
          # (Its caches' files are preallocated, so their sizes say
          # nothing.) The same search finds them where they are, on SA.
          for m in (sa, sb):
              m.succeed(f"echo {media_pieces} | base64 -d > /root/media-pieces")
          sa.succeed(f"grep -rlaF -f /root/media-pieces {shlex.quote(str(Path(FILM_FILE).parent.parent))}")
          kept = sb.execute("grep -rlaF -f /root/media-pieces /var/lib/spoolframe /var/cache/spoolframe /tmp")
          assert kept == (1, ""), kept

      with subtest("f. Bob's progress is on SB, and SA keeps no watch state"):
          # The direct play reported 4 s to SB, and SB gave it back.
          progress = only(direct, "progress")
          assert progress == {"event": "progress", "reported_ms": 4000, "stored_ms": 4000}, progress
          stored = sb.succeed(
              "sqlite3 -readonly /var/lib/spoolframe/spoolframe.db "
              "\"SELECT position_ms FROM watch_state WHERE media_id LIKE 'fed:%'\""
          ).split()
          assert stored == ["4000"], stored
          on_sa = sa.succeed("sqlite3 -readonly /var/lib/spoolframe/spoolframe.db 'SELECT count(*) FROM watch_state'").strip()
          assert on_sa == "0", on_sa

      with subtest("g. SA stops sharing Movies: SB's sync drops them, a play is refused, the download is kept"):
          rpc(sa, "federation_grantRevoke", [sb_on_sa, movies_id], sa_token)
          rpc(sb, "federation_refresh", [sa_on_sb], sb_token)
          events = probe("--list-items")
          remote = [e for e in events if e["event"] == "library" and e["library"].get("server")]
          assert remote == [], remote
          events = probe("--play-id", film_id, ok=False)
          error = only(events, "error")
          assert (error["stage"], error["kind"]) == ("play", "NotFound"), error
          assert not [e for e in events if e["event"] == "playing"], events
          # The download made and not fetched stays Ready, marked no longer
          # available: a device that has it keeps it, and none reads more.
          [status] = only(probe("--download-status", waiting["id"]), "download_status")["downloads"]
          assert status["id"] == waiting["id"], status
          assert status["state"]["type"] == "ready" and status["state"]["sha256"] == second_sha, status
          assert status.get("no_longer_available") is True, status

      with subtest("h. SB unpairs, and both sides say so"):
          rpc(sb, "federation_peerRevoke", [sa_on_sb], sb_token)
          [on_sb] = peers(sb, sb_token)
          assert on_sb["revokedAt"] is not None and not on_sb["connected"], on_sb
          # SA hears it over their connection, and unpairs its side.
          on_sa = peer_until(sa, sa_token, lambda p: p["revokedAt"] is not None, "SA's unpairing")
          assert on_sa["id"] == sb_on_sa, on_sa
          # Nothing of SA's is left to Bob.
          events = probe("--list-items")
          assert not [e for e in events if e["event"] == "library" and e["library"].get("server")], events
    '';
  }
