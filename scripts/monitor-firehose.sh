#!/bin/bash
#
# Firehose Health Monitor Script
# Checks if both dev and staging firehose consumers are actively processing commits
# Run this every 5 minutes via cron to ensure services stay healthy
#

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOG_FILE="$SCRIPT_DIR/../firehose-monitor.log"
MAX_IDLE_MINUTES=3  # Alert if no commits processed in last 3 minutes

# Function to log with timestamp
log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $1" | tee -a "$LOG_FILE"
}

# Function to check if a service is processing commits
check_service() {
    local service_name=$1
    local env_name=$2
    
    # Get the last firehose commit log from the last 5 minutes
    local last_commit=$(sudo journalctl -u "$service_name" --since "${MAX_IDLE_MINUTES} minutes ago" --no-pager 2>/dev/null | grep "Processed commit batch" | tail -n 1)
    
    if [ -z "$last_commit" ]; then
        log "⚠️  WARNING: $env_name firehose hasn't processed commits in ${MAX_IDLE_MINUTES}+ minutes"
        
        # Check if service is running
        if ! systemctl is-active --quiet "$service_name"; then
            log "❌ ERROR: $env_name service is not running! Attempting restart..."
            sudo systemctl restart "$service_name"
            sleep 3
            if systemctl is-active --quiet "$service_name"; then
                log "✅ $env_name service restarted successfully"
            else
                log "❌ CRITICAL: Failed to restart $env_name service"
                return 1
            fi
        else
            log "🔄 $env_name service is running but idle. Attempting restart..."
            sudo systemctl restart "$service_name"
            sleep 3
            log "✅ $env_name service restarted"
        fi
        return 2  # Return code 2 = service was restarted
    else
        # Extract sequence number from log
        local seq=$(echo "$last_commit" | grep -oP 'seq=\K[0-9]+' || echo "unknown")
        log "✅ $env_name firehose healthy (last seq: $seq)"
        return 0
    fi
}

# Function to check service health endpoint
check_health_endpoint() {
    local port=$1
    local env_name=$2
    
    if curl -sf "http://localhost:$port/health" > /dev/null 2>&1; then
        return 0
    else
        log "⚠️  WARNING: $env_name health endpoint (port $port) not responding"
        return 1
    fi
}

log "=== Starting Firehose Health Check ==="

# Check both services
dev_status=0
stg_status=0

check_service "bluesky-push-notifier-dev" "DEV" || dev_status=$?
check_service "bluesky-push-notifier-stg" "STG" || stg_status=$?

# Check health endpoints
check_health_endpoint 8081 "DEV" || dev_status=1
check_health_endpoint 8080 "STG" || stg_status=1

# Summary
if [ $dev_status -eq 0 ] && [ $stg_status -eq 0 ]; then
    log "✅ All services healthy"
    exit 0
elif [ $dev_status -eq 2 ] || [ $stg_status -eq 2 ]; then
    log "⚠️  One or more services were restarted"
    exit 0
else
    log "❌ One or more services have issues"
    exit 1
fi
