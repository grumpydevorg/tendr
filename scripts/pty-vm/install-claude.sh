#!/bin/bash
# Runs as the VM's sudo-capable user on the disposable VM, after provision-root.sh.
# Installs Claude Code (checksum-verified) and gives acc and the calling user a
# Claude config that uses exe.dev's LLM gateway, so no credential is stored on
# the VM. It overwrites both users' ~/.claude/settings.json.
# Install tendr separately, e.g. from a release binary into /usr/local/bin.
set -euo pipefail
base=https://downloads.claude.ai/claude-code-releases
ver=$(curl -fsSL "$base/latest")
[[ "$ver" =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]]
cd /tmp
curl -fsSL "$base/$ver/manifest.json" -o claude-manifest.json
want=$(python3 -c 'import json;print(json.load(open("claude-manifest.json"))["platforms"]["linux-x64"]["checksum"])')
curl -fsSL "$base/$ver/linux-x64/claude" -o claude-bin
got=$(sha256sum claude-bin | cut -d' ' -f1)
[ "$want" = "$got" ] || { echo "install-claude.sh: sha256 mismatch want=$want got=$got" >&2; exit 1; }
echo "claude $ver sha256 OK $got"
sudo install -m 0755 claude-bin /usr/local/bin/claude
command -v tendr >/dev/null || { echo "install-claude.sh: install tendr first" >&2; exit 1; }

# Claude config for acc and for the login user (who attaches over exe.dev's own
# `ssh -t <vm>.exe.xyz`): exe.dev gateway, no credential on the VM, no
# auto-update, and the first-run screens (theme, folder trust) already settled
# for ~/slice1/work and ~/work, so a prompt is not typed into them.
setup_user() {
  sudo -u "$1" -H bash -c '
set -e
mkdir -p ~/.claude ~/work ~/slice1/work
cat > ~/.claude/settings.json <<EOF
{
  "apiKeyHelper": "printf exe-gateway",
  "env": {
    "ANTHROPIC_BASE_URL": "https://llm.int.exe.xyz",
    "DISABLE_AUTOUPDATER": "1",
    "DISABLE_TELEMETRY": "1",
    "DISABLE_ERROR_REPORTING": "1"
  }
}
EOF
chmod 600 ~/.claude/settings.json
claude --version >/dev/null  # creates ~/.claude.json
python3 - "$(claude --version | cut -d" " -f1)" <<PY
import json, os, sys
path = os.path.expanduser("~/.claude.json")
d = json.load(open(path)) if os.path.exists(path) else {}
d.update(hasCompletedOnboarding=True, theme="dark", lastOnboardingVersion=sys.argv[1])
for w in ("~/slice1/work", "~/work"):
    p = d.setdefault("projects", {}).setdefault(os.path.expanduser(w), {})
    p.update(hasTrustDialogAccepted=True, hasCompletedProjectOnboarding=True)
json.dump(d, open(path, "w"), indent=2)
PY
chmod 600 ~/.claude.json
'
  sudo -u "$1" -H tendr --version
  sudo -u "$1" -H claude --version
}
setup_user acc
setup_user "$(id -un)"
