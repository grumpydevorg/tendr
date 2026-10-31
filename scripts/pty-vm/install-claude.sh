#!/bin/bash
# Runs as the VM's sudo-capable user on the disposable VM, after provision-root.sh.
# Installs Claude Code (checksum-verified) and gives the acc user a Claude config
# that uses exe.dev's LLM gateway, so no credential is stored on the VM.
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
[ "$want" = "$got" ] && echo "claude $ver sha256 OK $got"
sudo install -m 0755 claude-bin /usr/local/bin/claude
command -v tendr >/dev/null || { echo "install-claude.sh: install tendr first" >&2; exit 1; }

# Claude config for acc: exe.dev gateway, no credential on the VM, no auto-update.
sudo -u acc -H bash -c '
set -e
mkdir -p ~/.claude ~/work
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
'
sudo -u acc -H tendr --version
sudo -u acc -H claude --version
