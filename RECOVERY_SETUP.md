# Automatic Recovery Setup

This system now has multiple layers of automatic recovery to prevent the issues that caused your 4-day notification outage:

## 1. SystemD Service Recovery (Implemented)
- **Restart Policy**: `Restart=always` with 10-second delay
- **Rate Limiting**: Max 10 restarts per hour to prevent restart loops
- **Location**: `/etc/systemd/system/bluesky-push-notifier.service`

## 2. External Health Supervisor (Implemented)
- **Script**: `/home/ubuntu/bluesky-push-notifier/scripts/health-supervisor.sh`
- **Service**: `bluesky-supervisor.service`
- **Checks every 60 seconds**:
  - Service running status
  - API health endpoint
  - Channel closed errors
  - Event processing activity
  - Memory usage (restarts if >1GB)
- **Auto-restart** after 3 consecutive failures
- **Logs**: `/var/log/bluesky-push-notifier-supervisor.log`

## 3. Internal Recovery (Proposed)
- **Filter Wrapper**: `src/filter_wrapper.rs` - Adds automatic retry logic
- **Supervisor Module**: `src/supervisor.rs` - Task monitoring and restart

## What Happened on September 3rd

The filter task crashed silently without logging an error, breaking the internal channel between the firehose consumer and event filter. This caused:
- Firehose kept receiving events but couldn't pass them to the filter
- No notifications were sent for 4 days
- The service appeared healthy but wasn't processing events

## Prevention Measures

1. **Immediate Recovery**: SystemD will restart the entire service if it crashes
2. **Health Monitoring**: External supervisor checks for "channel closed" errors
3. **Proactive Restart**: Supervisor restarts service if no events processed for too long
4. **Memory Management**: Auto-restart if memory usage exceeds 1GB

## Manual Commands

Check supervisor status:
```bash
sudo systemctl status bluesky-supervisor
tail -f /var/log/bluesky-push-notifier-supervisor.log
```

Check main service:
```bash
sudo systemctl status bluesky-push-notifier
sudo journalctl -u bluesky-push-notifier -f
```

Force restart:
```bash
sudo systemctl restart bluesky-push-notifier
```

## Testing Recovery

To test the recovery mechanism:
1. Kill the main process: `sudo pkill -9 bluesky-push-no`
2. Watch it auto-restart within 10 seconds
3. Check logs to confirm recovery

The supervisor will detect issues like:
- Service crashes
- API not responding
- Channel closed errors
- No events being processed
- High memory usage

With these safeguards, the notification service should automatically recover from most failure scenarios.