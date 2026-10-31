#!/usr/bin/env bash
# Deploy or update tr-gateway on a Freestyle VM, published at https://$DOMAIN (wss://$DOMAIN/ws).
# The VM builds the ref from GitHub, so push it first.
# Needs the Freestyle CLI auth in the environment (FREESTYLE_API_KEY); nothing is printed from it.
# Usage: deploy/freestyle/deploy.sh [git-ref]      (default: master)
# Env:   TR_VM_SLUG (default terminal-gateway), TR_DOMAIN (default terminal-gateway.style.dev),
#        TR_VM_SNAPSHOT (default freestyle/ubuntu-sm: 2 vCPU / 4 GiB / 16 GB),
#        TR_FREESTYLE_CLI (default freestyle@0.2.21, the npm package that runs the CLI)
set -euo pipefail
REF="${1:-master}"
SLUG="${TR_VM_SLUG:-terminal-gateway}"
DOMAIN="${TR_DOMAIN:-terminal-gateway.style.dev}"
SNAP="${TR_VM_SNAPSHOT:-freestyle/ubuntu-sm}"
CLI="${TR_FREESTYLE_CLI:-freestyle@0.2.21}"
HERE="$(cd "$(dirname "$0")" && pwd)"

[ -n "${FREESTYLE_API_KEY:-}" ] || { echo "set FREESTYLE_API_KEY" >&2; exit 1; }
for tool in npx jq curl; do
  command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }
done
fs() { npx -y "$CLI" "$@"; }

if ! fs --output json vm get "$SLUG" >/dev/null 2>&1; then
  echo "creating VM $SLUG from $SNAP"
  fs --output json vm create --snapshot-id "$SNAP" --slug "$SLUG" --display-name "Terminal data server (tr-gateway)" \
    --metadata app=terminal --metadata role=tr-gateway --no-ssh >/dev/null
fi
VM_ID="$(fs --output json vm get "$SLUG" | jq -r .id)"

if ! fs --output json tls list | jq -e --arg d "$DOMAIN" '.rules[]? | select(.domain==$d)' >/dev/null; then
  echo "publishing https://$DOMAIN -> $SLUG:8080"
  fs --output json tls create --domain "$DOMAIN" --from public --to "vm=$VM_ID,port=8080" >/dev/null
fi

fs vm scp "$HERE/vm-setup.sh" "$SLUG:/root/vm-setup.sh" >/dev/null
# The first build takes about 4 minutes on 2 vCPU; later ones are incremental.
fs vm exec "$SLUG" --linux-user root --timeout-ms 1800000 -- bash /root/vm-setup.sh "$REF"

# The VM said it's ready; check the public path through Freestyle's edge too.
for _ in $(seq 1 30); do
  if curl -fsS "https://$DOMAIN/readyz" >/dev/null 2>&1; then echo "ready: wss://$DOMAIN/ws"; exit 0; fi
  sleep 2
done
echo "deployed, but https://$DOMAIN/readyz is not answering 200 yet" >&2
exit 1
