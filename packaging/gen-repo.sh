#!/usr/bin/env sh
#
# Builds repo/apps.json, the index the Homebrew Channel reads for this repository.
#
# Run after a release exists: it embeds the release manifest and the IPK checksum, so the Channel
# installs exactly the published file. Reads the package id and version from packaging/appinfo.json.
#
# Usage: packaging/gen-repo.sh [dist/…ipk] [dist/…manifest.json]

set -eu

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

ID="$(python3 -c 'import json;print(json.load(open("packaging/appinfo.json"))["id"])')"
VERSION="$(python3 -c 'import json;print(json.load(open("packaging/appinfo.json"))["version"])')"
IPK="${1:-dist/${ID}_${VERSION}_all.ipk}"
MANIFEST="${2:-dist/${ID}.manifest.json}"

[ -f "$IPK" ] || { echo "no IPK at $IPK" >&2; exit 1; }
[ -f "$MANIFEST" ] || { echo "no manifest at $MANIFEST (build the release first)" >&2; exit 1; }

mkdir -p repo
python3 - "$IPK" "$MANIFEST" repo/apps.json <<'PY'
import hashlib
import json
import sys

ipk_path, manifest_path, out_path = sys.argv[1:4]
with open(ipk_path, "rb") as handle:
    data = handle.read()
with open(manifest_path) as handle:
    manifest = json.load(handle)

manifest["ipkSize"] = len(data)
manifest["ipkHash"] = {"sha256": hashlib.sha256(data).hexdigest()}
ipk_url = manifest["ipkUrl"]
manifest_url = ipk_url.rsplit("/", 1)[0] + "/" + manifest["id"] + ".manifest.json"

index = {
    "paging": {
        "page": 1,
        "count": 1,
        "maxPage": 1,
        "itemsTotal": 1,
        "prevUrl": None,
        "nextUrl": None,
    },
    "packages": [
        {
            "id": manifest["id"],
            "title": manifest["title"],
            "iconUri": manifest["iconUri"],
            "manifestUrl": manifest_url,
            "manifest": manifest,
            "pool": "main",
            "requirements": {"webosRelease": ">=4.0, <5.0"},
            "shortDescription": manifest.get("appDescription", "")[:80],
        }
    ],
}

with open(out_path, "w") as handle:
    json.dump(index, handle, indent=2)
    handle.write("\n")
print(f"wrote {out_path} for {manifest['id']} {manifest['version']}")
PY
