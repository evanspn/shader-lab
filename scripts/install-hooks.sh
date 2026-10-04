#!/bin/bash
# Opt in to a git pre-push hook that runs the fast regression subset (a GPU is needed; with none it says so and passes).
set -euo pipefail
cd "$(dirname "$0")/.."
hook=.git/hooks/pre-push
cat > "$hook" <<'HOOK'
#!/bin/bash
cd "$(git rev-parse --show-toplevel)"
echo "pre-push: shaderlab regress --fast (git push --no-verify skips this)"
scripts/regress.sh --fast
HOOK
chmod +x "$hook"
echo "installed $hook"
