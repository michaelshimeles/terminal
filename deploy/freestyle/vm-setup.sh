#!/usr/bin/env bash
# Runs as root inside a Freestyle Ubuntu VM. Builds tr-gateway from a git ref and
# runs it under systemd on 0.0.0.0:8080; Freestyle's edge terminates TLS on 443.
# Usage: vm-setup.sh [git-ref]   (a branch, tag or full commit; default: master)
set -euo pipefail
REF="${1:-master}"
REPO="https://github.com/michaelshimeles/terminal.git"
SRC=/opt/terminal/src
export DEBIAN_FRONTEND=noninteractive

apt-get update -qq
apt-get install -y -qq build-essential pkg-config libssl-dev git curl ca-certificates >/dev/null

if ! id trgw >/dev/null 2>&1; then useradd --system --create-home --home-dir /var/lib/trgw --shell /usr/sbin/nologin trgw; fi

# CI builds with the latest stable Rust, so the VM keeps up with it.
export RUSTUP_HOME=/opt/rust/rustup CARGO_HOME=/opt/rust/cargo PATH=/opt/rust/cargo/bin:$PATH
if command -v cargo >/dev/null; then
  rustup update --no-self-update stable >/dev/null
else
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path
fi

mkdir -p /opt/terminal
if [ ! -d "$SRC/.git" ]; then
  git clone --quiet "$REPO" "$SRC"
fi
git -C "$SRC" fetch --quiet origin "$REF"
git -C "$SRC" checkout --quiet --detach FETCH_HEAD
cd "$SRC"
echo "building $(git rev-parse HEAD)"
cargo build --release --locked -p tr-gateway --bin tr-gateway --example cli
install -m 0755 target/release/tr-gateway /usr/local/bin/tr-gateway.new
mv /usr/local/bin/tr-gateway.new /usr/local/bin/tr-gateway
install -m 0755 target/release/examples/cli /usr/local/bin/tr-cli
git rev-parse HEAD > /opt/terminal/DEPLOYED_COMMIT

cat > /etc/systemd/system/tr-gateway.service <<'UNIT'
[Unit]
Description=Terminal data server (tr-gateway)
After=network-online.target
Wants=network-online.target
# Keep restarting however often it fails; systemd would otherwise give up after 5 quick tries.
StartLimitIntervalSec=0

[Service]
User=trgw
Environment=TR_GATEWAY_BIND=0.0.0.0:8080
Environment=RUST_LOG=info
ExecStart=/usr/local/bin/tr-gateway
Restart=always
RestartSec=2
LimitNOFILE=65536
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable tr-gateway >/dev/null 2>&1
systemctl restart tr-gateway

# Wait until the process is up (/healthz), then until it has the instrument list and
# every venue's mids (/readyz).
wait_for() {
  for _ in $(seq 1 "$2"); do
    if curl -fsS "http://127.0.0.1:8080/$1" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  echo "/$1 did not answer 200 within $2 s" >&2
  systemctl --no-pager status tr-gateway >&2 || true
  journalctl --no-pager -u tr-gateway -n 30 >&2 || true
  return 1
}
wait_for healthz 30
wait_for readyz 90
echo "deployed $(cat /opt/terminal/DEPLOYED_COMMIT); ready"
