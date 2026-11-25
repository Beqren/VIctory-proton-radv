#!/bin/sh
# Rebuild the patched Proton ("viewtree") from Valve's installed Proton Experimental.
#
# The copy runs Valve's Proton with one change: its wineserver keeps memory views in an rb-tree
# (Wine bug 60868, CachyOS/wine-cachyos pull request 50), so a page remap no longer walks a list of
# the process's ~125,000 views. The copy goes stale when Valve updates Proton Experimental, and a
# wineserver built for one Proton must never be put into another: run this script after each update.
#
# Steps: read the installed Proton's tag from its `version` file; find that tag's `wine` submodule
# commit (GitHub API); fetch only that commit into a fresh source directory; check the patch's
# sha256 and apply it (if it does not apply cleanly the patch has to be ported by hand: the script
# stops); run Wine's generation steps and configure; build only server/wineserver; check that the
# generated protocol version is the one the installed ntdll.so compares against (STOP if not);
# stage a reflink copy of Proton with Valve's server kept as files/bin/wineserver.stock; then, under
# the Proton lock, replace the old copy; make sure the prefix has Microsoft's d3dcompiler_47.dll.
# Nothing under Valve's install is touched, no sudo. A failed build leaves the old copy in place.
# Safe to run twice. Details and measurements: docs/proton.md and <data>/wine-viewtree/NOTES.md.
# Needs in $WORK: patch/pr50.patch (the published patch, sha256 below). Copy of <data>/wine-viewtree/rebuild.sh.
#
# usage: rebuild.sh        (settings below can be overridden from the environment)
set -eu

DATA=${DATA:-$HOME/data}                                # the data directory (outside the repository)
WORK=${WORK:-$DATA/wine-viewtree}                       # sources, build tree, patch
PATCH=${PATCH:-$WORK/patch/pr50.patch}
PATCH_SHA256=${PATCH_SHA256:-9ee40d566e17602b86d5a98cc26823244b5252718e636d037047154b9a0da74b}
VALVE=${VALVE:-$HOME/.local/share/Steam/steamapps/common/Proton - Experimental}   # Valve's install: read only
COPY=${COPY:-$HOME/.local/share/Steam/compatibilitytools.d/Proton-Experimental-viewtree}
PREFIX=${PREFIX:-$DATA/prefix-viewtree}                 # the copy's prefix (created by Proton on first run)
PREFIX_SRC=${PREFIX_SRC:-$DATA/prefix-experimental}     # a prefix that has Microsoft's d3dcompiler_47.dll
LOCK=${LOCK:-$DATA/.proton.lock}                        # the lock tools/vx.sh takes
LOCK_WAIT=${LOCK_WAIT:-0}                               # seconds to wait for it (0: fail at once)
WINE_URL=${WINE_URL:-https://github.com/ValveSoftware/wine.git}
PROTON_URL=${PROTON_URL:-https://github.com/ValveSoftware/Proton.git}
API=${API:-https://api.github.com/repos/ValveSoftware/Proton/contents/wine}
JOBS=${JOBS:-$(nproc)}

die() { echo "rebuild.sh: $*" >&2; exit 1; }
say() { echo "== $*"; }

[ "$(id -u)" -ne 0 ] || die "do not run as root"
[ -f "$VALVE/version" ] || die "no Proton Experimental at '$VALVE'"
[ -f "$PATCH" ] || die "no patch at $PATCH"
[ -f "$VALVE/files/lib/wine/x86_64-unix/ntdll.so" ] || die "no ntdll.so in '$VALVE'"
STOCK_SERVER="$VALVE/files/bin/wineserver"
[ -x "$STOCK_SERVER" ] || die "no wineserver in '$VALVE'"

# 1. the tag: the version file reads "<unix time> <tag>-x86_64"
TAG=$(awk '{print $2}' "$VALVE/version"); TAG=${TAG%-x86_64}
case $TAG in experimental-*) ;; *) die "unexpected tag '$TAG' from '$VALVE/version'" ;; esac
say "Valve's Proton Experimental: $TAG"

# 2. the tag's wine submodule commit
WINE_COMMIT=$(curl -fsS -H 'Accept: application/vnd.github+json' "$API?ref=$TAG" |
  python3 -I -c 'import json,sys; d=json.load(sys.stdin); print(d["sha"] if d.get("type")=="submodule" else "")') ||
  die "GitHub API call failed ($API?ref=$TAG)"
printf '%s' "$WINE_COMMIT" | grep -Eq '^[0-9a-f]{40}$' || die "no wine submodule commit found for $TAG"
PROTON_COMMIT=$(git ls-remote "$PROTON_URL" "refs/tags/$TAG" | awk '{print $1; exit}')
say "wine commit $WINE_COMMIT (Proton tag commit ${PROTON_COMMIT:-unknown})"

# 3. patch check, fresh source with one shallow fetch
echo "$PATCH_SHA256  $PATCH" | sha256sum -c --quiet - || die "patch sha256 differs from $PATCH_SHA256"
SRC=$WORK/src; BUILD=$WORK/build
rm -rf "$SRC" "$BUILD"
git init -q "$SRC"
git -C "$SRC" remote add origin "$WINE_URL"
git -C "$SRC" fetch -q --depth 1 origin "$WINE_COMMIT"
git -C "$SRC" checkout -q FETCH_HEAD
[ "$(git -C "$SRC" rev-parse HEAD)" = "$WINE_COMMIT" ] || die "fetched commit is not $WINE_COMMIT"
git -C "$SRC" apply --check "$PATCH" 2>/dev/null ||
  die "the patch does not apply cleanly to wine $WINE_COMMIT ($TAG): someone has to port $PATCH (see NOTES.md)"
git -C "$SRC" apply "$PATCH"
say "patch applied"

# 4. Valve's tree lacks generated files; these are Wine's own autogen.sh steps
(cd "$SRC" && dlls/winevulkan/make_vulkan -x vk.xml -X video.xml && tools/make_specfiles &&
  tools/make_requests && autoreconf -if && rm -rf autom4te.cache) > "$WORK/generate.log" 2>&1 ||
  die "generation steps failed, see $WORK/generate.log"
mkdir -p "$BUILD"
(cd "$BUILD" && "$SRC/configure" --enable-win64 --without-mingw --disable-tests --without-x --without-freetype \
  --without-gstreamer --without-wayland --without-vulkan --without-opengl --without-alsa --without-pulse \
  --without-cups --without-sane --without-gphoto --without-krb5 --without-netapi --without-pcap --without-usb \
  --without-v4l2 --without-capi --without-oss --without-sdl --without-udev --without-unwind --without-dbus \
  --without-gnutls --without-ffmpeg --without-fontconfig --without-gssapi --without-pcsclite \
  --without-xinerama --without-osmesa) > "$WORK/configure.log" 2>&1 || die "configure failed, see $WORK/configure.log"
make -C "$BUILD" -j"$JOBS" server/wineserver > "$WORK/make.log" 2>&1 || die "build failed, see $WORK/make.log"
NEW_SERVER=$BUILD/server/wineserver
[ -x "$NEW_SERVER" ] || die "no $NEW_SERVER after the build"
say "built $NEW_SERVER"

# 5. protocol version: the server's must equal what Valve's ntdll.so compares against
V=$(sed -n 's/^#define SERVER_PROTOCOL_VERSION //p' "$SRC/include/wine/server_protocol.h")
[ -n "$V" ] || die "no SERVER_PROTOCOL_VERSION in the generated header"
HITS=$(objdump -d --no-show-raw-insn "$VALVE/files/lib/wine/x86_64-unix/ntdll.so" | grep -F -c "\$0x$(printf %x "$V"),") || true
[ "${HITS:-0}" -gt 0 ] || die "STOP: protocol version $V from the source is not what the installed ntdll.so compares against (0 hits): the tree is not the one this Proton was built from. Nothing was changed."
say "protocol version $V matches the installed ntdll.so ($HITS hits)"

# 6. stage the copy beside the old one (reflinks), with the new server in it
STAGE="$(dirname "$COPY")/.$(basename "$COPY").new"
rm -rf "$STAGE"
trap 'rm -rf "$STAGE"' EXIT
cp -a --reflink=auto "$VALVE" "$STAGE"
mv "$STAGE/files/bin/wineserver" "$STAGE/files/bin/wineserver.stock"
install -m 755 "$NEW_SERVER" "$STAGE/files/bin/wineserver"
cmp -s "$STOCK_SERVER" "$STAGE/files/bin/wineserver.stock" || die "staged stock server differs from Valve's"
if cmp -s "$STOCK_SERVER" "$STAGE/files/bin/wineserver"; then die "staged server is Valve's own"; fi

# 7. replace the copy and fix the prefix, under the Proton lock
exec 8>"$LOCK"
flock -w "$LOCK_WAIT" 8 || die "a Proton job holds $LOCK; the old copy is untouched. Try again when it ends (LOCK_WAIT=<seconds> waits for it)."
# a wineserver lingers a few seconds after a job: wait for processes running from the copy (by exe)
in_use() { for e in /proc/[0-9]*/exe; do case $(readlink "$e" 2>/dev/null) in "$COPY"/*) return 0 ;; esac; done; return 1; }
n=0
while in_use; do
  n=$((n + 1)); [ "$n" -le 30 ] || die "a process is still running from $COPY; the old copy is untouched"
  sleep 2
done
rm -rf "$COPY"
mv "$STAGE" "$COPY"
trap - EXIT
say "copy installed at $COPY"

S=$PREFIX/pfx/drive_c/windows/system32
if [ ! -d "$S" ]; then
  say "prefix $PREFIX does not exist yet: run one job with the copy so Proton creates it, then run this script again for d3dcompiler_47.dll"
else
  SRC_DLL=$PREFIX_SRC/pfx/drive_c/windows/system32/d3dcompiler_47.dll
  [ -f "$SRC_DLL" ] || die "no $SRC_DLL to copy Microsoft's d3dcompiler_47.dll from"
  if ! cmp -s "$SRC_DLL" "$S/d3dcompiler_47.dll"; then
    if [ -f "$S/d3dcompiler_47.dll" ] && [ ! -e "$S/d3dcompiler_47.dll.wine-builtin" ]; then
      mv "$S/d3dcompiler_47.dll" "$S/d3dcompiler_47.dll.wine-builtin"
    fi
    cp "$SRC_DLL" "$S/d3dcompiler_47.dll"
    say "copied Microsoft's d3dcompiler_47.dll into the prefix"
  else
    say "prefix already has Microsoft's d3dcompiler_47.dll"
  fi
fi

echo
echo "Built: viewtree wineserver for $TAG"
echo "  Proton tag commit  ${PROTON_COMMIT:-unknown}"
echo "  wine commit        $WINE_COMMIT"
echo "  patch              $(basename "$PATCH") sha256 $PATCH_SHA256"
echo "  protocol version   $V (ntdll.so hits: $HITS)"
echo "  wineserver         sha256 $(sha256sum "$COPY/files/bin/wineserver" | cut -d' ' -f1)"
echo "  wineserver.stock   sha256 $(sha256sum "$COPY/files/bin/wineserver.stock" | cut -d' ' -f1)"
echo "  copy               $COPY (version: $(cat "$COPY/version"))"
echo "  prefix             $PREFIX"
