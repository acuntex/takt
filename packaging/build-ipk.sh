#!/usr/bin/env bash
#
# Builds dist/com.github.acuntex.takt_<version>_all.ipk from the release ARMv7 daemon plus the
# webOS app wrapper, native service, LS2 dev-mode role files, and init.d entrypoint.
#
# The IPK is a plain ar archive with debian-binary, control.tar.gz and data.tar.gz.
# No rootfs paths are written. Staging happens under dist/ and is removed afterwards.
#
# Environment:
#   TAKT_BIN      prebuilt ARMv7 binary (defaults to cargo zigbuild release output)
#   VERSION       package version (default 0.1.0)
#   TARGET        Rust target (default armv7-unknown-linux-gnueabi)
#   CARGO         cargo executable (default cargo)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PACKAGE_ID="com.github.acuntex.takt"
VERSION="${VERSION:-0.1.0}"
TARGET="${TARGET:-armv7-unknown-linux-gnueabi}"
CARGO="${CARGO:-cargo}"

# cargo-zigbuild needs a `zig` executable; allow a local unpacked toolchain.
if ! command -v zig >/dev/null 2>&1; then
  for candidate in "$HOME/zig/zig" /usr/local/bin/zig /opt/zig/zig; do
    if [[ -x "$candidate" ]]; then
      zig_dir="$(dirname "$candidate")"
      export PATH="$zig_dir:$PATH"
      break
    fi
  done
fi

DIST_DIR="$REPO_ROOT/dist"
BUILD_DIR="$DIST_DIR/.build-${PACKAGE_ID}"
IPK="$DIST_DIR/${PACKAGE_ID}_${VERSION}_all.ipk"
# IPK data is rooted at /media/developer/apps: use usr/palm/... (like webOS IPKs).
APP_DIR="$BUILD_DIR/data/usr/palm/applications/$PACKAGE_ID"
SERVICE_DIR="$BUILD_DIR/data/usr/palm/services/$PACKAGE_ID.service"
PACKAGES_DIR="$BUILD_DIR/data/usr/palm/packages/$PACKAGE_ID"
# appInstallService only extracts under /media/developer/apps on webOS. Keep the LS2
# dev-mode files and init.d entrypoint inside the service directory; postinst copies them
# into /var when it runs, and scripts/deploy.sh does the same explicitly.
LS2_DIR="$SERVICE_DIR/ls2"
INITD_DIR="$SERVICE_DIR/init.d"
INITD_NAME="90-start_takt"

if [[ -n "${TAKT_BIN:-}" ]]; then
  BIN="$TAKT_BIN"
  if [[ ! -x "$BIN" ]]; then
    echo ">> FAIL: TAKT_BIN is not executable: $BIN" >&2
    exit 1
  fi
else
  # Always build: a stale target binary must never be packaged silently. Cargo is incremental.
  BIN="$REPO_ROOT/daemon/target/$TARGET/release/takt"
  echo ">> building release ARMv7 binary ($TARGET, glibc 2.24) ..." >&2
  (
    cd "$REPO_ROOT/daemon"
    if "$CARGO" zigbuild --help >/dev/null 2>&1; then
      "$CARGO" zigbuild --release --target "$TARGET.2.24"
    else
      "$CARGO" build --release --target "$TARGET"
    fi
  )
fi
[[ -x "$BIN" ]] || {
  echo ">> FAIL: binary not found or not executable: $BIN" >&2
  exit 1
}

rm -rf "$BUILD_DIR"
mkdir -p "$APP_DIR" "$SERVICE_DIR" "$LS2_DIR" "$INITD_DIR" "$PACKAGES_DIR" "$BUILD_DIR/control"

# --- control archive ---
printf '2.0\n' >"$BUILD_DIR/debian-binary"
cat >"$BUILD_DIR/control/control" <<EOF
Package: $PACKAGE_ID
Version: $VERSION
Architecture: all
Maintainer: takt-webos
Section: misc
Priority: optional
Installed-Size: $(du -sk "$BUILD_DIR/data" | cut -f1)
Description: Takt — Music Assistant client for LG webOS TVs
 Web-UI wrapper for the local Takt daemon plus the native Luna service.
webOS-Package-Format-Version: 2
webOS-Packager-Version: x.y.x
EOF
chmod 0644 "$BUILD_DIR/control/control"

# --- webOS package metadata ---
cat >"$PACKAGES_DIR/packageinfo.json" <<EOF
{
  "id": "$PACKAGE_ID",
  "version": "$VERSION",
  "app": "$PACKAGE_ID"
}
EOF

# --- web app wrapper ---
install -m 0644 "$REPO_ROOT/packaging/appinfo.json" "$APP_DIR/appinfo.json"
install -m 0644 "$REPO_ROOT/packaging/app-index.html" "$APP_DIR/index.html"
install -m 0644 "$REPO_ROOT/packaging/icon.png" "$APP_DIR/icon.png"
install -m 0644 "$REPO_ROOT/packaging/largeIcon.png" "$APP_DIR/largeIcon.png"
install -m 0755 "$REPO_ROOT/packaging/setup.sh" "$SERVICE_DIR/setup.sh"

# --- native service ---
install -m 0644 "$REPO_ROOT/packaging/services.json" "$SERVICE_DIR/services.json"
install -m 0755 "$REPO_ROOT/packaging/start.sh" "$SERVICE_DIR/start.sh"
install -m 0755 "$REPO_ROOT/packaging/init.d/$INITD_NAME" "$SERVICE_DIR/$INITD_NAME"
install -m 0755 "$BIN" "$SERVICE_DIR/takt"

# Strip the staged copy only; never modify TAKT_BIN. Rust release builds are already
# optimized; stripping keeps the IPK small and removes build-host paths from symbols.
if command -v readelf >/dev/null 2>&1 &&
  ! LC_ALL=C readelf -S "$SERVICE_DIR/takt" | grep -q '\.symtab'; then
  echo ">> binary is already stripped" >&2
elif command -v arm-linux-gnueabi-strip >/dev/null 2>&1; then
  arm-linux-gnueabi-strip --strip-unneeded "$SERVICE_DIR/takt"
elif command -v arm-linux-gnueabihf-strip >/dev/null 2>&1; then
  arm-linux-gnueabihf-strip --strip-unneeded "$SERVICE_DIR/takt"
elif command -v strip >/dev/null 2>&1 &&
  strip --strip-unneeded "$SERVICE_DIR/takt" 2>/dev/null; then
  :
else
  echo ">> warning: no ARM-capable strip found; shipping the release binary as-is" >&2
fi

# --- LS2 dev-mode role/manifest/service files (Task 9 requirement) ---
ls2_copy() {
  local relative="$1"
  mkdir -p "$LS2_DIR/$(dirname "$relative")"
  install -m 0644 "$REPO_ROOT/packaging/ls2/$relative" "$LS2_DIR/$relative"
}
for file in "$REPO_ROOT"/packaging/ls2/*/*; do
  ls2_copy "${file#"$REPO_ROOT"/packaging/ls2/}"
done

# --- init.d entrypoint stays inside the service dir; postinst/deploy links it ---
install -m 0755 "$REPO_ROOT/packaging/init.d/$INITD_NAME" "$INITD_DIR/$INITD_NAME"

# --- validate staged binary before writing the final artifact ---
binary_machine="unknown"
if command -v readelf >/dev/null 2>&1; then
  binary_machine="$(LC_ALL=C readelf -h "$SERVICE_DIR/takt" | awk '/Machine:/{print $2}')"
fi
if [[ "$binary_machine" != "ARM" && "$binary_machine" != "unknown" ]]; then
  echo ">> FAIL: staged binary is not ARM (readelf Machine: $binary_machine)" >&2
  exit 1
fi

# --- assemble ---
# The control file is created before staging; refresh the size after all payload files exist.
sed -i "s/^Installed-Size:.*/Installed-Size: $(du -sb "$BUILD_DIR/data" | cut -f1)/" \
  "$BUILD_DIR/control/control"
tar --owner=0 --group=0 --numeric-owner -czf "$BUILD_DIR/control.tar.gz" \
  -C "$BUILD_DIR/control" control
tar --owner=0 --group=0 --numeric-owner -czf "$BUILD_DIR/data.tar.gz" -C "$BUILD_DIR/data" usr
mkdir -p "$DIST_DIR"
rm -f "$IPK"
(cd "$BUILD_DIR" && ar rcsD "$IPK" debian-binary control.tar.gz data.tar.gz)
rm -rf "$BUILD_DIR"

echo ">> built $IPK" >&2
echo ">> daemon binary: $BIN ($binary_machine)" >&2
