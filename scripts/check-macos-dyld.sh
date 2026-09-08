#!/bin/bash
# Fail the build if the macOS binary link-time-references anything newer than the
# deployment floor (10.15). dyld binds every non-weak undefined symbol before main(),
# so ONE such reference makes the whole app fail to launch on Catalina — a load bug,
# not a missing feature.
#
#   ./scripts/check-macos-dyld.sh <binary> <sdk-path> [options]
#   ./scripts/check-macos-dyld.sh src-tauri/target/release/ScreenExtend \
#       /Library/Developer/CommandLineTools/SDKs/MacOSX10.15.sdk
#
# Options:
#   --allowlist <file>  default scripts/macos-dyld-allowlist.txt
#   --src <dir>         Rust source to scan for runtime-resolved names
#                       (default <repo>/src-tauri/src); "" disables
#   --floor <x.y>       deployment floor (default 10.15)
#   --no-source-scan    skip the dlsym/AnyClass::get report
#   --quiet             only print offenders and the verdict
#
# Exit 0 = clean, 1 = at least one offender, 2 = usage/environment error.
#
# A symbol is an offender when it is undefined, strongly bound (not weak), and
# either absent from the SDK's .tbd stubs or annotated in the SDK headers as
# introduced after the floor. The .tbd stubs are the authoritative test: they are
# generated from the dylibs that shipped with that SDK, so "in the stub" == "the
# 10.15 dyld can bind it". The INTRO column is diagnostic only — it is scraped
# from the availability macro nearest the declaration, which can misattribute
# when several declarations share a 3-line window. Never relax the stub verdict
# because INTRO looks old.

set -uo pipefail

FLOOR="10.15"
ALLOWLIST=""
SRC_DIR=""
SRC_SCAN=1
QUIET=0
BIN=""
SDK=""

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

die() { echo "check-macos-dyld: $*" >&2; exit 2; }
say() { [ "$QUIET" -eq 1 ] || echo "$@"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --allowlist) ALLOWLIST="${2:-}"; shift 2 ;;
    --src) SRC_DIR="${2:-}"; SRC_SCAN=1; shift 2 ;;
    --floor) FLOOR="${2:-}"; shift 2 ;;
    --no-source-scan) SRC_SCAN=0; shift ;;
    --quiet) QUIET=1; shift ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    -*) die "unknown option $1" ;;
    *)
      if [ -z "$BIN" ]; then BIN="$1"
      elif [ -z "$SDK" ]; then SDK="$1"
      else die "unexpected argument $1"; fi
      shift ;;
  esac
done

[ -n "$BIN" ] && [ -n "$SDK" ] || die "usage: $0 <binary> <sdk-path> [options]"
[ -f "$BIN" ] || die "no such binary: $BIN"
[ -d "$SDK" ] || die "no such SDK: $SDK"
command -v nm >/dev/null 2>&1 || die "nm not found (install the Command Line Tools)"
command -v otool >/dev/null 2>&1 || die "otool not found (install the Command Line Tools)"

if [ -z "$ALLOWLIST" ]; then ALLOWLIST="$SCRIPT_DIR/macos-dyld-allowlist.txt"; fi
if [ -z "$SRC_DIR" ] && [ "$SRC_SCAN" -eq 1 ]; then SRC_DIR="$REPO_ROOT/src-tauri/src"; fi

FLOOR_MAJ="${FLOOR%%.*}"
FLOOR_MIN="${FLOOR#*.}"; FLOOR_MIN="${FLOOR_MIN%%.*}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/macos-dyld.XXXXXX")" || die "mktemp failed"
trap 'rm -rf "$WORK"' EXIT

# ---------------------------------------------------------------- binary facts
say "== binary =="
say "  $BIN"
say "  $(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
if [ "$QUIET" -eq 0 ]; then
  otool -l "$BIN" | grep -A4 -E 'LC_BUILD_VERSION|LC_VERSION_MIN_MACOSX' \
    | grep -E 'cmd |minos|sdk|version' | sed 's/^/  /'
  echo
  echo "== otool -L (dynamic libraries) =="
  otool -L "$BIN" | sed 's/^/  /'
  echo
fi

# ------------------------------------------------- SDK export index (.tbd stubs)
SDK_KEY="$(echo "$SDK" | tr -c 'A-Za-z0-9' '_')"
INDEX_CACHE="${TMPDIR:-/tmp}/macos-dyld-sdkindex${SDK_KEY}.txt"
if [ ! -s "$INDEX_CACHE" ]; then
  say "building SDK export index from .tbd stubs (first run only, ~30s)..."
  # A .tbd lists plain C symbols under `symbols:`/`weak-symbols:` but Objective-C
  # classes under a separate `objc-classes:` key, as BARE names — so the class-ref
  # symbols the linker actually emits (_OBJC_CLASS_$_NSString) have to be
  # reconstructed, or every ObjC class in the app looks absent from the SDK.
  find "$SDK/System/Library" "$SDK/usr/lib" -name '*.tbd' -print0 2>/dev/null \
    | xargs -0 cat 2>/dev/null \
    | awk '
        function emit(tok) {
          gsub(/[][,'"'"']/, "", tok); gsub(/^[ \t]+|[ \t]+$/, "", tok)
          if (tok == "") return
          if (key == "objc-classes") {
            print "_OBJC_CLASS_$_" tok; print "_OBJC_METACLASS_$_" tok
          } else if (key == "objc-eh-types") {
            print "_OBJC_EHTYPE_$_" tok
          } else if (key == "objc-ivars") {
            print "_OBJC_IVAR_$_" tok
          } else if (substr(tok, 1, 1) == "_" || substr(tok, 1, 4) == "$ld$" ||
                     tok == "dyld_stub_binder") {
            # dyld_stub_binder is the one exported symbol with no leading
            # underscore; every binary references it, so dropping it would make
            # every binary fail the gate.
            print tok
          }
        }
        {
          line = $0
          if (match(line, /(weak-)?symbols:|objc-classes:|objc-eh-types:|objc-ivars:/)) {
            k = substr(line, RSTART, RLENGTH); sub(/:$/, "", k); sub(/^weak-/, "", k)
            key = k
            line = substr(line, RSTART + RLENGTH)
            open = 1
          }
          if (!open) next
          n = split(line, tok, /[ \t]+/)
          for (i = 1; i <= n; i++) emit(tok[i])
          if (index(line, "]") > 0) open = 0
        }
      ' \
    | sort -u > "$INDEX_CACHE.tmp" && mv "$INDEX_CACHE.tmp" "$INDEX_CACHE"
fi
[ -s "$INDEX_CACHE" ] || die "SDK export index is empty — is $SDK a real SDK?"
say "SDK export index: $(wc -l < "$INDEX_CACHE" | tr -d ' ') symbols ($INDEX_CACHE)"
say ""

# --------------------------------------------------------- undefined symbols
# `nm -m` marks weak imports; a weak undefined symbol binds to 0 instead of
# aborting the load, so it is never a load bug.
nm -m -arch all "$BIN" 2>/dev/null > "$WORK/nm-m.txt" || nm -m "$BIN" > "$WORK/nm-m.txt"
[ -s "$WORK/nm-m.txt" ] || die "nm produced no output for $BIN"

awk '
  /\(undefined\)/ {
    # "(undefined) weak external _foo" — must not be confused with a mangled Rust
    # name that merely contains "Weak" (alloc::sync::Weak<T> and friends).
    weak = ($0 ~ /\(undefined\)[ \t]+weak/) ? 1 : 0
    lib = "?"
    if (match($0, /\(from [^)]+\)/)) lib = substr($0, RSTART + 6, RLENGTH - 7)
    # "(undefined) [weak] external <symbol> [(from <lib>)]" — take the field after
    # "external" rather than the first underscore-prefixed one, so a symbol without
    # the usual leading underscore (dyld_stub_binder) is not silently dropped.
    sym = ""
    for (i = 1; i < NF; i++) if ($i == "external") { sym = $(i + 1); break }
    if (sym == "" || sym ~ /^\(/) next
    print sym "\t" weak "\t" lib
  }
' "$WORK/nm-m.txt" | sort -u > "$WORK/undef-raw.txt"

# collapse per-arch duplicates: strong wins over weak (strong binding is the risk)
awk -F'\t' '
  { if (!($1 in w) || $2 == "0") { w[$1] = $2; l[$1] = $3 } }
  END { for (s in w) print s "\t" w[s] "\t" l[s] }
' "$WORK/undef-raw.txt" | sort > "$WORK/undef.txt"

TOTAL=$(wc -l < "$WORK/undef.txt" | tr -d ' ')

# Names without the leading underscore, for the header grep. Restricted to symbols
# that come from a framework (or look like a framework/os symbol) — feeding grep the
# whole libSystem C runtime (memcpy, malloc, ...) matches millions of header lines for
# no benefit: everything in libc has been there since long before the floor.
awk -F'\t' '
  {
    sym = $1; lib = $3
    bare = sym; sub(/^_/, "", bare)
    if (bare !~ /^[A-Za-z_][A-Za-z0-9_$]*$/) next
    frameworkish = (lib !~ /^(libSystem|libc\+\+|libobjc|libz|libiconv|libresolv|libcharset|libcompression|libbz2|libxml2|libsqlite3)/)
    if (frameworkish || bare ~ /^(os_|OBJC_CLASS_\$_|OBJC_METACLASS_\$_)/) print bare
  }
' "$WORK/undef.txt" | sed -e 's/^OBJC_CLASS_\$_//' -e 's/^OBJC_METACLASS_\$_//' \
  | sort -u > "$WORK/names.txt"

# ------------------------------------------------------------ availability scan
# Anchor on the availability MACRO, not on the symbol names: grepping the headers
# for names like NSString or NSObject matches millions of lines, while the set of
# lines that carry an availability annotation is small and cacheable per SDK. The
# annotation usually trails the declaration or sits on the next line, so keep two
# lines of leading context and stitch a 3-line window back together.
ANNOT_RE='API_AVAILABLE|API_DEPRECATED|API_UNAVAILABLE|__OSX_AVAILABLE|CF_AVAILABLE|CF_DEPRECATED|NS_AVAILABLE|NS_DEPRECATED|AVAILABLE_MAC_OS_X_VERSION'
ANNOT_CACHE="${TMPDIR:-/tmp}/macos-dyld-annot${SDK_KEY}.txt"
if [ ! -s "$ANNOT_CACHE" ]; then
  say "building SDK availability corpus from headers (first run only, ~1 min)..."
  grep -rn --include='*.h' -B2 -E "$ANNOT_RE" \
    "$SDK/System/Library/Frameworks" 2>/dev/null > "$ANNOT_CACHE.tmp"
  mv "$ANNOT_CACHE.tmp" "$ANNOT_CACHE"
fi
say "SDK availability corpus: $(wc -l < "$ANNOT_CACHE" | tr -d ' ') lines ($ANNOT_CACHE)"
say ""

: > "$WORK/avail.txt"
if [ -s "$WORK/names.txt" ]; then
  awk -v namefile="$WORK/names.txt" '
    function ver(s,   v) {
      # API_AVAILABLE(macos(11.0)) / API_DEPRECATED(..., macos(10.6, 10.15))
      if (match(s, /macos\([0-9]+\.[0-9]+/)) {
        v = substr(s, RSTART + 6, RLENGTH - 6); return v
      }
      # __OSX_AVAILABLE_STARTING(__MAC_10_11, ...) / __MAC_OS_X_VERSION_MIN_REQUIRED
      if (match(s, /__MAC_[0-9]+_[0-9]+/)) {
        v = substr(s, RSTART + 6, RLENGTH - 6); gsub(/_/, ".", v); return v
      }
      # CF_AVAILABLE_MAC(10_11) / CF_AVAILABLE(10_11, 5_0)
      if (match(s, /CF_AVAILABLE(_MAC)?\([0-9]+_[0-9]+/)) {
        v = substr(s, RSTART, RLENGTH); sub(/^[^(]*\(/, "", v); gsub(/_/, ".", v); return v
      }
      if (match(s, /AVAILABLE_MAC_OS_X_VERSION_[0-9]+_[0-9]+_AND_LATER/)) {
        v = substr(s, RSTART + 27, RLENGTH - 27 - 10); gsub(/_/, ".", v); return v
      }
      return ""
    }
    function strip(s) { sub(/^[^ ]+\.h[:-][0-9]+[:-]/, "", s); return s }
    function cmpv(a, b,   pa, pb) {
      split(a, pa, "."); split(b, pb, ".")
      if (pa[1]+0 != pb[1]+0) return (pa[1]+0 < pb[1]+0) ? -1 : 1
      if (pa[2]+0 != pb[2]+0) return (pa[2]+0 < pb[2]+0) ? -1 : 1
      return 0
    }
    BEGIN {
      while ((getline n < namefile) > 0) if (n != "") names[n] = 1
      nl = 0
    }
    { line[++nl] = $0 }
    END {
      for (i = 1; i <= nl; i++) {
        # grep -B2: the annotation is on the MATCH line ("path.h:NN:"), the
        # declaration it belongs to is that line or one of the two context lines
        # before it ("path.h-NN-").
        if (line[i] !~ /^[^ ]+\.h:[0-9]+:/) continue
        v = ver(strip(line[i]))
        if (v == "") continue
        path = line[i]; sub(/\.h:[0-9]+:.*$/, ".h", path)
        blob = strip(line[i])
        if (i - 1 >= 1) blob = strip(line[i-1]) " " blob
        if (i - 2 >= 1) blob = strip(line[i-2]) " " blob
        n2 = split(blob, tok, /[^A-Za-z0-9_$]+/)
        for (k = 1; k <= n2; k++) {
          t = tok[k]
          if (t == "" || !(t in names)) continue
          # keep the EARLIEST version seen: a symbol declared once and deprecated
          # later shows up in several annotations; the introduction is what gates.
          if (!(t in best) || cmpv(v, best[t]) < 0) { best[t] = v; src[t] = path }
        }
      }
      for (t in best) print t "\t" best[t] "\t" src[t]
    }
  ' "$ANNOT_CACHE" | sort > "$WORK/avail.txt"
fi

# ---------------------------------------------------------------- allowlist
: > "$WORK/allow.txt"
if [ -f "$ALLOWLIST" ]; then
  sed -e 's/#.*//' -e 's/[[:space:]]//g' "$ALLOWLIST" | grep -v '^$' | sort -u > "$WORK/allow.txt"
fi

# ---------------------------------------------------------------- classify
say "== undefined symbols ($TOTAL) =="
say ""
printf '%-8s %-52s %-9s %-26s %s\n' STATUS SYMBOL INTRO LIBRARY NOTE > "$WORK/table.txt"

awk -F'\t' \
  -v sdkindex="$INDEX_CACHE" -v availf="$WORK/avail.txt" -v allowf="$WORK/allow.txt" \
  -v fmaj="$FLOOR_MAJ" -v fmin="$FLOOR_MIN" -v floor="$FLOOR" '
  function newer(v,   p) {
    if (v == "") return 0
    split(v, p, ".")
    if (p[1]+0 > fmaj+0) return 1
    if (p[1]+0 < fmaj+0) return 0
    return (p[2]+0 > fmin+0) ? 1 : 0
  }
  BEGIN {
    while ((getline l < sdkindex) > 0) sdk[l] = 1
    while ((getline l < availf) > 0) { split(l, f, "\t"); intro[f[1]] = f[2] }
    while ((getline l < allowf) > 0) allow[l] = 1
    fails = 0; weaks = 0; oks = 0; unknowns = 0; allowed = 0
  }
  {
    sym = $1; weak = $2; lib = $3
    bare = sym; sub(/^_/, "", bare)
    sub(/^OBJC_(META)?CLASS_\$_/, "", bare)   # _OBJC_CLASS_$_SCStream -> SCStream
    v = (bare in intro) ? intro[bare] : ""
    inSdk = (sym in sdk)
    note = ""; status = ""

    if (weak == "1") { status = "WEAK"; note = "weakly imported - binds to 0, safe"; weaks++ }
    else if (sym in allow) { status = "ALLOW"; note = "allowlisted"; allowed++ }
    else if (!inSdk) { status = "FAIL"; note = "ABSENT from the " floor " SDK stubs"; fails++ }
    else if (newer(v)) { status = "FAIL"; note = "header says introduced " v " > " floor; fails++ }
    else if (v != "") { status = "ok"; oks++ }
    else { status = "ok"; note = "in SDK stub; no header annotation found"; oks++; unknowns++ }

    printf "%-8s %-52s %-9s %-26s %s\n", status, sym, (v == "" ? "-" : v), lib, note
  }
  END {
    printf "\x01%d\t%d\t%d\t%d\t%d\n", fails, weaks, oks, unknowns, allowed
  }
' "$WORK/undef.txt" > "$WORK/rows.txt"

SUMMARY=$(grep $'^\x01' "$WORK/rows.txt" | tr -d $'\x01')
grep -v $'^\x01' "$WORK/rows.txt" > "$WORK/rows-clean.txt"

FAILS=$(echo "$SUMMARY" | cut -f1)
WEAKS=$(echo "$SUMMARY" | cut -f2)
OKS=$(echo "$SUMMARY" | cut -f3)
UNKNOWNS=$(echo "$SUMMARY" | cut -f4)
ALLOWED=$(echo "$SUMMARY" | cut -f5)

if [ "$QUIET" -eq 1 ]; then
  grep -E '^(FAIL)' "$WORK/rows-clean.txt"
else
  cat "$WORK/table.txt"
  sort -k1,1r -k2,2 "$WORK/rows-clean.txt"
  echo
fi

# ----------------------------------------------------- runtime-resolved names
if [ "$SRC_SCAN" -eq 1 ] && [ -d "$SRC_DIR" ]; then
  say "== runtime-resolved (NOT link-time; these are the safe way to reach newer APIs) =="
  {
    grep -rhoE 'dlsym\([^,]+,[[:space:]]*c?"[A-Za-z_][A-Za-z0-9_]*"' "$SRC_DIR" 2>/dev/null \
      | grep -oE '"[A-Za-z_][A-Za-z0-9_]*"' | tr -d '"' | sed 's/^/  dlsym          /'
    grep -rhoE 'dlsym\([^)]*b"[A-Za-z_][A-Za-z0-9_]*' "$SRC_DIR" 2>/dev/null \
      | grep -oE 'b"[A-Za-z_][A-Za-z0-9_]*' | sed 's/^b"//' | sed 's/^/  dlsym          /'
    grep -rhoE 'AnyClass::get\(c?"[A-Za-z_][A-Za-z0-9_]*"' "$SRC_DIR" 2>/dev/null \
      | grep -oE '"[A-Za-z_][A-Za-z0-9_]*"' | tr -d '"' | sed 's/^/  AnyClass::get  /'
    grep -rhoE 'AnyProtocol::get\(c?"[A-Za-z_][A-Za-z0-9_]*"' "$SRC_DIR" 2>/dev/null \
      | grep -oE '"[A-Za-z_][A-Za-z0-9_]*"' | tr -d '"' | sed 's/^/  AnyProtocol    /'
    grep -rhoE 'sck_(optional_cfstring|class)\(c"[A-Za-z_][A-Za-z0-9_]*"' "$SRC_DIR" 2>/dev/null \
      | grep -oE '"[A-Za-z_][A-Za-z0-9_]*"' | tr -d '"' | sed 's/^/  sck runtime    /'
    grep -rhoE 'set_optional[a-z0-9_]*\([^,]*,[[:space:]]*"k[A-Za-z0-9_]*"' "$SRC_DIR" 2>/dev/null \
      | grep -oE '"k[A-Za-z0-9_]*"' | tr -d '"' | sed 's/^/  VT dlsym key   /'
    # A dlsym target is often reached through a helper that takes &str and does the
    # CString::new itself (permissions.rs cg_bool_fn, streamer/mod.rs), so the literal
    # never sits next to the word "dlsym".
    grep -rhoE 'CString::new\("[A-Za-z_][A-Za-z0-9_]*"' "$SRC_DIR" 2>/dev/null \
      | grep -oE '"[A-Za-z_][A-Za-z0-9_]*"' | tr -d '"' | sed 's/^/  CString+dlsym  /'
    # ...and the dlsym call is often split over several lines, putting the name out of
    # reach of a line-based match. `c"Name".as_ptr()` is the giveaway.
    grep -rhoE 'c"[A-Za-z_][A-Za-z0-9_]*"\.as_ptr\(\)' "$SRC_DIR" 2>/dev/null \
      | grep -oE '"[A-Za-z_][A-Za-z0-9_]*"' | tr -d '"' | sed 's/^/  cstr FFI name  /'
  } | sort -u -k2 | sed '/^[[:space:]]*$/d' > "$WORK/runtime.txt"
  [ "$QUIET" -eq 1 ] || cat "$WORK/runtime.txt"
  say "  (best effort: only string literals are found. A name computed at runtime or"
  say "   passed through a helper that this scan does not know about will be missing.)"
  say ""
fi

# ---------------------------------------------------------------- verdict
echo "== verdict =="
echo "  floor          $FLOOR"
echo "  undefined      $TOTAL"
echo "  ok             $OKS  (of which $UNKNOWNS in the SDK stub with no header annotation)"
echo "  weak imports   $WEAKS"
echo "  allowlisted    $ALLOWED"
echo "  offenders      $FAILS"
if [ "${FAILS:-0}" -gt 0 ]; then
  echo
  echo "  FAIL: the binary strongly references $FAILS symbol(s) the $FLOOR dyld cannot bind."
  echo "        dyld binds these before main(), so the app will not launch on $FLOOR."
  echo "        Fix by resolving them at runtime (dlsym / AnyClass::get / msg_send!) or,"
  echo "        if the symbol really is present at $FLOOR, add it to"
  echo "        $ALLOWLIST with a reason."
  exit 1
fi
echo
echo "  PASS: every strongly-bound undefined symbol exists at $FLOOR."
exit 0
