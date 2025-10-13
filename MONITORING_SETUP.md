# Monitoring Setup for Bluesky Push Notifier

## Overview

An automated monitoring system has been deployed to ensure the firehose consumers remain healthy and automatically recover from failures.

## Components

### 1. Health Monitor Script
**Location:** `/home/ubuntu/bluesky-push-notifier/scripts/monitor-firehose.sh`

**What it does:**
- Checks if dev and staging services have processed commits in the last 3 minutes
- Verifies HTTP health endpoints are responding
- Automatically restarts services if they become idle or unresponsive
- Logs all actions with timestamps

**Manual execution:**
```bash
/home/ubuntu/bluesky-push-notifier/scripts/monitor-firehose.sh
```

**Example output:**
```
[2025-10-10 01:23:28] === Starting Firehose Health Check ===
[2025-10-10 01:23:28] ✅ DEV firehose healthy (last seq: 14180906000)
[2025-10-10 01:23:28] ✅ STG firehose healthy (last seq: 14180824000)
[2025-10-10 01:23:28] ✅ All services healthy
```

### 2. Systemd Timer
**Service:** `firehose-monitor.service`  
**Timer:** `firehose-monitor.timer`

**Schedule:** Runs every 5 minutes, starting 2 minutes after boot

**Commands:**
```bash
# Check timer status
sudo systemctl status firehose-monitor.timer

# View timer schedule
sudo systemctl list-timers firehose-monitor.timer

# View monitor execution logs
sudo journalctl -u firehose-monitor.service -f

# Manually trigger monitor
sudo systemctl start firehose-monitor.service

# Disable monitoring (not recommended)
sudo systemctl stop firehose-monitor.timer
sudo systemctl disable firehose-monitor.timer

# Re-enable monitoring
sudo systemctl enable firehose-monitor.timer
sudo systemctl start firehose-monitor.timer
```

### 3. Monitor Log File
**Location:** `/home/ubuntu/bluesky-push-notifier/firehose-monitor.log`

**View log:**
```bash
tail -f /home/ubuntu/bluesky-push-notifier/firehose-monitor.log
```

## Service Health Checks

### Quick Status Check
```bash
# Check both services
systemctl status bluesky-push-notifier-dev bluesky-push-notifier-stg

# Check if firehose is processing
sudo journalctl -u bluesky-push-notifier-dev --since "1 minute ago" | grep "Processed commit" | tail -n 5
sudo journalctl -u bluesky-push-notifier-stg --since "1 minute ago" | grep "Processed commit" | tail -n 5
```

### Manual Service Management
```bash
# Restart a service
sudo systemctl restart bluesky-push-notifier-dev
sudo systemctl restart bluesky-push-notifier-stg

# View real-time logs
sudo journalctl -u bluesky-push-notifier-dev -f
sudo journalctl -u bluesky-push-notifier-stg -f

# Check for reconnection events
sudo journalctl -u bluesky-push-notifier-dev --since "1 hour ago" | grep -i "reconnect"
```

### Health Endpoints
```bash
# Dev service (port 8081)
curl http://localhost:8081/health

# Staging service (port 8080)  
curl http://localhost:8080/health
```

## Alert Conditions

The monitor will restart a service if:
1. No commits have been processed in the last 3 minutes
2. The health endpoint is not responding
3. The service is not running

## Log Rotation

To prevent monitor logs from growing too large:

```bash
# Add to /etc/logrotate.d/bluesky-push-notifier
sudo tee /etc/logrotate.d/bluesky-push-notifier << 'LOGROTATE'
/home/ubuntu/bluesky-push-notifier/firehose-monitor.log {
    daily
    rotate 7
    compress
    missingok
    notifempty
    create 0644 ubuntu ubuntu
}
LOGROTATE
```

## Troubleshooting

### Monitor is not running
```bash
sudo systemctl start firehose-monitor.timer
sudo systemctl enable firehose-monitor.timer
sudo systemctl status firehose-monitor.timer
```

### Services keep restarting
```bash
# Check for errors in service logs
sudo journalctl -u bluesky-push-notifier-dev --since "1 hour ago" | grep -i error

# Check monitor log for restart reasons
tail -n 50 /home/ubuntu/bluesky-push-notifier/firehose-monitor.log
```

### No commits being processed
```bash
# Check firehose connection
sudo journalctl -u bluesky-push-notifier-dev --since "5 minutes ago" | grep -E "Connecting|WebSocket|stream ended"

# Manually restart
sudo systemctl restart bluesky-push-notifier-dev
```

## Monitoring Best Practices

1. **Check monitor log daily:**
   ```bash
   tail -n 100 /home/ubuntu/bluesky-push-notifier/firehose-monitor.log
   ```

2. **Set up external monitoring** (optional):
   - Use UptimeRobot or similar to ping health endpoints
   - Alert if endpoint returns non-200 status

3. **Review restart patterns:**
   - Frequent restarts may indicate network issues
   - Check monitor log for patterns

4. **Monitor system resources:**
   ```bash
   # Check memory usage
   ps aux | grep bluesky-push-notifier
   
   # Check CPU usage
   top -p $(pgrep -f bluesky-push-notifier | tr '\n' ',')
   ```

## Configuration

To adjust monitor sensitivity, edit `/home/ubuntu/bluesky-push-notifier/scripts/monitor-firehose.sh`:

```bash
MAX_IDLE_MINUTES=3  # Change to 5 for less aggressive monitoring
```

After changes:
```bash
sudo systemctl restart firehose-monitor.timer
```
