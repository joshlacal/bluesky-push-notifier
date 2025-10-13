# Firehose Reliability Improvements

## Problem Summary

The Bluesky push notification server's firehose consumer was experiencing silent failures where it would stop processing events after several hours without logging errors or attempting to reconnect.

### Root Causes Identified

1. **Missing cursor parameter in WebSocket URL**: The firehose was connecting to Bluesky without passing the cursor parameter, causing it to not receive any events after initial connection.

2. **Stream end not handled**: When `subscription.next()` returned `None` (stream ended), the code pattern `Some(frame_result) = subscription.next()` wouldn't match, and the select loop would hang indefinitely.

3. **No heartbeat timeout**: There was no mechanism to detect when the WebSocket connection became stale or stopped receiving data.

4. **Excessive logging suppression**: The firehose module was set to `warn` level, hiding all INFO logs that would show connection status and activity.

## Fixes Implemented

### 1. Added Cursor to WebSocket Subscription (`src/firehose.rs`)

**Before:**
```rust
async fn new(bgs: &str, _cursor: Option<String>) -> Result<Self> {
    let ws_url = format!("wss://{}/xrpc/{}", host, NSID);
    // ... cursor was ignored
}
```

**After:**
```rust
async fn new(bgs: &str, cursor: Option<String>) -> Result<Self> {
    let ws_url = if let Some(cursor_val) = cursor {
        format!("wss://{}/xrpc/{}?cursor={}", host, NSID, cursor_val)
    } else {
        format!("wss://{}/xrpc/{}", host, NSID)
    };
    // ... cursor is now included in the URL
}
```

### 2. Handle Stream End (`src/firehose.rs`)

**Before:**
```rust
tokio::select! {
    Some(frame_result) = subscription.next() => {
        // Only handled when Some is returned
    }
}
```

**After:**
```rust
tokio::select! {
    frame_option = subscription.next() => {
        match frame_option {
            Some(frame_result) => {
                // Handle frame
            },
            None => {
                // Stream ended - reconnect!
                warn!("Firehose stream ended (returned None), reconnecting...");
                break 'inner;
            }
        }
    }
}
```

### 3. Added Heartbeat Timeout (`src/firehose.rs`)

Added a 60-second timeout to detect stale connections:

```rust
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(60);
let mut last_activity = tokio::time::Instant::now();

tokio::select! {
    frame_option = subscription.next() => {
        last_activity = tokio::time::Instant::now(); // Update on activity
        // ... handle frame
    },
    _ = tokio::time::sleep_until(last_activity + HEARTBEAT_TIMEOUT) => {
        warn!("No data received from firehose for {} seconds, reconnecting...", 
              HEARTBEAT_TIMEOUT.as_secs());
        break 'inner;
    }
}
```

### 4. Enabled INFO Logging (`src/logging.rs`)

**Before:**
```rust
"bluesky_push_notifier::firehose=warn",
```

**After:**
```rust
"bluesky_push_notifier::firehose=info",
```

### 5. Removed Debug Clutter (`src/firehose.rs`)

Removed temporary ERROR-level debug logs that were added for troubleshooting.

## Monitoring System

Created an automated monitoring system to ensure services stay healthy.

### Monitor Script (`scripts/monitor-firehose.sh`)

- Checks if both dev and staging services have processed commits in the last 3 minutes
- Verifies health endpoints are responding
- Automatically restarts services if they're stuck or idle
- Logs all actions with timestamps to `firehose-monitor.log`

### Systemd Timer

Runs the monitor script every 5 minutes automatically:

```bash
# Check timer status
sudo systemctl status firehose-monitor.timer

# View monitor logs
journalctl -u firehose-monitor.service -f

# Manually run monitor
/home/ubuntu/bluesky-push-notifier/scripts/monitor-firehose.sh
```

## Testing

To verify the fixes work:

1. **Check firehose is running:**
   ```bash
   sudo journalctl -u bluesky-push-notifier-dev --since "1 minute ago" | grep "Processed commit"
   ```

2. **Run monitor script manually:**
   ```bash
   /home/ubuntu/bluesky-push-notifier/scripts/monitor-firehose.sh
   ```

3. **Watch logs for reconnection behavior:**
   ```bash
   sudo journalctl -u bluesky-push-notifier-dev -f | grep -E "reconnect|stream ended|timeout"
   ```

## Expected Behavior

With these fixes:

1. **Automatic recovery**: If the WebSocket stream ends or times out, the firehose will automatically reconnect
2. **Visible activity**: You can see commit processing in logs at INFO level
3. **Heartbeat protection**: If no data is received for 60 seconds, the connection is reset
4. **Monitor safety net**: Even if all else fails, the monitor will restart stuck services every 5 minutes

## Deployment

Changes have been deployed to both dev (port 8081) and staging (port 8080) environments.

Services are running with:
- Process monitoring via systemd
- Automatic health checks every 5 minutes
- Reconnection logic for network failures
- Heartbeat timeout for stale connections

## Future Improvements

Consider:

1. **Metrics/Alerting**: Add Prometheus metrics for connection uptime, reconnection count, and processing rate
2. **Longer timeout**: If 60s is too aggressive, increase `HEARTBEAT_TIMEOUT`
3. **Circuit breaker**: Add exponential backoff with longer delays after repeated failures
4. **Ping/pong**: Implement WebSocket ping/pong frames if supported by Bluesky's firehose
5. **Database monitoring**: Add checks for cursor table growth and cleanup

## Troubleshooting

**Firehose stopped processing:**
```bash
sudo systemctl restart bluesky-push-notifier-dev  # or -stg
```

**Monitor not running:**
```bash
sudo systemctl start firehose-monitor.timer
sudo systemctl status firehose-monitor.timer
```

**Check what monitor is doing:**
```bash
tail -f /home/ubuntu/bluesky-push-notifier/firehose-monitor.log
```

**See all firehose reconnections:**
```bash
sudo journalctl -u bluesky-push-notifier-dev --since "1 hour ago" | grep -i "reconnect"
```
