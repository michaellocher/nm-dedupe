#!/bin/bash
# End-to-end checks on throwaway npm projects. Usage: tests/sandbox.sh [scratch-dir]
# Uses an isolated HOME so the real index in ~/.cache/nm-dedupe is never touched.
set -u
B="$(cd "$(dirname "$0")/.." && pwd)/target/release/nm-dedupe"
T="${1:-${TMPDIR:-/tmp}/nm-dedupe-test}"
rm -rf "$T"; mkdir -p "$T/home" "$T/p1" "$T/p2" "$T/p3"
export HOME="$T/home" npm_config_cache="$T/npmcache"
fail=0
check() { if eval "$2"; then echo "PASS  $1"; else echo "FAIL  $1"; fail=1; fi; }
num() { grep -o "$1 [0-9]*" | grep -o '[0-9]*$'; }

for p in p1 p2; do (cd "$T/$p" && npm init -y >/dev/null && npm i --silent typescript@5.6.3 lodash@4.17.21 esbuild@0.24.0 >/dev/null 2>&1); done
(cd "$T/p3" && npm init -y >/dev/null && npm i --silent typescript@5.6.3 >/dev/null 2>&1)

out=$("$B" -n "$T/p1" "$T/p2"); echo "$out" | sed 's/^/      /'
check "dry run changes nothing" '[ "$(echo "$out" | num "to clone:")" -gt 1000 ]'
out=$("$B" -n "$T/p1" "$T/p2")
check "dry run is repeatable" '[ "$(echo "$out" | num "already cloned:")" = 0 ]'

out=$("$B" "$T/p1" "$T/p2"); echo "$out" | sed 's/^/      /'
check "real run clones, no errors" '[ "$(echo "$out" | num "errors")" = 0 ] && [ "$(echo "$out" | num "cloned")" -gt 1000 ]'
out=$("$B" "$T/p1" "$T/p2")
check "rerun: all already cloned" '[ "$(echo "$out" | num "to clone:")" = 0 ]'

out=$("$B" "$T/p3"); echo "$out" | sed 's/^/      /'
check "later project matched via index" '[ "$(echo "$out" | num "cloned")" -gt 100 ]'

check "modules still work" '(cd "$T/p2" && node -e "require(\"lodash\").chunk([1],1); require(\"typescript\")" && npx esbuild --version >/dev/null)'
check "content identical" '[ -z "$(diff -rq "$T/p1/node_modules" "$T/p2/node_modules" | grep -v package-lock)" ]'
check "no temp files left" '[ -z "$(find "$T" -name ".nmdd-*")" ]'

echo "// edited" >> "$T/p1/node_modules/lodash/chunk.js"
check "copy-on-write: edit stays local" '! grep -q edited "$T/p2/node_modules/lodash/chunk.js"'

R="$T/regr"; mkdir -p "$R/a/node_modules/x" "$R/b/node_modules/x"
printf 'original content 1234\n' > "$R/a/node_modules/x/f.js"; "$B" -q "$R/a" >/dev/null
printf 'EVIL!!!! content 1234\n' > "$R/a/node_modules/x/f.js"; touch -t 202501010000 "$R/a/node_modules/x/f.js"; "$B" -q "$R/a" >/dev/null
printf 'original content 1234\n' > "$R/b/node_modules/x/f.js"; "$B" -q "$R/b" >/dev/null
check "same-size edit of canonical never propagates" '[ "$(cat "$R/b/node_modules/x/f.js")" = "original content 1234" ]'

mkdir -p "$T/m/node_modules/y"; printf 'abc\n' > "$T/m/node_modules/y/1"; cp -p "$T/m/node_modules/y/1" "$T/m/node_modules/y/2"
chmod 600 "$T/m/node_modules/y/2"; touch -t 202001010000 "$T/m/node_modules/y/2"; before=$(stat -f '%p %m' "$T/m/node_modules/y/2")
"$B" -q "$T/m" >/dev/null
check "mode + mtime preserved" '[ "$(stat -f "%p %m" "$T/m/node_modules/y/2")" = "$before" ]'

rm -rf "$T"
exit $fail
