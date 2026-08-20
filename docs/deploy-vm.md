# Deploying on a VM

A complete guide to running the bot as a long-lived Docker Compose service on any VM you control — a cloud instance, bare metal, whatever. This is the `loop` run mode: one continuous process, restarted by Docker/systemd if it dies, rather than the cron-triggered `--run-mode once` pattern (see the README's Run modes section).

## Prerequisites

1. **A server**
   - Ubuntu 24.04 LTS (recommended — the scripts below target it, though anything with a recent Docker works).
   - Minimum 2 vCPU / 4GB RAM / 40GB SSD.
   - Any provider — Hetzner, DigitalOcean, AWS, GCP, your own hardware.
   - A public IP if you want to reach it over SSH from elsewhere.

2. **SSH access** — key-based auth, root or sudo.

3. **Your local machine** — an SSH client and `git`. Docker locally only if you plan to build the image on your machine and transfer it (`--build-local` below); otherwise the server builds it.

## Quick start

### 1. Server setup (one-time)

SSH into the server as root. Download the initialization script, read it, then run it — don't pipe a remote script straight into a root shell:

```bash
curl -fsSL -o init-server.sh https://raw.githubusercontent.com/Templar-Protocol/templar-liquidator/main/scripts/init-server.sh
less init-server.sh   # read it before running anything as root
sudo bash init-server.sh
```

This installs Docker + Compose, creates a dedicated `liquidator` user, configures the firewall (UFW: allows 22 and 80 *before* enabling — so an existing SSH session doesn't get locked out), sets up log rotation, and installs a cron-driven watchdog that restarts the container if it's found down.

### 2. SSH key setup

From your local machine:

```bash
ssh-copy-id liquidator@YOUR_SERVER_IP
ssh liquidator@YOUR_SERVER_IP   # confirm it works
```

### 3. Deploy

From your local machine, in a checkout of this repo:

```bash
./scripts/deploy.sh YOUR_SERVER_IP              # git deploy (default) — clones and builds on the server
./scripts/deploy.sh YOUR_SERVER_IP --build-local  # build locally, transfer the image instead
```

### 4. Configure environment

```bash
ssh liquidator@YOUR_SERVER_IP
cd /opt/templar-liquidator/repo   # or /opt/templar-liquidator if you used --build-local
nano .env
```

Fill in at minimum the three required vars — `REGISTRY_ACCOUNT_IDS`, `SIGNER_ACCOUNT_ID`, `SIGNER_KEY` — plus `NEAR_NETWORK` and anything else from [docs/configuration.md](configuration.md) you want to change from its default. See `.env.example` for the full annotated list.

### 5. Start in dry-run — or verify it isn't

Before starting the container, `deploy.sh` guarantees `.env` carries an explicit `DRY_RUN=` — if one isn't already there, it appends `DRY_RUN=true` for you, on **both** deploy paths from step 3. So a first deploy (`--git-deploy` or `--build-local`, but not `--update` — see below) starts in dry-run by default even if you never touched `DRY_RUN` yourself.

That safety net exists because the two paths run different compose files with genuinely different defaults, and it's worth understanding which one is actually protecting you:

- **Git deploy (default)** — the server runs the repo's own, untouched `docker-compose.yml`, which sets no `DRY_RUN` override, so the binary's own safe-by-default `true` applies regardless of the script's safety net.
- **`--build-local`** — the server runs `docker-compose.prod.yml` (installed *as* `docker-compose.yml`), whose `DRY_RUN=${DRY_RUN:-false}` means **live** if `.env` omits `DRY_RUN` entirely. This is the path the script's safety net is actually for — without it, an operator who skipped setting `DRY_RUN` would go live silently.

The one gap the safety net doesn't close: if you already had a `.env` with `DRY_RUN=false` (or a blank `DRY_RUN=`) sitting on the server *before* running the script, `deploy.sh` leaves it alone — it only fills in a *missing* `DRY_RUN=` line, it never second-guesses one that's already there.

`./scripts/deploy.sh` reports which mode it actually started in at the end of its output (`start_service` re-reads whatever `.env` ended up with) — check that rather than assuming. Watch the logs either way:

```bash
cd /opt/templar-liquidator/repo   # or /opt/templar-liquidator
docker compose logs -f
```

Let it run through several full scan cycles — long enough to see every configured market scanned at least once, ideally 24 hours — before going live. Confirm registry discovery finds the markets you expect and scans complete cleanly.

### 6. Go live

Once you trust the configuration:

```bash
nano .env   # set DRY_RUN=false
docker compose down
docker compose up -d
```

## `scripts/deploy.sh` reference

```text
Usage: ./scripts/deploy.sh <server-ip> [options]

Options:
    --build-local       Build the Docker image locally and transfer it
    --git-deploy        Clone the repo and build on the server (default)
    --update            Update an existing deployment
    -h, --help          Show this help message
```

```bash
./scripts/deploy.sh 123.45.67.89                    # git deploy (default)
./scripts/deploy.sh 123.45.67.89 --build-local        # build locally, transfer
./scripts/deploy.sh 123.45.67.89 --update             # update existing deployment
```

`--git-deploy` clones `https://github.com/Templar-Protocol/templar-liquidator.git` (branch `main` by default; override with `GIT_BRANCH`) into `/opt/templar-liquidator/repo` on the server and builds `docker-compose.prod.yml` there. `--build-local` builds the image on your machine, `docker save`s it, and `scp`s it across along with `docker-compose.prod.yml` (renamed to `docker-compose.yml` on the server) and `.env.example`.

## Manual deployment (no scripts)

If you'd rather do it by hand:

```bash
# Server setup
sudo apt update && sudo apt upgrade -y

# Install Docker from its own pinned apt repository rather than piping an
# installer script into a root shell (https://docs.docker.com/engine/install/ubuntu/):
sudo apt install -y ca-certificates curl gnupg
sudo install -m 0755 -d /etc/apt/keyrings
sudo curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
sudo chmod a+r /etc/apt/keyrings/docker.asc
echo \
  "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu \
  $(. /etc/os-release && echo "$VERSION_CODENAME") stable" | \
  sudo tee /etc/apt/sources.list.d/docker.list > /dev/null
sudo apt update
sudo apt install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

sudo useradd -m -s /bin/bash liquidator
sudo usermod -aG docker liquidator

# Clone and configure
su - liquidator
mkdir -p /opt/templar-liquidator && cd /opt/templar-liquidator
git clone https://github.com/Templar-Protocol/templar-liquidator.git repo
cd repo
cp .env.example .env
nano .env   # your credentials — .env.example ships DRY_RUN=true; keep it while you validate

# Build and run
docker compose build                             # builds via docker-compose.yml's `build:` section, tags templar-liquidator:latest
docker compose -f docker-compose.prod.yml up -d   # docker-compose.prod.yml has no `build:` of its own — it runs that same tag with prod-tuned settings; same DRY_RUN=${DRY_RUN:-false} asymmetry as --build-local (step 5 above) applies here too, so don't delete that line from .env

# Monitor
docker compose -f docker-compose.prod.yml logs -f
```

## Optional: Grafana + Loki log aggregation

`scripts/setup-loki-grafana.sh` installs Grafana + Loki + Promtail on the server as native systemd services (not Docker containers) for a searchable log dashboard, if `docker compose logs` isn't enough for your operation. Download it, read it, then run it — don't pipe a remote script straight into a root shell:

```bash
ssh liquidator@YOUR_SERVER_IP
curl -fsSL -o setup-loki-grafana.sh https://raw.githubusercontent.com/Templar-Protocol/templar-liquidator/main/scripts/setup-loki-grafana.sh
less setup-loki-grafana.sh   # read it before running anything as root
sudo bash setup-loki-grafana.sh
```

(If you deployed via git-deploy, the script is already on the server at `/opt/templar-liquidator/repo/scripts/setup-loki-grafana.sh` — no need to re-download it.)

Neither service authenticates at the transport level — Loki runs with `auth_enabled: false` and Grafana ships the default `admin`/`admin` credentials until you change them — so the script binds both to loopback (`127.0.0.1`) by default and opens no firewall rule for either. Reach them from your local machine over an SSH tunnel instead of a public port:

```bash
ssh -L 3000:localhost:3000 -L 3100:localhost:3100 liquidator@YOUR_SERVER_IP
```

...then open `http://localhost:3000` in your local browser and change the Grafana admin password on first login.

Exposing either service beyond loopback is a deliberate, separate opt-in the script does *not* do for you: edit `http_addr` in `/etc/grafana/grafana.ini` and `http_listen_address` in `/etc/loki/config.yaml` (and `/etc/promtail/config.yaml`), open the relevant port in `ufw`, and put a real authenticating TLS reverse proxy in front — don't expose Loki directly, and don't leave Grafana on its default password if you do this. A `ufw allow` rule alone would not be enough if you ever containerized this stack instead: Docker's published-port `DNAT` rules are evaluated ahead of UFW's `INPUT` chain, so a `0.0.0.0`-published container port stays reachable from the internet even with UFW otherwise locked down to 22/80 — the same reason the bot's own `HTTP_PORT` mapping in `docker-compose.yml`/`docker-compose.prod.yml` binds its host side to `127.0.0.1` explicitly rather than relying on a firewall rule alone (see [Metrics and health](../README.md#metrics-and-health)).

## Monitoring and maintenance

```bash
docker compose logs -f                  # real-time logs
docker compose logs --tail 100          # last 100 lines
docker compose logs | grep "liquidation"

docker compose ps
docker stats templar-liquidator --no-stream

docker compose restart
```

Update an existing deployment:

```bash
./scripts/deploy.sh YOUR_SERVER_IP --update
```

`--update` only works against a **git-deploy** layout — it looks for `/opt/templar-liquidator/repo` and exits with an error if that directory doesn't exist, so it won't touch a `--build-local` deployment (re-run `./scripts/deploy.sh YOUR_SERVER_IP --build-local` to update that one instead). It also doesn't touch `.env` or print which `DRY_RUN` mode it restarted in the way a fresh deploy does — whatever `.env` already has in place carries over unchanged, so check `docker compose logs -f` yourself after it finishes if you want to confirm.

...or manually:

```bash
cd /opt/templar-liquidator/repo
git pull origin main
docker compose down
docker compose build     # docker-compose.prod.yml has no `build:` of its own; rebuild via docker-compose.yml
docker compose up -d
```

## Troubleshooting

**Container won't start**
```bash
docker compose logs
df -h                        # disk space
sudo systemctl status docker
```

**Out of memory**
```bash
free -h
docker stats
# Consider a bigger instance, or lowering the cpus/memory limits in docker-compose.prod.yml
```

**Can't connect over SSH**
```bash
ssh -v liquidator@YOUR_SERVER_IP
ssh-add -l
ssh-copy-id liquidator@YOUR_SERVER_IP   # re-copy the key
```

**Logs missing from Grafana** (only relevant if you installed Loki)
```bash
sudo systemctl status promtail
sudo systemctl status loki
sudo systemctl status grafana-server
sudo journalctl -u promtail -n 50
```
