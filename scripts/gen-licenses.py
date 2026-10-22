#!/usr/bin/env python3
"""Writes the list behind the admin pages' Open source licenses.

    scripts/gen-licenses.py           write crates/spoolframe-web/assets/licenses.json
    scripts/gen-licenses.py --check   fail if that file is not what this would write

Run it inside `devenv shell` after Cargo.lock changes; CI runs it with
--check (devenv.nix, spoolframe:licenses-list).

The file is committed, not made by build.rs: the Nix build has no
network, and `cargo metadata` would need it.

What is listed is what the server ships:

- The server binary: `cargo tree -e normal,no-proc-macro` from
  spoolframe-server with `embed-web`, as nix/package.nix builds it, for
  each system the flake builds (TARGETS).
- The web app's wasm: the same from spoolframe-web, for wasm32.
  Normal dependencies only: no build-dependencies, no dev-dependencies, no
  proc-macros (they run in the compiler, none of their code ships), no
  crates for other platforms. cargo tree resolves features as a build of
  the package would; `cargo metadata`'s resolve turns on what
  dev-dependencies ask for too, and lists more.
- Fixed entries, read from the repo (fixed_components): what the web app
  carries into the browser besides crates. The Catppuccin palette in
  input.css, the Lucide icons copied into src/app/icons.rs, the fonts the
  build copies in from nixpkgs (their name tables, SPOOLFRAME_WEB_FONTS)
  and Tailwind, whose output is the stylesheet.

- The programs the server runs (programs): FFmpeg, built with libplacebo
  (nix/ffmpeg.nix), and Tesseract (nix/package.nix), as nixpkgs builds
  them at the revision flake.lock pins. They run as separate programs,
  not linked into the server; they are listed because the server's image
  ships them.

The C libraries the server links from the host (OpenSSL, libucl) are not
listed: Nix and the distributions that ship them carry their notices.

Groups: this workspace's crates are "spoolframe", every other crate is
"crates", the programs "programs", and the web app's fixed entries
"browser". A crate's `ships` names where it is: "server", "web" or both.

Texts: each crate's own license files, matched to the SPDX ids of its
license expression by their wording. An id with no file of its own gets
the standard text from scripts/licenses/spdx/<id>.txt. Texts are kept
once each, however many crates share them.

Python 3 standard library only.
"""

import argparse
import json
import os
import re
import struct
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WEB = os.path.join(ROOT, "crates", "spoolframe-web")
OUT = os.path.join(WEB, "assets", "licenses.json")
LICENSES_DIR = os.path.join(ROOT, "scripts", "licenses")
SPDX_DIR = os.path.join(LICENSES_DIR, "spdx")

SERVER = "spoolframe-server"
SERVER_FEATURES = "embed-web"
# The flake's systems (flake.nix).
TARGETS = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "aarch64-apple-darwin"]
WEB_APP = "spoolframe-web"
WEB_TARGET = "wasm32-unknown-unknown"

# The fonts the build copies from nixpkgs' google-fonts into the web app's
# assets (devenv.nix and nix/package.nix, `install -m 644 "$t/<file>"`):
# the file there to the name shown and its OFL text in
# scripts/licenses/fonts. --check fails for a font the build copies with no
# entry here, and for one whose name table no longer says OFL 1.1 or whose
# copyright differs from the first line of that text.
FONTS = {
    "BricolageGrotesque[opsz,wdth,wght].ttf": ("Bricolage Grotesque", "BricolageGrotesque-OFL.txt"),
    "Figtree[wght].ttf": ("Figtree", "Figtree-OFL.txt"),
    "JetBrainsMono[wght].ttf": ("JetBrains Mono", "JetBrainsMono-OFL.txt"),
    "Outfit[wght].ttf": ("Outfit", "Outfit-OFL.txt"),
}
FONT_BUILDS = [os.path.join(ROOT, "devenv.nix"), os.path.join(ROOT, "nix", "package.nix")]

# The Tailwind release scripts/licenses/tailwindcss.LICENSE was read from.
# When the committed stylesheet's banner names another, --check fails until
# the file is looked at again.
TAILWIND = "4.3.3"

# The programs the server runs, as read in nixpkgs at PROGRAMS_NIXPKGS, the
# revision flake.lock pinned then: name, version, SPDX id as built, and the
# copyright line the program itself prints. --check fails when flake.lock
# pins another nixpkgs, until they are read again there and this is
# updated.
#
# FFmpeg: pkgs/development/libraries/ffmpeg/generic.nix defaults withGPL
# and withVersion3 to true, so ffmpeg-headless is configured with
# --enable-gpl and --enable-version3 (and x264, x265, xvid, vid.stab):
# GPL-3.0-or-later as built, which its meta.license lists and the built
# `ffmpeg -L` says. libplacebo (pkgs/by-name/li/libplacebo, what FFmpeg
# 6.1 and later take) is lgpl21Plus. Tesseract (tesseract5.nix) is asl20.
PROGRAMS_NIXPKGS = "b6c8664de9b6cc07fe5666a29f91884ba81197c4"
PROGRAMS = [
    ("FFmpeg", "9.0.1", "GPL-3.0-or-later", ["Copyright (c) 2000-2026 the FFmpeg developers"]),
    ("libplacebo", "7.360.1", "LGPL-2.1-or-later", []),
    ("Tesseract", "5.5.3", "Apache-2.0", []),
]

# A license file's id, by its wording. The first that matches wins, so the
# more specific come first.
SIGNATURES = [
    ("Apache-2.0 WITH LLVM-exception", ["Apache License", "LLVM Exceptions"]),
    ("Apache-2.0", ["Apache License", "Version 2.0, January 2004"]),
    ("MIT", ["Permission is hereby granted, free of charge",
             "The above copyright notice and this permission notice shall be included"]),
    ("MIT-0", ["Permission is hereby granted, free of charge"]),
    ("MPL-2.0", ["Mozilla Public License", "2.0"]),
    ("BSD-3-Clause", ["Redistribution and use in source and binary forms", "Neither the name"]),
    ("BSD-2-Clause", ["Redistribution and use in source and binary forms"]),
    ("ISC", ["Permission to use, copy, modify, and/or distribute this software for any"]),
    ("Unlicense", ["This is free and unencumbered software released into the public domain"]),
    ("CC0-1.0", ["CC0 1.0 Universal"]),
    ("Unicode-3.0", ["UNICODE LICENSE V3"]),
    ("Zlib", ["This software is provided 'as-is', without any express or implied"]),
    ("BSL-1.0", ["Boost Software License - Version 1.0"]),
    ("CDLA-Permissive-2.0", ["Community Data License Agreement", "Permissive", "Version 2.0"]),
    ("AGPL-3.0", ["GNU AFFERO GENERAL PUBLIC LICENSE", "Version 3, 19 November 2007"]),
    ("GPL-3.0", ["GNU GENERAL PUBLIC LICENSE", "Version 3, 29 June 2007"]),
    ("OFL-1.1", ["SIL OPEN FONT LICENSE", "Version 1.1"]),
]

LICENSE_FILE = re.compile(r"(?i)^(licen[cs]e|copying|unlicense|copyright)")
NOTICE_FILE = re.compile(r"(?i)^notice")
# Copyright lines in a license's own template or about the license itself.
NOT_A_HOLDER = re.compile(
    r"(?i)\[yyyy\]|\{yyyy\}|<year>|\[year\]|<copyright|\[name of|"
    r"notice|license|Free Software Foundation|holders? or contributors"
)


def cargo(*args):
    return subprocess.run(
        ["cargo", *args], cwd=ROOT, check=True, capture_output=True, text=True
    ).stdout


def linked(package, target, features=None):
    """The (name, version) of every crate `package` links for `target`."""
    args = ["tree", "--locked", "-p", package, "-e", "normal,no-proc-macro",
            "--target", target, "--prefix", "none", "-f", "{p}"]
    if features:
        args += ["--features", features]
    out = set()
    for line in cargo(*args).splitlines():
        m = re.match(r"^(\S+) v(\S+)", line)
        if m:
            out.add((m.group(1), m.group(2)))
    return out


def normalize(text):
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    return "\n".join(line.rstrip() for line in text.split("\n")).strip("\n") + "\n"


def read(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        return normalize(f.read())


def rel(path):
    return os.path.relpath(path, ROOT)


def classify(text):
    """The ids a file's wording matches: more than one for a file that
    holds several licenses."""
    found = []
    for spdx, phrases in SIGNATURES:
        if all(p in text for p in phrases):
            # Apache-2.0 WITH LLVM-exception is Apache-2.0 too; MIT is MIT-0's wording and more.
            if spdx == "Apache-2.0" and "Apache-2.0 WITH LLVM-exception" in found:
                continue
            if spdx == "MIT-0" and "MIT" in found:
                continue
            if spdx == "BSD-2-Clause" and "BSD-3-Clause" in found:
                continue
            found.append(spdx)
    # 0BSD is ISC's wording less the condition to keep the notice.
    if "ISC" in found and "provided that the above copyright notice" not in " ".join(text.split()):
        found[found.index("ISC")] = "0BSD"
    return found


def ids(expression):
    """The license ids an SPDX expression names, `X WITH Y` as one."""
    tokens = re.findall(r"\(|\)|[^\s()]+", expression)
    out, i = [], 0
    while i < len(tokens):
        t = tokens[i]
        if t in ("(", ")", "AND", "OR"):
            i += 1
            continue
        if i + 2 < len(tokens) and tokens[i + 1] == "WITH":
            t = f"{t} WITH {tokens[i + 2]}"
            i += 2
        if t not in out:
            out.append(t)
        i += 1
    return out


def base_id(spdx):
    """The GNU licenses' -only and -or-later share one text."""
    return re.sub(r"-(only|or-later)$", "", spdx)


def holders(texts):
    lines = []
    for text in texts:
        for line in text.split("\n"):
            line = line.strip().strip("*#/ ").strip()
            if not re.match(r"^Copyright\b", line) or NOT_A_HOLDER.search(line):
                continue
            if len(line) <= len("Copyright (c)") or line in lines:
                continue
            lines.append(line)
    return lines[:12]


def standard(spdx):
    path = os.path.join(SPDX_DIR, f"{base_id(spdx)}.txt")
    if not os.path.exists(path):
        sys.exit(f"gen-licenses: no standard text for {spdx}: add {rel(path)}")
    return read(path)


def crate_texts(package, workspace_root, workspace):
    """{id: text} for each id in the crate's expression, and its copyright lines."""
    crate_dir = os.path.dirname(package["manifest_path"])
    expression = package["license"]
    if not expression and package.get("license_file"):
        # `license-file` alone (uclicious-libucl-sys): the file's wording
        # names it, as cargo deny reads it.
        matched = classify(read(os.path.join(crate_dir, package["license_file"])))
        if len(matched) == 1:
            expression = matched[0]
    if not expression:
        sys.exit(f"gen-licenses: {package['name']} {package['version']} has no license expression")
    expression = expression.replace("/", " OR ")
    wanted = ids(expression)
    dirs = [crate_dir] + ([workspace_root] if package["id"] in workspace else [])
    files, notices = [], []
    for d in dirs:
        for name in sorted(os.listdir(d)):
            path = os.path.join(d, name)
            if not os.path.isfile(path):
                continue
            if LICENSE_FILE.match(name):
                files.append(read(path))
            elif NOTICE_FILE.match(name):
                notices.append(read(path))
        if files:
            break
    if package.get("license_file"):
        files.append(read(os.path.join(crate_dir, package["license_file"])))

    texts = {}
    for text in files:
        matched = classify(text)
        # A file holding several licenses is not any one of them.
        if len(matched) != 1:
            continue
        for spdx in wanted:
            if base_id(spdx) == matched[0] and spdx not in texts:
                texts[spdx] = text
    for spdx in wanted:
        if spdx not in texts:
            texts[spdx] = standard(spdx)
    # Apache-2.0 §4(d): a NOTICE file goes with the work.
    for spdx in wanted:
        if spdx.startswith("Apache-2.0") and notices:
            texts[spdx] = texts[spdx] + "\n\nNOTICE\n\n" + "\n".join(notices)

    lines = holders(files)
    if not lines and package.get("authors"):
        names = [re.sub(r"\s*<[^>]*>", "", a).strip() for a in package["authors"]]
        names = [n for n in names if n]
        if names:
            lines = ["Copyright (c) " + ", ".join(names)]
    return expression, texts, lines


def font_names(path):
    """{nameID: string} of a TTF's `name` table (first record of each id)."""
    with open(path, "rb") as f:
        data = f.read()
    for i in range(struct.unpack(">H", data[4:6])[0]):
        tag, _, offset, _ = struct.unpack(">4sIII", data[12 + 16 * i:28 + 16 * i])
        if tag != b"name":
            continue
        count, strings = struct.unpack(">HH", data[offset + 2:offset + 6])
        out = {}
        for j in range(count):
            platform, _, _, name_id, length, at = struct.unpack(
                ">6H", data[offset + 6 + 12 * j:offset + 18 + 12 * j]
            )
            raw = data[offset + strings + at:offset + strings + at + length]
            out.setdefault(name_id, raw.decode("utf-16-be" if platform in (0, 3) else "latin-1"))
        return out
    sys.exit(f"gen-licenses: {path} has no name table")


def built_fonts():
    """The font files each build copies in; the same set in every build."""
    copied = None
    for path in FONT_BUILDS:
        with open(path, encoding="utf-8") as f:
            names = set(re.findall(r'install -m 644 "\$t/([^"]+)"', f.read()))
        if copied is not None and names != copied:
            sys.exit(f"gen-licenses: {rel(path)} copies other fonts than {rel(FONT_BUILDS[0])}: "
                     "make them copy the same")
        copied = names
    return copied


def fonts(text_id):
    out = []
    copied = built_fonts()
    for name in sorted(copied):
        if name not in FONTS:
            sys.exit(f"gen-licenses: the build copies the font {name}, which has no entry in "
                     "FONTS in this script: add it, with its OFL text in scripts/licenses/fonts")
    for name in FONTS:
        if name not in copied:
            sys.exit(f"gen-licenses: FONTS lists {name}, which the build no longer copies: remove it")
    # The google-fonts the build copies from (devenv.nix sets it), so a
    # nixpkgs bump that changes a font's license or copyright fails here.
    source = os.environ.get("SPOOLFRAME_WEB_FONTS")
    if not source:
        sys.exit("gen-licenses: SPOOLFRAME_WEB_FONTS is not set: run this inside devenv shell")
    for name, (title, ofl) in sorted(FONTS.items(), key=lambda kv: kv[1][0]):
        names = font_names(os.path.join(source, name))
        grant = names.get(13, "") + " " + names.get(14, "")
        if "SIL Open Font License, Version 1.1" not in grant:
            sys.exit(f"gen-licenses: {name} no longer says SIL OFL 1.1 in its name table "
                     f"({names.get(13)!r}): read its license and update the entry")
        text = read(os.path.join(LICENSES_DIR, "fonts", ofl))
        copyright = names[0].strip()
        if text.split("\n", 1)[0].strip() != copyright:
            sys.exit(f"gen-licenses: scripts/licenses/fonts/{ofl} does not start with the font's "
                     f"copyright, {copyright!r}: replace the text")
        version = re.match(r"Version\s+(\S+?)(?:;|$|\s)", names.get(5, ""))
        if not version:
            sys.exit(f"gen-licenses: {name} has no version in its name table")
        out.append({"name": title, "version": version.group(1), "group": "browser", "ships": ["web"],
                    "license": "OFL-1.1", "copyright": [copyright],
                    "texts": {"OFL-1.1": text_id(text)}})
    return out


def fixed_components(text_id):
    """Catppuccin, Lucide, the fonts and Tailwind: what the web app carries
    into the browser besides crates, read from the repo so that --check
    sees drift."""
    out = []

    text = read(os.path.join(LICENSES_DIR, "catppuccin.LICENSE"))
    if "Copyright (c) 2021 Catppuccin" not in text or classify(text) != ["MIT"]:
        sys.exit("gen-licenses: scripts/licenses/catppuccin.LICENSE is no longer MIT, "
                 "Copyright (c) 2021 Catppuccin: update the entry in fixed_components")
    if "Catppuccin" not in read(os.path.join(WEB, "input.css")):
        sys.exit("gen-licenses: crates/spoolframe-web/input.css no longer names Catppuccin: "
                 "if the palette is gone, remove its entry in fixed_components")
    out.append({"name": "Catppuccin", "version": "palette", "group": "browser", "ships": ["web"],
                "license": "MIT", "copyright": holders([text]), "texts": {"MIT": text_id(text)}})

    text = read(os.path.join(LICENSES_DIR, "lucide.LICENSE"))
    # Lucide's file holds ISC for Lucide and MIT for the icons from Feather.
    if classify(text) != ["MIT", "ISC"] or "Lucide Icons and Contributors" not in text:
        sys.exit("gen-licenses: scripts/licenses/lucide.LICENSE is no longer ISC plus Feather's "
                 "MIT: read it and update the entry in fixed_components")
    out.append({"name": "Lucide", "version": "icons", "group": "browser", "ships": ["web"],
                "license": "ISC AND MIT", "copyright": holders([text]),
                "texts": {"ISC": text_id(text), "MIT": text_id(text)}})

    out.extend(fonts(text_id))

    with open(os.path.join(WEB, "assets", "tailwind.css"), encoding="utf-8") as f:
        m = re.match(r"/\*! tailwindcss v(\S+) \| MIT License", f.readline())
    if not m:
        sys.exit("gen-licenses: crates/spoolframe-web/assets/tailwind.css has no Tailwind banner")
    if m.group(1) != TAILWIND:
        sys.exit(f"gen-licenses: the stylesheet is Tailwind {m.group(1)}, the license file is "
                 f"from {TAILWIND}: check it in that release and update TAILWIND")
    text = read(os.path.join(LICENSES_DIR, "tailwindcss.LICENSE"))
    if classify(text) != ["MIT"]:
        sys.exit("gen-licenses: scripts/licenses/tailwindcss.LICENSE is no longer MIT")
    out.append({"name": "Tailwind CSS", "version": m.group(1), "group": "browser", "ships": ["web"],
                "license": "MIT", "copyright": holders([text]), "texts": {"MIT": text_id(text)}})
    return out


def programs(text_id):
    """FFmpeg, libplacebo and Tesseract: what the server runs, as nixpkgs
    builds them. --check fails when the nixpkgs flake.lock pins, or the
    way the build takes them, is no longer what PROGRAMS was read from."""
    with open(os.path.join(ROOT, "flake.lock"), encoding="utf-8") as f:
        lock = json.load(f)
    node = lock["nodes"]["root"]["inputs"]["nixpkgs"]
    pinned = lock["nodes"][node]["locked"].get("rev")
    if pinned != PROGRAMS_NIXPKGS:
        sys.exit(f"gen-licenses: flake.lock pins nixpkgs {pinned}, PROGRAMS was read at "
                 f"{PROGRAMS_NIXPKGS}: check FFmpeg's, libplacebo's and Tesseract's versions and "
                 "licenses there (ffmpeg's configure flags too) and update PROGRAMS_NIXPKGS")
    ffmpeg = read(os.path.join(ROOT, "nix", "ffmpeg.nix"))
    if "ffmpeg-headless.override {withPlacebo = true;}" not in ffmpeg:
        sys.exit("gen-licenses: nix/ffmpeg.nix no longer builds ffmpeg-headless with libplacebo: "
                 "update PROGRAMS")
    if "tesseract.override" not in read(os.path.join(ROOT, "nix", "package.nix")):
        sys.exit("gen-licenses: nix/package.nix no longer runs nixpkgs' tesseract: update PROGRAMS")
    return [{"name": name, "version": version, "group": "programs", "ships": ["server"],
             "license": spdx, "copyright": lines, "texts": {spdx: text_id(standard(spdx))}}
            for name, version, spdx, lines in PROGRAMS]


def generate():
    metadata = json.loads(cargo("metadata", "--locked", "--all-features", "--format-version", "1"))
    workspace = set(metadata["workspace_members"])
    by_key = {}
    for package in metadata["packages"]:
        key = (package["name"], package["version"])
        if key in by_key:
            sys.exit(f"gen-licenses: two packages are {key[0]} {key[1]}")
        by_key[key] = package

    ships = {}
    for target in TARGETS:
        for key in linked(SERVER, target, SERVER_FEATURES):
            ships.setdefault(key, set()).add("server")
    for key in linked(WEB_APP, WEB_TARGET):
        ships.setdefault(key, set()).add("web")

    texts, text_index, components = [], {}, []

    def text_id(text):
        if text not in text_index:
            text_index[text] = len(texts)
            texts.append(text)
        return text_index[text]

    for key in sorted(ships):
        package = by_key[key]
        expression, by_id, lines = crate_texts(package, metadata["workspace_root"], workspace)
        components.append({
            "name": key[0],
            "version": key[1],
            "group": "spoolframe" if package["id"] in workspace else "crates",
            "ships": sorted(ships[key], key=["server", "web"].index),
            "license": expression,
            "copyright": lines,
            "texts": {spdx: text_id(text) for spdx, text in by_id.items()},
        })

    components.extend(fixed_components(text_id))
    components.extend(programs(text_id))

    data = {"components": components, "texts": texts}
    # One entry a line, so a change in Cargo.lock reads as a small diff.
    return json.dumps(data, indent=0, sort_keys=True, ensure_ascii=False) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--check", action="store_true", help="fail if the committed list is stale")
    args = parser.parse_args()
    data = generate()
    if args.check:
        try:
            with open(OUT, encoding="utf-8") as f:
                current = f.read()
        except FileNotFoundError:
            current = None
        if current != data:
            sys.exit(
                f"gen-licenses: {rel(OUT)} is out of date with Cargo.lock or the web app's "
                "assets: run scripts/gen-licenses.py in devenv shell and commit it"
            )
        return
    with open(OUT, "w", encoding="utf-8") as f:
        f.write(data)
    summary = {}
    for c in json.loads(data)["components"]:
        summary[c["group"]] = summary.get(c["group"], 0) + 1
    print(f"wrote {rel(OUT)}: {summary}, {len(json.loads(data)['texts'])} texts")


if __name__ == "__main__":
    main()
