#!/usr/bin/env sh
#
# Generates the release manifest and the Homebrew Channel index for a tag.
#
# Run before `gh release create`: it reads the IPK the Channel will serve, validates the tag, the
# file name and the package inside the archive, and writes both outputs from them. Reads the package
# id and version from packaging/appinfo.json.
#
# Usage: packaging/gen-repo.sh <tag> [ipk]
#   <tag>  the release tag, e.g. v0.1.0; it must match v<appinfo version>
#   [ipk]  the Channel IPK (default dist/channel/<id>_<version>_all.ipk)
#
# Writes dist/<id>.manifest.json and repo/apps.json. Nothing is written when a validation fails.

set -eu

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

[ $# -ge 1 ] || { echo "usage: packaging/gen-repo.sh <tag> [ipk]" >&2; exit 2; }

TAG="$1"
ID="$(python3 -c 'import json;print(json.load(open("packaging/appinfo.json"))["id"])')"
VERSION="$(python3 -c 'import json;print(json.load(open("packaging/appinfo.json"))["version"])')"
IPK="${2:-dist/channel/${ID}_${VERSION}_all.ipk}"
MANIFEST="dist/${ID}.manifest.json"

[ "$TAG" = "v$VERSION" ] || {
  echo ">> FAIL: tag $TAG does not match packaging/appinfo.json ($VERSION)" >&2
  exit 1
}
[ -f "$IPK" ] || { echo ">> FAIL: no IPK at $IPK (build the release first)" >&2; exit 1; }
[ "$(basename "$IPK")" = "${ID}_${VERSION}_all.ipk" ] || {
  echo ">> FAIL: $(basename "$IPK") is not ${ID}_${VERSION}_all.ipk" >&2
  exit 1
}

# Both builders write the same file name, so the path distinguishes them. This is a path check, not
# proof of the builder: a copy of the maintainer IPK placed here would still pass it.
[ -L "$IPK" ] && { echo ">> FAIL: $IPK is a symlink; publish the file the builder wrote" >&2; exit 1; }
IPK_ABS="$(cd "$(dirname "$IPK")" && pwd)/$(basename "$IPK")"
case "$IPK_ABS" in
  "$REPO_ROOT"/dist/channel/*) ;;
  *)
    echo ">> FAIL: $IPK is not under dist/channel/ — that is the maintainer IPK, not the Channel artifact" >&2
    exit 1
    ;;
esac

DUPLICATES="$(ar t "$IPK" 2>/dev/null | sort | uniq -d)"
[ -z "$DUPLICATES" ] || {
  echo ">> FAIL: $IPK carries duplicate archive members ($DUPLICATES)" >&2
  exit 1
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
ar p "$IPK" control.tar.gz >"$WORK/control.tar.gz" 2>/dev/null || {
  echo ">> FAIL: $IPK has no control.tar.gz" >&2
  exit 1
}
ar p "$IPK" data.tar.gz >"$WORK/data.tar.gz" 2>/dev/null || {
  echo ">> FAIL: $IPK has no data.tar.gz" >&2
  exit 1
}

mkdir -p repo
python3 - "$IPK" "$WORK/control.tar.gz" "$WORK/data.tar.gz" "$TAG" "$MANIFEST" repo/apps.json <<'PY'
import hashlib
import io
import json
import sys
import tarfile

ipk_path, control_path, data_path, tag, manifest_out, index_out = sys.argv[1:7]

with open("packaging/appinfo.json") as handle:
    appinfo = json.load(handle)

package_id = appinfo["id"]
version = appinfo["version"]


def member_bytes(archive, suffix, exact_name=None):
    """The one member ending in `suffix` (or named exactly) — not merely the first, because a decoy
    earlier in the archive would be validated while extraction uses another file."""
    found = []
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar.getmembers():
            if not member.isfile():
                continue
            name = member.name.rstrip("/")
            if (exact_name is not None and name != exact_name) or (
                exact_name is None and not name.endswith(suffix)
            ):
                continue
            found.append(tar.extractfile(member).read())
    if len(found) != 1:
        raise SystemExit(
            ">> FAIL: expected one %s in %s, found %d" % (exact_name or suffix, archive, len(found))
        )
    return found[0]


data_appinfo = member_bytes(
    data_path,
    "usr/palm/applications/%s/appinfo.json" % package_id,
    exact_name="usr/palm/applications/%s/appinfo.json" % package_id,
)
packaged = json.loads(data_appinfo)
if packaged.get("id") != package_id or packaged.get("version") != version:
    raise SystemExit(
        ">> FAIL: the IPK is %s %s, expected %s %s"
        % (packaged.get("id"), packaged.get("version"), package_id, version)
    )

control = member_bytes(control_path, "control", exact_name="control")
installed_size = None
for line in control.decode("utf-8", "replace").splitlines():
    if line.startswith("Installed-Size:"):
        installed_size = int(line.split(":", 1)[1].strip())
if installed_size is None:
    raise SystemExit(">> FAIL: the control file has no Installed-Size")

with open(ipk_path, "rb") as handle:
    raw = handle.read()

ipk_name = ipk_path.rsplit("/", 1)[-1]
base_url = "https://github.com/acuntex/takt/releases/download/%s" % tag
manifest = {
    "id": package_id,
    "version": version,
    "type": appinfo["type"],
    "title": appinfo["title"],
    "appDescription": appinfo["appDescription"],
    "iconUri": "https://raw.githubusercontent.com/acuntex/takt/%s/docs/assets/icon.png" % tag,
    "sourceUrl": "https://github.com/acuntex/takt",
    "rootRequired": True,
    "ipkUrl": "%s/%s" % (base_url, ipk_name),
    "ipkHash": {"sha256": hashlib.sha256(raw).hexdigest()},
    "ipkSize": len(raw),
    "installedSize": installed_size,
}

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
            "id": package_id,
            "title": manifest["title"],
            "iconUri": manifest["iconUri"],
            "manifestUrl": "%s/%s.manifest.json" % (base_url, package_id),
            "manifest": manifest,
            "pool": "main",
            "requirements": {"webosRelease": ">=4.0, <5.0"},
            "shortDescription": manifest["appDescription"][:80],
        }
    ],
}

for path, document in ((manifest_out, manifest), (index_out, index)):
    with open(path, "w") as handle:
        json.dump(document, handle, indent=2)
        handle.write("\n")

print("wrote %s and %s for %s %s" % (manifest_out, index_out, package_id, version))
PY
