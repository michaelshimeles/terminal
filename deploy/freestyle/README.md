# tr-gateway on Freestyle

The Terminal data server runs on a Freestyle VM at **https://terminal-gateway.style.dev**. Clients connect to `wss://terminal-gateway.style.dev/ws`, for example the desktop app:

```sh
TR_SERVER_URL=wss://terminal-gateway.style.dev cargo run --release
```

## What runs where

- One VM, slug `terminal-gateway`, from the `freestyle/ubuntu-sm` snapshot: 2 vCPU, 4 GiB of memory, 16 GB of disk.
- `tr-gateway` runs under systemd (`tr-gateway.service`) as the unprivileged user `trgw`, bound to `0.0.0.0:8080`. systemd restarts it whenever it exits.
- Freestyle's edge terminates TLS on 443 and forwards to port 8080, so the gateway itself serves plain HTTP and websockets.
- The source lives in `/opt/terminal/src`. `/opt/terminal/DEPLOYED_COMMIT` holds the commit that's running, and the CLI example is installed as `tr-cli`.

## Why this size

The VM builds the gateway from source on every deploy, so the 16 GB disk holds the Rust toolchain and the build cache. On 2 vCPU a first build takes about 4 minutes, and later ones are incremental. The gateway itself is one process with a few upstream websockets: on a Mac against the live feed, with one client watching BTC's quotes, book, trades and 1m bars, its resident memory was about 15 MiB. A bigger VM buys nothing until there are many more clients.

## Deploy or update

From a machine with Node (for `npx`), `jq`, `curl` and a Freestyle API key:

```sh
export FREESTYLE_API_KEY=...        # never commit or paste it
deploy/freestyle/deploy.sh master   # or a branch, tag or full commit
```

`deploy.sh` creates the VM if it's missing and publishes the domain with a TLS rule if that's missing. It then copies `vm-setup.sh` to the VM and runs it at the ref you give. The VM builds that ref from GitHub, so push it first. `vm-setup.sh` installs the build tools and Rust (keeping Rust on the latest stable, like CI), builds `tr-gateway` and the CLI with `--locked`, installs them, writes the systemd unit and restarts the service. It waits for `/readyz` on the VM, and `deploy.sh` then waits for `/readyz` through the public URL.

A deploy restarts the gateway, which drops every client connection. Clients reconnect and resubscribe on their own. A subscription made while the gateway is still starting gets `Stale`, then `Live` and its snapshot once the gateway has its instrument list and mids (`server/README.md`).

## Check health

```sh
curl -fsS https://terminal-gateway.style.dev/healthz   # the process is up
curl -fsS https://terminal-gateway.style.dev/readyz    # upstream connected, every venue's mids in
cli() { cargo run -q --release -p tr-gateway --example cli -- --url wss://terminal-gateway.style.dev "$@"; }
cli stats
cli watch --quotes BTC --book BTC --trades BTC --bars BTC:1m --secs 30
```

On the VM, `systemctl status tr-gateway` and `journalctl -u tr-gateway` show the service and its logs, and `cat /opt/terminal/DEPLOYED_COMMIT` shows what's deployed. The gateway's logs never carry addresses.

## Cost

2 vCPU and 4 GiB running around the clock come to about $80 a month of Freestyle usage above the free allowance.

## `/stats` and `/metrics` are public

Anyone can read `/stats` (JSON) and `/metrics` (Prometheus text) through the public URL right now. They show counts, budgets, latency and the upstream subscriptions, which reveal the markets clients are watching. They carry no client addresses or user data. Micky hasn't decided whether to restrict them, so they stay as they are for now. Restricting them later means serving them on a second port the edge doesn't publish, or checking a token.
