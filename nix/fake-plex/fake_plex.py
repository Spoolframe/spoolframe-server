"""A Plex Media Server stand-in for the NixOS federation test.

It answers the PMS endpoints spoolframe-plex calls (crates/spoolframe-plex),
in the JSON shapes it reads (wire.rs), with fixed data: a Movies section of
two films and a TV section of one show, one season and two episodes. Every
playable item has one Media with one Part, its `file` under /data as Plex
would report it; the server maps /data to the media directory given here
(plex.pathMappings). With --library bob, it is Bob's Movies instead: his
copy of Alice's first film and another of its title (fake_plex.bobs_library).

Paging comes as Plex sends it: X-Plex-Container-Start and -Size, as headers
or in the query. An endpoint not modelled answers an empty container, as a
PMS with nothing to say would; an item it does not have is a 404.
"""

import argparse
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

MACHINE = "fakeplex0000000000000000000000000000000"
STAMP = 1700000000
# Plex's numeric types, as `type=` filters a section's listing.
TYPES = {"1": "movie", "2": "show", "3": "season", "4": "episode", "10": "track"}

SECTIONS = [
    {"key": "1", "type": "movie", "title": "Movies", "path": "/data/movies"},
    {"key": "2", "type": "show", "title": "TV Shows", "path": "/data/tv"},
]


def media(item_id, rel, duration):
    """One version of an item: one H.264/AAC Matroska part at /data/rel."""
    return [
        {
            "id": item_id * 10,
            "container": "mkv",
            "videoCodec": "h264",
            "videoResolution": "sd",
            "audioCodec": "aac",
            "audioChannels": 1,
            "duration": duration,
            "Part": [
                {
                    "id": item_id * 10 + 1,
                    "file": "/data/" + rel,
                    "size": 0,  # filled in from the file at start
                    "duration": duration,
                    "container": "mkv",
                    "Stream": [
                        {"streamType": 1, "codec": "h264"},
                        {"streamType": 2, "codec": "aac", "samplingRate": 48000},
                    ],
                }
            ],
        }
    ]


def thumb(key):
    return "/library/metadata/%d/thumb/%d" % (key, STAMP)


def library():
    """Every item, by ratingKey, as /library/metadata/{key} returns it."""
    films = [
        (101, "Federation Test Film", 2001, "tt0000101", "101",
         "movies/Federation Test Film (2001)/Federation Test Film (2001).mkv"),
        (102, "Second Test Film", 2002, "tt0000102", "102",
         "movies/Second Test Film (2002)/Second Test Film (2002).mkv"),
    ]
    items = {}
    for key, title, year, imdb, tmdb, rel in films:
        items[key] = {
            "ratingKey": str(key),
            "key": "/library/metadata/%d" % key,
            "type": "movie",
            "title": title,
            "year": year,
            "originallyAvailableAt": "%d-06-01" % year,
            "summary": "A film made for the federation test.",
            "contentRating": "PG",
            "duration": 10000,
            "guid": "plex://movie/fake%06d" % key,
            "Guid": [{"id": "imdb://" + imdb}, {"id": "tmdb://" + tmdb}],
            "thumb": thumb(key),
            "addedAt": STAMP + key,
            "updatedAt": STAMP + key,
            "librarySectionID": 1,
            "librarySectionTitle": "Movies",
            "Media": media(key, rel, 10000),
        }
    items[201] = {
        "ratingKey": "201",
        "key": "/library/metadata/201/children",
        "type": "show",
        "title": "Federation Test Show",
        "year": 2010,
        "summary": "A show made for the federation test.",
        "contentRating": "TV-PG",
        "guid": "plex://show/fake000201",
        "Guid": [{"id": "imdb://tt0000201"}, {"id": "tmdb://201"}, {"id": "tvdb://201"}],
        "thumb": thumb(201),
        "childCount": 1,
        "leafCount": 2,
        "addedAt": STAMP + 201,
        "librarySectionID": 2,
        "librarySectionTitle": "TV Shows",
        "Genre": [{"tag": "Documentary"}],
    }
    items[202] = {
        "ratingKey": "202",
        "key": "/library/metadata/202/children",
        "type": "season",
        "title": "Season 1",
        "index": 1,
        "parentRatingKey": "201",
        "parentTitle": "Federation Test Show",
        "guid": "plex://season/fake000202",
        "thumb": thumb(202),
        "leafCount": 2,
        "addedAt": STAMP + 202,
        "librarySectionID": 2,
        "librarySectionTitle": "TV Shows",
    }
    for n, key in ((1, 203), (2, 204)):
        rel = "tv/Federation Test Show/Season 01/Federation Test Show - S01E%02d.mkv" % n
        items[key] = {
            "ratingKey": str(key),
            "key": "/library/metadata/%d" % key,
            "type": "episode",
            "title": "Episode %d" % n,
            "index": n,
            "parentIndex": 1,
            "parentRatingKey": "202",
            "grandparentRatingKey": "201",
            "parentTitle": "Season 1",
            "grandparentTitle": "Federation Test Show",
            "year": 2010,
            "originallyAvailableAt": "2010-01-%02d" % n,
            "summary": "An episode made for the federation test.",
            "contentRating": "TV-PG",
            "duration": 10000,
            "guid": "plex://episode/fake%06d" % key,
            "Guid": [{"id": "imdb://tt0000%d" % key}, {"id": "tvdb://%d" % key}],
            "thumb": thumb(key),
            "parentThumb": thumb(202),
            "grandparentThumb": thumb(201),
            "addedAt": STAMP + key,
            "librarySectionID": 2,
            "librarySectionTitle": "TV Shows",
            "Media": media(key, rel, 10000),
        }
    return items


def film(key, title, year, guids, rel):
    """A film of Bob's Movies, as library() makes Alice's."""
    return {
        "ratingKey": str(key),
        "key": "/library/metadata/%d" % key,
        "type": "movie",
        "title": title,
        "year": year,
        "originallyAvailableAt": "%d-06-01" % year,
        "summary": "A film of Bob's own.",
        "contentRating": "PG",
        "duration": 10000,
        "guid": "plex://movie/fake%06d" % key,
        "Guid": [{"id": g} for g in guids],
        "thumb": thumb(key),
        "addedAt": STAMP + key,
        "updatedAt": STAMP + key,
        "librarySectionID": 1,
        "librarySectionTitle": "Movies",
        "Media": media(key, rel, 10000),
    }


def bobs_library():
    """Bob's Movies (P8-05's search): his own copy of Alice's first film,
    the same title by its TMDB id and year, and another film of that title
    from another year, which is another title."""
    rel = "movies/Second Test Film (2002)/Second Test Film (2002).mkv"
    return {
        301: film(301, "Federation Test Film", 2001, ["tmdb://101"], rel),
        302: film(302, "Federation Test Film", 1987, ["tmdb://987", "imdb://tt0000987"], rel),
    }


LIBRARIES = {
    "alice": (SECTIONS, library),
    "bob": (SECTIONS[:1], bobs_library),
    "empty": ([], dict),
}


class Plex:
    def __init__(self, media_dir, poster, name):
        sections, items = LIBRARIES[name]
        self.sections = sections
        self.items = items()
        self.poster = poster
        for item in self.items.values():
            for version in item.get("Media", []):
                for part in version["Part"]:
                    local = os.path.join(media_dir, part["file"][len("/data/"):])
                    part["size"] = os.path.getsize(local)

    def section(self, key):
        return next((s for s in self.sections if s["key"] == key), None)

    def in_section(self, key, kind=None):
        found = [i for i in self.items.values() if str(i["librarySectionID"]) == key]
        if kind is None:
            # A section's own listing is its top level: films, or shows.
            kind = self.section(key)["type"]
        return sorted((i for i in found if i["type"] == kind), key=lambda i: int(i["ratingKey"]))

    def children(self, key):
        return sorted(
            (i for i in self.items.values() if i.get("parentRatingKey") == key),
            key=lambda i: i.get("index", 0),
        )

    def leaves(self, show):
        return sorted(
            (i for i in self.items.values() if i.get("grandparentRatingKey") == show),
            key=lambda i: (i.get("parentIndex", 0), i.get("index", 0)),
        )


def container(metadata=None, **extra):
    body = {"size": len(metadata or []), "Metadata": metadata or []}
    body.update(extra)
    return {"MediaContainer": body}


def page(items, start, size):
    """`items` from `start`, at most `size` of them, as Plex pages."""
    if size is None:
        return items[start:]
    return items[start:start + size]


class Handler(BaseHTTPRequestHandler):
    plex = None
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("fake-plex: %s\n" % (fmt % args))

    def paging(self, query):
        def read(name):
            value = self.headers.get(name) or (query.get(name) or [None])[0]
            return int(value) if value not in (None, "") else None

        return read("X-Plex-Container-Start") or 0, read("X-Plex-Container-Size")

    def send_json(self, body, status=200):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def not_found(self):
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):  # noqa: N802 (http.server's name)
        url = urlsplit(self.path)
        query = parse_qs(url.query)
        parts = [p for p in url.path.split("/") if p]
        plex = self.plex
        start, size = self.paging(query)

        if parts == []:
            return self.send_json(container(
                friendlyName="Fake Plex", version="1.40.0.0000", machineIdentifier=MACHINE))
        if parts == ["identity"]:
            return self.send_json(container(machineIdentifier=MACHINE, version="1.40.0.0000"))
        if parts == ["library", "sections"]:
            directories = [
                {"key": s["key"], "type": s["type"], "title": s["title"],
                 "Location": [{"id": int(s["key"]), "path": s["path"]}]}
                for s in plex.sections
            ]
            return self.send_json({"MediaContainer": {
                "size": len(directories), "Directory": directories}})
        if len(parts) == 4 and parts[:2] == ["library", "sections"] and parts[3] == "all":
            section = plex.section(parts[2])
            if section is None:
                return self.not_found()
            kind = TYPES.get((query.get("type") or [None])[0])
            items = plex.in_section(parts[2], kind)
            guid = (query.get("guid") or [None])[0]
            if guid is not None:
                items = [i for i in items if i.get("guid") == guid
                         or any(g["id"] == guid for g in i.get("Guid", []))]
            return self.send_json(container(
                page(items, start, size), totalSize=len(items),
                librarySectionID=int(section["key"]), librarySectionTitle=section["title"]))
        if len(parts) == 4 and parts[:2] == ["library", "sections"] and parts[3] == "search":
            # A title search: the words in the title, of the type, in the
            # year if one is given.
            section = plex.section(parts[2])
            if section is None:
                return self.not_found()
            kind = TYPES.get((query.get("type") or [None])[0])
            text = ((query.get("query") or [""])[0]).lower()
            year = (query.get("year") or [None])[0]
            items = [i for i in plex.in_section(parts[2], kind or section["type"])
                     if text in i["title"].lower() and (year is None or str(i.get("year")) == year)]
            return self.send_json(container(
                page(items, start, size), totalSize=len(items),
                librarySectionID=int(section["key"]), librarySectionTitle=section["title"]))
        if parts == ["library", "recentlyAdded"]:
            items = sorted(
                (i for i in plex.items.values() if i["type"] in ("movie", "episode")),
                key=lambda i: -i["addedAt"])
            return self.send_json(container(page(items, start, size), totalSize=len(items)))
        if len(parts) >= 3 and parts[:2] == ["library", "metadata"]:
            return self.metadata(parts[2:], start, size)
        # Search, hubs, playlists, watch state: nothing here.
        return self.send_json(container(totalSize=0, Hub=[]))

    def metadata(self, rest, start, size):
        plex = self.plex
        keys = rest[0].split(",")
        found = [plex.items[int(k)] for k in keys if k.isdigit() and int(k) in plex.items]
        if not found:
            return self.not_found()
        item = found[0]
        section = {"librarySectionID": item["librarySectionID"],
                   "librarySectionTitle": item["librarySectionTitle"]}
        if len(rest) == 1:
            return self.send_json(container(found, **section))
        if rest[1:] == ["children"]:
            return self.send_json(container(plex.children(item["ratingKey"]), **section))
        if rest[1:] == ["allLeaves"]:
            leaves = plex.leaves(item["ratingKey"])
            return self.send_json(container(
                page(leaves, start, size), totalSize=len(leaves), **section))
        if len(rest) == 3 and rest[1] in ("thumb", "art"):
            with open(plex.poster, "rb") as f:
                data = f.read()
            self.send_response(200)
            self.send_header("Content-Type", "image/png")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return None
        return self.not_found()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--listen", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=32400)
    parser.add_argument("--media-dir", required=True, help="what /data is on the server")
    parser.add_argument("--library", choices=sorted(LIBRARIES), default="alice",
                        help="whose library it serves; empty: no sections")
    args = parser.parse_args()
    poster = os.path.join(args.media_dir, "poster.png")
    Handler.plex = Plex(args.media_dir, poster, args.library)
    server = ThreadingHTTPServer((args.listen, args.port), Handler)
    sys.stderr.write("fake-plex: listening on %s:%d\n" % (args.listen, args.port))
    server.serve_forever()


if __name__ == "__main__":
    main()
