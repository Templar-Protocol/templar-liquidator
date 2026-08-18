# Deploying on a VM

A complete guide to running the bot as a long-lived Docker Compose service on any VM you control — a cloud instance, bare metal, whatever. This is the `loop` run mode: one continuous process, restarted by Docker/systemd if it dies, rather than the cron-triggered `--run-mode once` pattern the [GCP Terraform module](deploy-gcp.md) uses.

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

SSH into the server as root and run the initialization script:

```bash
curl -fsSL https://raw.githubusercontent.com/Templar-Protocol/templar-liquidator/main/scripts/init-server.sh | sudo bash
```

This installs Docker + Compose, creates a dedicated `liquidator` user, configures the firewall (UFW: allows 22 and 80), sets up log rotation, and installs a cron-driven watchdog that restarts the container if it's found down.

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

### 5. Start in dry-run

The bot starts in dry-run automatically (`docker-compose.yml` doesn't override `DRY_RUN`, so the binary's own safe-by-default `true` applies). Watch it:

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

```
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
curl -fsSL https://get.docker.com | sudo sh
sudo apt install docker-compose-plugin
sudo useradd -m -s /bin/bash liquidator
sudo usermod -aG docker liquidator

# Clone and configure
su - liquidator
mkdir -p /opt/templar-liquidator && cd /opt/templar-liquidator
git clone https://github.com/Templar-Protocol/templar-liquidator.git repo
cd repo
cp .env.example .env
nano .env   # your credentials

# Build and run
docker compose -f docker-compose.prod.yml build
docker compose -f docker-compose.prod.yml up -d

# Monitor
docker compose logs -f
```

## Optional: Grafana + Loki log aggregation

`scripts/setup-loki-grafana.sh` installs Grafana + Loki + Promtail on the server for a searchable log dashboard, if `docker compose logs` isn't enough for your operation:

```bash
ssh liquidator@YOUR_SERVER_IP
sudo bash /tmp/setup-loki-grafana.sh
```

Grafana listens on port 3000 (`admin`/`admin` by default — change it immediately). Open the additional ports if you install it:

```bash
sudo ufw allow 3000/tcp   # Grafana UI
sudo ufw allow 3100/tcp   # Loki API (optional — can stay localhost-only)
sudo ufw reload
```

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

...or manually:

```bash
cd /opt/templar-liquidator/repo
git pull origin main
docker compose down
docker compose -f docker-compose.prod.yml build
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
