"""A made-up library for a demo server (`devenv up demo`): store screenshots
that show no one's posters. Writes, under OUT:

- catalog.json, for fake_plex.py --catalog: a Movies section of invented
  films and a TV Shows section of one invented show;
- images/: each item's poster and backdrop, drawn by ffmpeg (a gradient
  and the title), no artwork of anyone's.

Every item plays the same file, media/feature.mkv (made by the devenv
task with ffmpeg): /data/feature.mkv as Plex reports it.
"""

import argparse
import json
import os
import subprocess
import sys

STAMP = 1760000000

FILMS = [
    # title, year, minutes, rating, genres, summary, two colours
    ("The Lantern Keeper", 2024, 112, "PG-13", ["Drama", "Mystery"],
     "On a storm-cut island, the last lighthouse keeper finds a log book written in her own hand, dated fifty years from now.",
     "0x1b2a49", "0xf2a541"),
    ("Paper Satellites", 2023, 98, "PG", ["Family", "Adventure"],
     "Two kids build a satellite out of cereal boxes, and something in orbit answers back.",
     "0x0b3d91", "0x7fd1ae"),
    ("Saltwater Kings", 2022, 124, "R", ["Crime", "Thriller"],
     "A fishing town's quiet smuggling trade unravels when the new harbour master starts counting boats.",
     "0x0f2027", "0x2c5364"),
    ("Northbound", 2025, 104, "PG-13", ["Road Trip", "Comedy"],
     "Three estranged brothers drive their late father's camper to the Arctic Circle to scatter his ashes, and argue the whole way.",
     "0x355c7d", "0xf67280"),
    ("Glasshouse", 2021, 95, "R", ["Horror"],
     "A botanist house-sits a greenhouse that grows only at night.",
     "0x134e5e", "0x71b280"),
    ("Midnight Garden Radio", 2024, 101, "PG", ["Romance", "Music"],
     "A late-night radio host and the caller who only phones in to request songs that don't exist yet.",
     "0x41295a", "0x2f0743"),
    ("Iron & Ivy", 2020, 131, "PG-13", ["Action", "Sci-Fi"],
     "In a city where buildings grow like trees, a demolition crew is hired to stop the forest.",
     "0x232526", "0x5f8f3b"),
    ("The Cartographer's Daughter", 2019, 118, "PG", ["Adventure", "Drama"],
     "She inherits a map of a country that was never there, and a ticket to go see it.",
     "0x8e5c2c", "0xe8cfa0"),
    ("Low Tide", 2023, 89, "PG-13", ["Mystery"],
     "Every full moon, the tide goes out too far, and something new is left on the sand.",
     "0x1e3c72", "0x2a5298"),
    ("Clockwork Orchard", 2022, 96, "G", ["Animation", "Family"],
     "A tinkering robot tends the last orchard on a terraformed moon.",
     "0xc94b4b", "0x4b134f"),
    ("Second Shift", 2025, 107, "R", ["Thriller"],
     "A night-shift security guard notices the cameras showing tomorrow.",
     "0x000000", "0x434343"),
    ("Summer of the Comet", 2021, 115, "PG", ["Coming of Age"],
     "One last summer before college, one comet, and a promise to watch it together.",
     "0xff7e5f", "0xfeb47b"),
]

SHOW = ("Harbor Lights", 2024, "TV-14", ["Drama"],
        "Life, love and bad weather in a small fishing harbour, told one shift at the cannery at a time.",
        "0x16222a", "0x3a6073")
EPISODES = [
    ["Fog Warning", "The New Boat", "Cannery Row", "Night Catch", "The Regatta", "High Water"],
    ["Off Season", "Salt Lines", "The Inspector", "Lighthouse Party"],
]

FILE = "/data/feature.mkv"


def draw(out, size, top, bottom, title, line2=""):
    """An image `size` (WxH) of a top-to-bottom gradient and `title`."""
    w, h = size.split("x")
    # As large as fits: about 0.55 of the font size a letter.
    font = int(min(int(w) / (9 if int(w) < int(h) else 18), int(w) * 0.85 / (0.55 * max(len(title), 1))))
    text = title.replace(":", r"\:").replace("'", "’")
    sub = line2.replace(":", r"\:").replace("'", "’")
    graph = (
        f"gradients=s={size}:c0={top}:c1={bottom}:x0=0:y0=0:x1={w}:y1={h}:nb_colors=2:d=1"
    )
    if text:
        graph += (f",drawtext=text='{text}':fontcolor=white:fontsize={font}:x=(w-text_w)/2:y=h*0.62:"
                  "shadowcolor=black@0.6:shadowx=3:shadowy=3")
    if sub:
        graph += (f",drawtext=text='{sub}':fontcolor=white@0.85:fontsize={font // 2}:"
                  "x=(w-text_w)/2:y=h*0.62+" + str(int(font * 1.4)))
    subprocess.run(
        ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-y",
         "-f", "lavfi", "-i", graph, "-frames:v", "1", "-q:v", "3", out],
        check=True)


def media(key, minutes, size):
    ms = minutes * 60 * 1000
    return [{
        "id": key * 10, "container": "mkv", "videoCodec": "h264", "videoResolution": "1080",
        "width": 1920, "height": 1080, "audioCodec": "aac", "audioChannels": 2, "duration": ms,
        "Part": [{"id": key * 10 + 1, "file": FILE, "size": size, "duration": ms, "container": "mkv",
                  "Stream": [{"streamType": 1, "codec": "h264"},
                             {"streamType": 2, "codec": "aac", "samplingRate": 48000}]}],
    }]


def art(key):
    return {"thumb": "/library/metadata/%d/thumb/%d" % (key, STAMP),
            "art": "/library/metadata/%d/art/%d" % (key, STAMP)}


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("out")
    parser.add_argument("--feature-size", type=int, default=0,
                        help="media/feature.mkv's size, for each Part")
    args = parser.parse_args()
    images = os.path.join(args.out, "images")
    os.makedirs(images, exist_ok=True)
    items = []

    for n, (title, year, minutes, rating, genres, summary, a, b) in enumerate(FILMS):
        key = 100 + n
        poster, backdrop = (os.path.join(images, "%d-%s.jpg" % (key, k)) for k in ("poster", "art"))
        draw(poster, "600x900", a, b, title, str(year))
        draw(backdrop, "1920x1080", b, a, title)
        items.append({
            "ratingKey": str(key), "key": "/library/metadata/%d" % key, "type": "movie",
            "title": title, "year": year, "originallyAvailableAt": "%d-05-%02d" % (year, 1 + n),
            "summary": summary, "contentRating": rating, "duration": minutes * 60000,
            "guid": "plex://movie/demo%06d" % key, "Guid": [{"id": "tmdb://9%05d" % key}],
            "Genre": [{"tag": g} for g in genres], "audienceRating": 7.0 + (n % 5) * 0.4,
            **art(key), "addedAt": STAMP + n * 3600, "updatedAt": STAMP + n * 3600,
            "librarySectionID": 1, "librarySectionTitle": "Movies",
            "Media": media(key, minutes, args.feature_size),
            "_images": {"thumb": poster, "art": backdrop},
        })

    title, year, rating, genres, summary, a, b = SHOW
    show = 500
    poster, backdrop = (os.path.join(images, "%d-%s.jpg" % (show, k)) for k in ("poster", "art"))
    draw(poster, "600x900", a, b, title, "A Series")
    draw(backdrop, "1920x1080", b, a, title)
    leaves = sum(len(s) for s in EPISODES)
    items.append({
        "ratingKey": str(show), "key": "/library/metadata/%d/children" % show, "type": "show",
        "title": title, "year": year, "summary": summary, "contentRating": rating,
        "guid": "plex://show/demo%06d" % show, "Guid": [{"id": "tvdb://9%05d" % show}],
        "Genre": [{"tag": g} for g in genres], **art(show), "childCount": len(EPISODES),
        "leafCount": leaves, "addedAt": STAMP + 90000, "librarySectionID": 2,
        "librarySectionTitle": "TV Shows", "_images": {"thumb": poster, "art": backdrop},
    })
    for s, names in enumerate(EPISODES, start=1):
        season = show + s * 100
        items.append({
            "ratingKey": str(season), "key": "/library/metadata/%d/children" % season,
            "type": "season", "title": "Season %d" % s, "index": s,
            "parentRatingKey": str(show), "parentTitle": title,
            "guid": "plex://season/demo%06d" % season, "thumb": art(show)["thumb"],
            "leafCount": len(names), "addedAt": STAMP + 90000 + s,
            "librarySectionID": 2, "librarySectionTitle": "TV Shows",
            "_images": {"thumb": poster, "art": backdrop},
        })
        for e, name in enumerate(names, start=1):
            key = season + e
            still = os.path.join(images, "%d-still.jpg" % key)
            draw(still, "1280x720", a, b, name, "S%d · E%d" % (s, e))
            items.append({
                "ratingKey": str(key), "key": "/library/metadata/%d" % key, "type": "episode",
                "title": name, "index": e, "parentIndex": s,
                "parentRatingKey": str(season), "grandparentRatingKey": str(show),
                "parentTitle": "Season %d" % s, "grandparentTitle": title,
                "year": year + s - 1, "originallyAvailableAt": "%d-%02d-%02d" % (year + s - 1, 1 + e, 10),
                "summary": "%s: an episode of %s." % (name, title), "contentRating": rating,
                "duration": 44 * 60000, "guid": "plex://episode/demo%06d" % key,
                "Guid": [{"id": "tvdb://9%05d" % key}], **art(key),
                "parentThumb": art(season)["thumb"], "grandparentThumb": art(show)["thumb"],
                "grandparentArt": art(show)["art"],
                "addedAt": STAMP + 90000 + key, "librarySectionID": 2,
                "librarySectionTitle": "TV Shows", "Media": media(key, 44, args.feature_size),
                "_images": {"thumb": still, "art": backdrop},
            })

    catalog = {
        "name": "Spoolframe Demo Library",
        "sections": [
            {"key": "1", "type": "movie", "title": "Movies", "path": "/data/movies"},
            {"key": "2", "type": "show", "title": "TV Shows", "path": "/data/tv"},
        ],
        "items": items,
    }
    with open(os.path.join(args.out, "catalog.json"), "w", encoding="utf-8") as f:
        json.dump(catalog, f, indent=1)
    sys.stderr.write("demo catalog: %d items in %s\n" % (len(items), args.out))


if __name__ == "__main__":
    main()
