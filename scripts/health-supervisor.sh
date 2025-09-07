#!/bin/bash

# Bluesky Push Notifier Health Supervisor
# This script monitors the service health and restarts it if needed

SERVICE_NAME="bluesky-push-notifier"
LOG_FILE="/var/log/${SERVICE_NAME}-supervisor.log"
CHECK_INTERVAL=60  # Check every 60 seconds
MAX_FAILURES=3     # Restart after 3 consecutive failures
FAILURE_COUNT=0

# Function to log messages
log_message() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $1" | tee -a "$LOG_FILE"
}

# Function to check if the service is healthy
check_health() {
    # Check 1: Is the service running?
    if ! systemctl is-active --quiet "$SERVICE_NAME"; then
        log_message "ERROR: Service is not running"
        return 1
    fi
    
    # Check 2: Is the API responding?
    if ! curl -s -f http://localhost:8080/health > /dev/null 2>&1; then
        log_message "ERROR: API health check failed"
        return 1
    fi
    
    # Check 3: Are there recent "channel closed" errors?
    RECENT_ERRORS=$(journalctl -u "$SERVICE_NAME" --since "5 minutes ago" 2>/dev/null | grep -c "channel closed")
    if [ "$RECENT_ERRORS" -gt 10 ]; then
        log_message "ERROR: Detected $RECENT_ERRORS 'channel closed' errors in last 5 minutes"
        return 1
    fi
    
    # Check 4: Is the firehose processing events?
    LAST_EVENT=$(journalctl -u "$SERVICE_NAME" --since "5 minutes ago" 2>/dev/null | grep -c "Processed commit")
    if [ "$LAST_EVENT" -eq 0 ]; then
        log_message "WARNING: No events processed in last 5 minutes"
        # Don't fail immediately, could be a quiet period
    fi
    
    # Check 5: Memory usage (restart if using more than 1GB)
    PID=$(systemctl show -p MainPID "$SERVICE_NAME" | cut -d= -f2)
    if [ "$PID" != "0" ]; then
        MEM_KB=$(ps -o rss= -p "$PID" 2>/dev/null | tr -d ' ')
        if [ -n "$MEM_KB" ] && [ "$MEM_KB" -gt 1048576 ]; then  # 1GB in KB
            log_message "WARNING: High memory usage: $(($MEM_KB / 1024))MB"
            return 1
        fi
    fi
    
    return 0
}

# Function to restart the service
restart_service() {
    log_message "Restarting $SERVICE_NAME service..."
    systemctl restart "$SERVICE_NAME"
    sleep 10  # Give it time to start
    
    if systemctl is-active --quiet "$SERVICE_NAME"; then
        log_message "Service restarted successfully"
        FAILURE_COUNT=0
        return 0
    else
        log_message "ERROR: Failed to restart service"
        return 1
    fi
}

# Main monitoring loop
log_message "Starting health supervisor for $SERVICE_NAME"

while true; do
    if check_health; then
        if [ $FAILURE_COUNT -gt 0 ]; then
            log_message "Service recovered, resetting failure count"
        fi
        FAILURE_COUNT=0
    else
        FAILURE_COUNT=$((FAILURE_COUNT + 1))
        log_message "Health check failed (failure count: $FAILURE_COUNT/$MAX_FAILURES)"
        
        if [ $FAILURE_COUNT -ge $MAX_FAILURES ]; then
            log_message "Maximum failures reached, attempting restart"
            if ! restart_service; then
                log_message "CRITICAL: Failed to restart service after $MAX_FAILURES failures"
                # Could send an alert here (email, webhook, etc.)
            fi
        fi
    fi
    
    sleep $CHECK_INTERVAL
done