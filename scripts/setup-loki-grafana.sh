#!/usr/bin/env bash
#
# Grafana + Loki Log Aggregation Setup
# 
# This script installs and configures:
#   - Grafana Loki: Log aggregation backend (stores logs for 30 days)
#   - Promtail: Log collector (reads Docker container logs)
#   - Grafana: Visualization and query UI
#
# Requirements:
#   - Ubuntu 24.04 LTS
#   - Docker installed and running
#   - Root or sudo access
#
# Usage:
#   sudo bash setup-loki-grafana.sh
#
# Neither service is authenticated at the transport level (Loki runs with
# auth_enabled: false; Grafana ships default admin/admin credentials until
# changed), so both are bound to loopback (127.0.0.1) by default -- the same
# posture the bot's own /healthz+/metrics port uses in docker-compose.yml.
# Nothing is opened in the firewall for them.
#
# After installation (default, loopback-only):
#   - Grafana UI: http://localhost:3000 on the server itself, or via an SSH
#     tunnel from your workstation: ssh -L 3000:localhost:3000 <user>@<host>
#   - Default credentials: admin / admin (change on first login)
#   - Loki API: http://localhost:3100
#
# Exposing either service beyond loopback is an explicit, separate opt-in:
# edit http_addr in /etc/grafana/grafana.ini and http_listen_address in
# /etc/loki/config.yaml, open the relevant port in ufw, and put a real
# authenticating reverse proxy (TLS + credentials) in front -- do not expose
# Loki directly, and do not leave Grafana on its default admin/admin password
# if you do this.
#
set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Check if running as root
if [ "$EUID" -ne 0 ]; then 
    log_error "Please run as root"
    exit 1
fi

log_info "Installing Grafana Loki + Grafana for liquidator monitoring..."

# Install prerequisites
log_info "Installing prerequisites..."
apt-get update -qq
apt-get install -y unzip wget curl gpg

# Create directories
mkdir -p /opt/loki
mkdir -p /var/lib/loki/{index,chunks}
mkdir -p /etc/loki
mkdir -p /etc/promtail

# Release binaries are verified against Grafana's own published SHA256SUMS
# for this exact pinned version (fetched from
# https://github.com/grafana/loki/releases/tag/v3.3.2 and copied in here, not
# re-downloaded at install time) so a compromised mirror or tampered
# in-transit download is caught before the archive is ever extracted or its
# binary executed as a systemd service.
LOKI_VERSION="3.3.2"
LOKI_ZIP_SHA256="dd2dbf4d91a607a87207ffc20ae92370c1b1e22e66688a90f723a11549e8bd69"
PROMTAIL_ZIP_SHA256="ab5ab317c800b4804839832a9838834a23f487398671587f81e074fb561d0757"

verify_checksum() {
    local file="$1"
    local expected="$2"
    local actual
    actual="$(sha256sum "$file" | awk '{print $1}')"
    if [ "$actual" != "$expected" ]; then
        log_error "Checksum mismatch for $file"
        log_error "  expected: $expected"
        log_error "  actual:   $actual"
        exit 1
    fi
}

# Download Loki
log_info "Downloading Loki..."
cd /opt/loki
wget -q https://github.com/grafana/loki/releases/download/v${LOKI_VERSION}/loki-linux-amd64.zip
verify_checksum loki-linux-amd64.zip "$LOKI_ZIP_SHA256"
unzip -o loki-linux-amd64.zip
chmod +x loki-linux-amd64
rm loki-linux-amd64.zip

# Download Promtail
log_info "Downloading Promtail..."
wget -q https://github.com/grafana/loki/releases/download/v${LOKI_VERSION}/promtail-linux-amd64.zip
verify_checksum promtail-linux-amd64.zip "$PROMTAIL_ZIP_SHA256"
unzip -o promtail-linux-amd64.zip
chmod +x promtail-linux-amd64
rm promtail-linux-amd64.zip

# Create Loki config
log_info "Creating Loki configuration..."
cat > /etc/loki/config.yaml <<'EOF'
auth_enabled: false

server:
  # Loki has no authentication of its own (auth_enabled: false above), so it
  # is bound to loopback by default -- the same posture the bot's own
  # /healthz+/metrics port uses. See the header comment for how to opt in to
  # remote access deliberately.
  http_listen_address: 127.0.0.1
  http_listen_port: 3100
  grpc_listen_address: 127.0.0.1
  grpc_listen_port: 9096

common:
  path_prefix: /var/lib/loki
  storage:
    filesystem:
      chunks_directory: /var/lib/loki/chunks
      rules_directory: /var/lib/loki/rules
  replication_factor: 1
  ring:
    kvstore:
      store: inmemory

schema_config:
  configs:
    - from: 2020-10-24
      store: boltdb-shipper
      object_store: filesystem
      schema: v11
      index:
        prefix: index_
        period: 24h

ruler:
  alertmanager_url: http://localhost:9093

# Retention - keep 30 days
limits_config:
  retention_period: 720h
  enforce_metric_name: false
  reject_old_samples: true
  reject_old_samples_max_age: 168h
  ingestion_rate_mb: 10
  ingestion_burst_size_mb: 20

chunk_store_config:
  max_look_back_period: 0s

table_manager:
  retention_deletes_enabled: true
  retention_period: 720h

compactor:
  working_directory: /var/lib/loki/compactor
  shared_store: filesystem
  compaction_interval: 10m
  retention_enabled: true
  retention_delete_delay: 2h
  retention_delete_worker_count: 150
EOF

# Create Promtail config
log_info "Creating Promtail configuration..."
cat > /etc/promtail/config.yaml <<'EOF'
server:
  # Unauthenticated like Loki above; loopback-only for the same reason.
  http_listen_address: 127.0.0.1
  http_listen_port: 9080
  grpc_listen_port: 0

positions:
  filename: /var/lib/promtail/positions.yaml

clients:
  - url: http://localhost:3100/loki/api/v1/push

scrape_configs:
  - job_name: docker
    docker_sd_configs:
      - host: unix:///var/run/docker.sock
        refresh_interval: 5s
    relabel_configs:
      - source_labels: ['__meta_docker_container_name']
        regex: '/(.*)'
        target_label: 'container'
      - source_labels: ['__meta_docker_container_log_stream']
        target_label: 'stream'
    pipeline_stages:
      - docker: {}
      - regex:
          expression: '^(?P<timestamp>\S+)\s+(?P<level>\w+)\s+(?P<message>.*)$'
      - labels:
          level:
          
  - job_name: system
    static_configs:
      - targets:
          - localhost
        labels:
          job: varlogs
          __path__: /var/log/*log
EOF

# Create Promtail positions directory
mkdir -p /var/lib/promtail

# Set permissions
chown -R liquidator:liquidator /var/lib/loki
chown -R liquidator:liquidator /var/lib/promtail

# Create Loki systemd service
log_info "Creating Loki service..."
cat > /etc/systemd/system/loki.service <<'EOF'
[Unit]
Description=Loki - Log Aggregation System
Documentation=https://grafana.com/docs/loki/latest/
After=network-online.target

[Service]
Type=simple
User=liquidator
Group=liquidator
ExecStart=/opt/loki/loki-linux-amd64 -config.file=/etc/loki/config.yaml
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

# Create Promtail systemd service
log_info "Creating Promtail service..."
cat > /etc/systemd/system/promtail.service <<'EOF'
[Unit]
Description=Promtail - Log Collector for Loki
Documentation=https://grafana.com/docs/loki/latest/
After=network-online.target

[Service]
Type=simple
User=root
Group=root
ExecStart=/opt/loki/promtail-linux-amd64 -config.file=/etc/promtail/config.yaml
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

# Install Grafana
log_info "Installing Grafana..."
apt-get install -y apt-transport-https software-properties-common wget
mkdir -p /etc/apt/keyrings/
wget -q -O - https://apt.grafana.com/gpg.key | gpg --dearmor > /etc/apt/keyrings/grafana.gpg
echo "deb [signed-by=/etc/apt/keyrings/grafana.gpg] https://apt.grafana.com stable main" | tee /etc/apt/sources.list.d/grafana.list
apt-get update
apt-get install -y grafana

# Bind Grafana to loopback by default: it ships with default admin/admin
# credentials until changed on first login, so leaving it reachable from
# anywhere on a freshly-provisioned server is a real window of exposure.
# Matches the loopback-only posture used for Loki/Promtail above and for the
# bot's own /healthz+/metrics port. Handles both the stock commented-out
# line and an already-set one.
sed -i -E 's/^;?\s*http_addr\s*=.*/http_addr = 127.0.0.1/' /etc/grafana/grafana.ini

# Configure Grafana datasource
log_info "Configuring Grafana datasource..."
mkdir -p /etc/grafana/provisioning/datasources
cat > /etc/grafana/provisioning/datasources/loki.yaml <<'EOF'
apiVersion: 1

datasources:
  - name: Loki
    type: loki
    access: proxy
    url: http://localhost:3100
    isDefault: true
    editable: true
EOF

# Create dashboard for liquidator
log_info "Creating liquidator dashboard..."
mkdir -p /etc/grafana/provisioning/dashboards
cat > /etc/grafana/provisioning/dashboards/dashboard.yaml <<'EOF'
apiVersion: 1

providers:
  - name: 'default'
    orgId: 1
    folder: ''
    type: file
    disableDeletion: false
    updateIntervalSeconds: 10
    allowUiUpdates: true
    options:
      path: /var/lib/grafana/dashboards
EOF

mkdir -p /var/lib/grafana/dashboards
cat > /var/lib/grafana/dashboards/liquidator.json <<'EOF'
{
  "dashboard": {
    "title": "Templar Liquidator",
    "tags": ["liquidator", "templar"],
    "timezone": "browser",
    "panels": [
      {
        "id": 1,
        "title": "Liquidation Events",
        "type": "logs",
        "gridPos": {"x": 0, "y": 0, "w": 24, "h": 8},
        "targets": [
          {
            "expr": "{container=\"templar-liquidator\"} |~ \"liquidat\""
          }
        ]
      },
      {
        "id": 2,
        "title": "Errors",
        "type": "logs",
        "gridPos": {"x": 0, "y": 8, "w": 24, "h": 8},
        "targets": [
          {
            "expr": "{container=\"templar-liquidator\"} |~ \"ERROR|WARN\""
          }
        ]
      },
      {
        "id": 3,
        "title": "Market Scans",
        "type": "logs",
        "gridPos": {"x": 0, "y": 16, "w": 24, "h": 8},
        "targets": [
          {
            "expr": "{container=\"templar-liquidator\"} |~ \"Scanning market\""
          }
        ]
      }
    ],
    "schemaVersion": 16,
    "version": 0
  }
}
EOF

chown -R grafana:grafana /var/lib/grafana

# No firewall rules opened for Grafana (3000) or Loki (3100) here on purpose:
# both are bound to loopback above, so a ufw allow rule for them would just
# be an unused hole -- and, separately, would NOT actually gate access if
# either service were ever run in a Docker container instead, since Docker's
# published-port DNAT is evaluated ahead of ufw's INPUT chain. Exposing
# either service is a deliberate, separate opt-in (see the header comment).

# Start services
log_info "Starting services..."
systemctl daemon-reload
systemctl enable loki promtail grafana-server
systemctl restart loki
sleep 2
systemctl restart promtail
systemctl restart grafana-server

# Wait for services to start
sleep 5

# Check status
log_info "Checking service status..."
systemctl is-active --quiet loki && log_info "✓ Loki is running" || log_error "✗ Loki failed to start"
systemctl is-active --quiet promtail && log_info "✓ Promtail is running" || log_error "✗ Promtail failed to start"
systemctl is-active --quiet grafana-server && log_info "✓ Grafana is running" || log_error "✗ Grafana failed to start"

# Get server IP (for the SSH tunnel hint below only -- Grafana/Loki are not
# reachable at this address; see the loopback-binding note above)
SERVER_IP=$(curl -s ifconfig.me)

log_info ""
log_info "=========================================="
log_info "Installation complete!"
log_info "=========================================="
log_info ""
log_info "Grafana and Loki are bound to loopback (127.0.0.1) and are NOT"
log_info "reachable from outside this server -- no firewall rule opens them."
log_info ""
log_info "Grafana URL (from the server itself): http://localhost:3000"
log_info "Grafana URL (from your workstation, via SSH tunnel):"
log_info "  ssh -L 3000:localhost:3000 root@${SERVER_IP}"
log_info "  then open http://localhost:3000 locally"
log_info "Default credentials:"
log_info "  Username: admin"
log_info "  Password: admin"
log_info ""
log_info "Loki endpoint: http://localhost:3100 (loopback only)"
log_info ""
log_info "To expose either service beyond loopback, see the deliberate"
log_info "opt-in steps in this script's header comment -- do not do this"
log_info "without an authenticating TLS reverse proxy in front."
log_info ""
log_info "Next steps:"
log_info "  1. Open Grafana (locally or via the SSH tunnel above)"
log_info "  2. Login with admin/admin (you'll be prompted to change password)"
log_info "  3. Go to Explore > Select 'Loki' datasource"
log_info "  4. Query: {container=\"templar-liquidator\"}"
log_info ""
log_info "Useful queries:"
log_info "  All liquidator logs:     {container=\"templar-liquidator\"}"
log_info "  Liquidation events:      {container=\"templar-liquidator\"} |~ \"liquidated=\""
log_info "  Errors only:             {container=\"templar-liquidator\"} |~ \"ERROR\""
log_info "  Specific market:         {container=\"templar-liquidator\"} |~ \"ibtc-usdc\""
log_info ""
log_info "Service management:"
log_info "  systemctl status loki"
log_info "  systemctl status promtail"
log_info "  systemctl status grafana-server"
log_info ""
log_info "Logs are retained for 30 days"
log_info "=========================================="
