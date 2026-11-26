#!/bin/bash
# Fail unless a macOS .app carries a whole, coherent code signature -- the kind TCC can
# match a grant against.
#
#   bash scripts/check-macos-signature.sh <App.app>
#
# A bundle with only the linker's ad-hoc signature (one slice, identifier `<exe>-<hash>`,
# Info.plist not bound, nothing sealed) has no designated requirement
# (SecCodeCopyDesignatedRequirement returns -67062), so TCC stores grants against a csreq the
# running process can never satisfy. `codesign -dv` alone does not catch it: it happily prints
# the linker signature.
#
# Checks, all of which must pass:
#   1. codesign --verify --deep --strict
#   2. the signing Identifier equals CFBundleIdentifier from Info.plist
#   3. Info.plist is bound to the signature
#   4. resources are sealed
#   5. every architecture slice of the main executable is signed, and its CDHash appears in
#      the designated requirement -- or, when the requirement is not cdhash-based (the
#      Developer ID case), the app satisfies that requirement for every slice.
#   6. every Mach-O under Contents/Resources (today: libopus.dylib, which the app dlopens) is
#      signed (`codesign --verify --strict`) and carries every architecture the main
#      executable has -- an x86_64-only dylib in a universal app loads nowhere but Intel, and
#      under the hardened runtime an unsigned one does not load at all.
#
# Exit 0 = all pass, 1 = a check failed, 2 = usage/environment error.

set -uo pipefail

app="${1:-}"
if [ -z "$app" ] || [ ! -d "$app/Contents" ]; then
  echo "usage: $0 <App.app>" >&2
  exit 2
fi
app="${app%/}"

fail=0
pass() { echo "ok    $*"; }
bad()  { echo "FAIL  $*"; fail=1; }

plist="$app/Contents/Info.plist"
bundle_id=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$plist" 2>/dev/null)
exe_name=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$plist" 2>/dev/null)
if [ -z "$bundle_id" ] || [ -z "$exe_name" ]; then
  echo "error: cannot read CFBundleIdentifier/CFBundleExecutable from $plist" >&2
  exit 2
fi
exe="$app/Contents/MacOS/$exe_name"
[ -f "$exe" ] || { echo "error: main executable $exe is missing" >&2; exit 2; }

# 1. Structural verification of the whole bundle.
if out=$(codesign --verify --deep --strict --verbose=2 "$app" 2>&1); then
  pass "codesign --verify --deep --strict"
else
  bad "codesign --verify --deep --strict:"; echo "$out" | sed 's/^/      /'
fi

info=$(codesign -dvvv "$app" 2>&1)
signed_id=$(printf '%s\n' "$info" | sed -n 's/^Identifier=//p' | head -1)

# 2. Identifier. A linker-only signature carries `<exe>-<hash>` here.
if [ "$signed_id" = "$bundle_id" ]; then
  pass "Identifier=$signed_id matches CFBundleIdentifier"
else
  bad "Identifier=${signed_id:-<none>} but CFBundleIdentifier=$bundle_id"
fi

# 3. Info.plist bound.
plist_line=$(printf '%s\n' "$info" | grep -E '^Info\.plist' | head -1)
if [ -n "$plist_line" ] && ! printf '%s' "$plist_line" | grep -q 'not bound'; then
  pass "$plist_line"
else
  bad "Info.plist is not bound (${plist_line:-no Info.plist line})"
fi

# 4. Sealed resources.
sealed_line=$(printf '%s\n' "$info" | grep -E '^Sealed Resources' | head -1)
if [ -n "$sealed_line" ] && ! printf '%s' "$sealed_line" | grep -q '=none'; then
  pass "$sealed_line"
else
  bad "resources are not sealed (${sealed_line:-no Sealed Resources line})"
fi

# 5. The designated requirement, from the Security framework rather than `codesign -d -r-`:
# for a universal ad-hoc bundle `codesign -d -r-` shows only the host slice's implicit
# requirement, while TCC captures the full `cdhash H"a" or cdhash H"b"` that this returns.
dr=$(python3 -I - "$app" <<'PY'
import ctypes, sys
from ctypes import c_void_p, c_uint32, c_char_p, c_long, byref, create_string_buffer
cf = ctypes.CDLL('/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation')
sec = ctypes.CDLL('/System/Library/Frameworks/Security.framework/Security')
cf.CFURLCreateFromFileSystemRepresentation.restype = c_void_p
cf.CFURLCreateFromFileSystemRepresentation.argtypes = [c_void_p, c_char_p, c_long, ctypes.c_bool]
cf.CFStringGetCString.argtypes = [c_void_p, c_char_p, c_long, c_uint32]
sec.SecStaticCodeCreateWithPath.argtypes = [c_void_p, c_uint32, ctypes.POINTER(c_void_p)]
sec.SecCodeCopyDesignatedRequirement.argtypes = [c_void_p, c_uint32, ctypes.POINTER(c_void_p)]
sec.SecRequirementCopyString.argtypes = [c_void_p, c_uint32, ctypes.POINTER(c_void_p)]
p = sys.argv[1].encode()
url = cf.CFURLCreateFromFileSystemRepresentation(None, p, len(p), True)
code = c_void_p()
if sec.SecStaticCodeCreateWithPath(url, 0, byref(code)) != 0:
    sys.exit(3)
req = c_void_p()
st = sec.SecCodeCopyDesignatedRequirement(code, 0, byref(req))
if st != 0:
    print(f"<SecCodeCopyDesignatedRequirement status {st}>")
    sys.exit(4)
s = c_void_p()
sec.SecRequirementCopyString(req, 0, byref(s))
buf = create_string_buffer(8192)
cf.CFStringGetCString(s, buf, 8192, 0x08000100)
print(buf.value.decode())
PY
)
dr_status=$?
if [ $dr_status -ne 0 ]; then
  bad "no designated requirement: ${dr:-python3/Security framework call failed (status $dr_status)}"
else
  echo "      designated => $dr"
fi

archs=$(lipo -archs "$exe" 2>/dev/null)
[ -n "$archs" ] || archs=$(uname -m)
for arch in $archs; do
  slice=$(codesign -dvvv --arch "$arch" "$app" 2>&1)
  cdhash=$(printf '%s\n' "$slice" | sed -n 's/^CDHash=//p' | head -1)
  if [ -z "$cdhash" ]; then
    bad "$arch slice: not signed ($(printf '%s' "$slice" | tail -1))"
    continue
  fi
  if [ $dr_status -ne 0 ]; then
    echo "      $arch slice CDHash=$cdhash (no designated requirement to check it against)"
    continue
  fi
  if printf '%s' "$dr" | grep -q 'cdhash H"'; then
    if printf '%s' "$dr" | grep -qi "cdhash H\"$cdhash\""; then
      pass "$arch slice CDHash=$cdhash is in the designated requirement"
    else
      bad "$arch slice CDHash=$cdhash is NOT in the designated requirement"
    fi
  else
    if out=$(codesign --verify --strict --arch "$arch" -R="$dr" "$app" 2>&1); then
      pass "$arch slice CDHash=$cdhash satisfies the non-cdhash designated requirement"
    else
      bad "$arch slice does not satisfy its designated requirement:"; echo "$out" | sed 's/^/      /'
    fi
  fi
done

# 6. Mach-O resources. Detected by content (`lipo -archs` answers only for Mach-O), not by
# extension; NUL-delimited so a path with spaces or newlines is one path.
resources="$app/Contents/Resources"
macho_count=0
if [ -d "$resources" ]; then
  find_err=$(mktemp -t se-sigcheck) || { echo "error: mktemp failed" >&2; exit 2; }
  while IFS= read -r -d '' f; do
    res_archs=$(lipo -archs "$f" 2>/dev/null) || continue
    [ -n "$res_archs" ] || continue
    macho_count=$((macho_count + 1))
    rel="${f#"$app"/}"
    if out=$(codesign --verify --strict --verbose=2 "$f" 2>&1); then
      pass "$rel is signed ($res_archs)"
    else
      bad "$rel is not validly signed:"; echo "$out" | sed 's/^/      /'
    fi
    for arch in $archs; do
      case " $res_archs " in
        *" $arch "*) ;;
        *) bad "$rel lacks the $arch slice the main executable has (has: $res_archs)" ;;
      esac
    done
  done < <(find "$resources" -type f -print0 2>"$find_err")
  if [ -s "$find_err" ]; then
    bad "could not walk $resources:"; sed 's/^/      /' "$find_err"
  fi
  rm -f "$find_err"
fi
echo "      $macho_count Mach-O file(s) under Contents/Resources checked"

if [ $fail -ne 0 ]; then
  echo "check-macos-signature: FAILED for $app"
  exit 1
fi
echo "check-macos-signature: ok for $app"
