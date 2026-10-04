#!/bin/bash
# Fail if the tracked tree holds anything that must not be public: images outside tests/golden (goldens are the shader's
# output over the SYNTHETIC terminal frame only), personal paths, e-mail addresses, home folders.
set -uo pipefail
cd "$(dirname "$0")/.."
bad=0
images=$(git ls-files | grep -iE '\.(png|jpe?g|gif|webp|bmp|ppm|mp4|mov)$' | grep -v '^tests/golden/' || true)
if [[ -n "$images" ]]; then echo "images outside tests/golden:"; echo "$images"; bad=1; fi
big=$(git ls-files tests/golden | while read -r f; do [[ -f "$f" ]] && [[ $(stat -f %z "$f" 2>/dev/null || stat -c %s "$f") -gt 40000 ]] && echo "$f"; done)
if [[ -n "$big" ]]; then echo "golden images should stay tiny (over 40 KB):"; echo "$big"; bad=1; fi
hits=$(git grep -nIE '(/Users/[A-Za-z0-9._-]+|/home/[A-Za-z0-9._-]+|[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[a-z]{2,})' -- . ':!Cargo.lock' ':!scripts/privacy-scan.sh' ':!*.md' | grep -v 'noreply.github.com' | grep -vE '@(example|users)\.' || true)
if [[ -n "$hits" ]]; then echo "personal paths or addresses:"; echo "$hits"; bad=1; fi
[[ $bad == 0 ]] && echo "privacy scan: clean"
exit $bad
