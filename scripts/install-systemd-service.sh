#!/usr/bin/env bash

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 <dev|stg>" >&2
  exit 1
fi

ENVIRONMENT="$1"

case "$ENVIRONMENT" in
  dev)
    DESCRIPTION="Bluesky Push Notifier (Dev Environment)"
    AFTER="network.target"
    WANTS="network.target"
    SYSLOG_IDENTIFIER="bluesky-push-notifier-dev"
    ;;
  stg)
    DESCRIPTION="Bluesky Push Notifier (Staging/TestFlight Environment)"
    AFTER="network.target postgresql.service"
    WANTS="postgresql.service"
    SYSLOG_IDENTIFIER="bluesky-push-notifier-stg"
    ;;
  *)
    echo "Unknown environment: $ENVIRONMENT" >&2
    exit 1
    ;;
esac

SERVICE_NAME="bluesky-push-notifier-${ENVIRONMENT}.service"

sudo tee "/etc/systemd/system/${SERVICE_NAME}" >/dev/null <<UNIT
[Unit]
Description=${DESCRIPTION}
After=${AFTER}
Wants=${WANTS}

[Service]
Type=simple
User=ubuntu
Group=ubuntu
WorkingDirectory=/home/ubuntu/bluesky-push-notifier
ExecStart=/usr/bin/doppler run --config ${ENVIRONMENT} -- /home/ubuntu/bluesky-push-notifier/target/release/bluesky-push-notifier
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal
SyslogIdentifier=${SYSLOG_IDENTIFIER}
LimitNOFILE=65536
MemoryHigh=1G
MemoryMax=2G

[Install]
WantedBy=multi-user.target
UNIT

sudo systemctl daemon-reload
sudo systemctl enable "${SERVICE_NAME}"
sudo systemctl restart "${SERVICE_NAME}"

echo "Updated systemd unit: ${SERVICE_NAME}"
