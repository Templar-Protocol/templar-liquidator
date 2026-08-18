#!/usr/bin/env bash
# Server initialization script for Hetzner
# Run this once on a fresh Ubuntu 24.04 server
# Usage: bash <(curl -s https://raw.githubusercontent.com/Templar-Protocol/templar-liquidator/main/scripts/init-server.sh)

set -e

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${GREEN}=== Templar Liquidator Server Setup ===${NC}"
echo ""

# Check if running as root
if [ "$EUID" -ne 0 ]; then 
    echo "Please run as root (or use sudo)"
    exit 1
fi

echo "1. Updating system packages..."
apt update && apt upgrade -y

echo "2. Installing Docker..."
if ! command -v docker &> /dev/null; then
    # Installed from Docker's own apt repository (GPG-signed packages, apt
    # verifies the signature itself) rather than piping the get.docker.com
    # convenience script into root: that script is fetched over HTTPS but is
    # otherwise unauthenticated -- nothing checks it hasn't changed between
    # review and execution. This mirrors how setup-loki-grafana.sh installs
    # Grafana (its own signed apt repo), so both scripts follow the same
    # pattern.
    apt-get update
    apt-get install -y ca-certificates curl gnupg
    install -m 0755 -d /etc/apt/keyrings
    curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
    chmod a+r /etc/apt/keyrings/docker.asc

    ARCH="$(dpkg --print-architecture)"
    # shellcheck disable=SC1091
    CODENAME="$(. /etc/os-release && echo "$VERSION_CODENAME")"
    echo "deb [arch=${ARCH} signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu ${CODENAME} stable" \
        > /etc/apt/sources.list.d/docker.list
    apt-get update
    apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

    systemctl enable docker
    systemctl start docker
else
    echo "Docker already installed"
fi

echo "3. Installing Docker Compose..."
apt install -y docker-compose-plugin

echo "4. Installing utilities..."
apt install -y htop vim git curl wget ncdu jq

echo "5. Creating liquidator user..."
if ! id -u liquidator &> /dev/null; then
    useradd -m -s /bin/bash liquidator
    usermod -aG docker liquidator
    echo "liquidator ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers.d/liquidator
    
    # Copy SSH keys from root
    if [ -d /root/.ssh ]; then
        mkdir -p /home/liquidator/.ssh
        cp /root/.ssh/authorized_keys /home/liquidator/.ssh/ 2>/dev/null || true
        chown -R liquidator:liquidator /home/liquidator/.ssh
        chmod 700 /home/liquidator/.ssh
        chmod 600 /home/liquidator/.ssh/authorized_keys 2>/dev/null || true
    fi
else
    echo "User liquidator already exists"
fi

echo "6. Creating application directory..."
mkdir -p /opt/templar-liquidator
chown -R liquidator:liquidator /opt/templar-liquidator

echo "7. Configuring firewall..."
if command -v ufw &> /dev/null; then
    # Allow rules MUST land before the firewall is enabled: ufw's default
    # incoming policy is deny, so enabling it first -- even briefly -- drops
    # the active SSH session and locks the operator out of the box.
    ufw allow 22/tcp
    ufw allow 80/tcp
    ufw --force enable
    echo "Firewall configured"
fi

echo "8. Optimizing system settings..."
# Increase file limits
cat >> /etc/security/limits.conf << EOF
liquidator soft nofile 65536
liquidator hard nofile 65536
EOF

# Optimize sysctl
cat >> /etc/sysctl.conf << EOF
# Networking optimizations
net.core.rmem_max = 134217728
net.core.wmem_max = 134217728
net.ipv4.tcp_rmem = 4096 87380 67108864
net.ipv4.tcp_wmem = 4096 65536 67108864
EOF
sysctl -p

echo "9. Setting up log rotation..."
cat > /etc/logrotate.d/templar-liquidator << EOF
/opt/templar-liquidator/logs/*.log {
    daily
    rotate 7
    compress
    delaycompress
    missingok
    notifempty
    create 644 liquidator liquidator
}
EOF

echo "10. Creating monitoring script..."
cat > /opt/templar-liquidator/monitor.sh << 'EOF'
#!/bin/bash
# Simple health check script

APP_DIR="/opt/templar-liquidator"
LOG_FILE="/var/log/liquidator-monitor.log"

if [ -d "$APP_DIR/repo" ]; then
    cd "$APP_DIR/repo"
else
    cd "$APP_DIR"
fi

if ! docker compose ps | grep -q "Up"; then
    echo "[$(date)] ALERT: Liquidator is down! Attempting restart..." >> "$LOG_FILE"
    docker compose up -d >> "$LOG_FILE" 2>&1
else
    echo "[$(date)] OK: Liquidator is running" >> "$LOG_FILE"
fi
EOF

chmod +x /opt/templar-liquidator/monitor.sh

# Add to crontab for liquidator user
(crontab -u liquidator -l 2>/dev/null; echo "*/5 * * * * /opt/templar-liquidator/monitor.sh") | crontab -u liquidator -

echo ""
echo -e "${GREEN}=== Setup Complete! ===${NC}"
echo ""
echo "Summary:"
echo "  ✓ System updated"
echo "  ✓ Docker installed"
echo "  ✓ User 'liquidator' created"
echo "  ✓ Application directory: /opt/templar-liquidator"
echo "  ✓ Firewall configured"
echo "  ✓ Monitoring script installed"
echo ""
echo "Next steps:"
echo "  1. Logout and login as 'liquidator' user"
echo "  2. Deploy liquidator using deployment script"
echo "  3. Configure .env file with your credentials"
echo ""
echo "Commands:"
echo "  ssh liquidator@$(hostname -I | awk '{print $1}')"
echo ""
