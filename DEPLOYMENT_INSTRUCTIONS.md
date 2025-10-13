# App Attest Fix - Deployment Instructions

## Quick Deploy

```bash
cd /home/ubuntu/bluesky-push-notifier
chmod +x quick-deploy.sh
./quick-deploy.sh
```

Or manually:

```bash
cd /home/ubuntu/bluesky-push-notifier
cargo build --release
sudo systemctl restart bluesky-push-notifier-dev.service
sudo journalctl -u bluesky-push-notifier-dev.service -f
```

## What Was Fixed

### Issue 1: Challenge Mismatch (V1)
- **Problem**: Server used old stored challenge instead of current challenge
- **Fix**: Always use presented challenge for validation
- **File**: `src/app_attest.rs` line 407

### Issue 2: Assertion During Attestation (V1)
- **Problem**: Server tried to verify assertions during initial attestation
- **Fix**: Skip assertion verification when attestation is present
- **Files**: `src/api.rs` lines 1070-1079, 1262-1276

### Issue 3: Counter=0 Transition (V2)
- **Problem**: Device with counter=0 couldn't be used or unregistered
- **Fix**: Special handling for counter=0 (treat as needing attestation)
- **Files**: `src/api.rs` lines 1172-1265, 1638-1695

### Issue 4: Key Mismatch Detection (V3)
- **Problem**: Client and server have different keys, signature fails
- **Fix**: Detect "invalid signature", return HTTP 428 to request re-attestation
- **Files**: `src/api.rs` lines 1296-1316, 1702-1730

## Expected Behavior

### Successful Registration
```
✅ App Attest validation succeeded!
✅ Attestation verified for DID ... - skipping assertion check
Device registered successfully
```

### Successful Assertion (Normal Request)
```
🔍 ASSERTION: Parsed counter from assertion: 6 (previous was: 5)
✅ ASSERTION: Verification succeeded with counter 6
```

### Key Mismatch Recovery
```
🔍 ASSERTION: Parsed counter from assertion: 47 (previous was: 5)
❌ ASSERTION: Verification failed: invalid signature
Signature validation failed, requesting re-attestation (likely key mismatch)
HTTP 428 returned to client
Client receives 428 and re-attests
✅ Re-attestation verified for DID ... (counter was 0)
Success
```

### Unregister with Key Mismatch
```
❌ ASSERTION: Verification failed: invalid signature
Signature validation failed during unregister, allowing deletion anyway
Device unregistered successfully
```

## Testing Steps

### Test 1: Fresh Registration
1. Delete app from device
2. Reinstall and open
3. Enable notifications
4. **Expected**: ✅ Success, see "Attestation verified"

### Test 2: Normal Usage
1. With notifications enabled
2. Toggle a preference (e.g., likes on/off)
3. **Expected**: ✅ Success, see "ASSERTION: Verification succeeded"

### Test 3: Key Mismatch Recovery
1. Server has old key in database
2. Client tries to update preference
3. **Expected**: Client receives HTTP 428, automatically re-attests, succeeds

### Test 4: Disable/Re-enable Flow
1. Enable notifications → Success
2. Disable immediately → Success
3. Re-enable → Success
4. **Expected**: All steps succeed

## Troubleshooting

### Build Fails
```bash
# Check Rust version
rustc --version  # Should be 1.70+

# Clean and rebuild
cargo clean
cargo build --release
```

### Service Won't Start
```bash
# Check logs
sudo journalctl -u bluesky-push-notifier-dev.service -n 100

# Check if port is in use
sudo lsof -i :8081

# Verify binary exists
ls -lah /home/ubuntu/bluesky-push-notifier/target/release/bluesky-push-notifier
```

### Still Getting Errors

1. **Check logs for specific error**:
   ```bash
   sudo journalctl -u bluesky-push-notifier-dev.service -f | grep -E "ERROR|WARN|❌"
   ```

2. **Clear device from database** (forces fresh registration):
   ```bash
   # Connect to database
   doppler run --config dev -- psql $DATABASE_URL
   
   # Check current state
   SELECT did, app_attest_counter, app_attest_key_id 
   FROM user_devices 
   WHERE did = 'YOUR_DID_HERE';
   
   # Delete if needed
   DELETE FROM user_devices WHERE did = 'YOUR_DID_HERE';
   ```

3. **Client-side reset**:
   - Delete and reinstall app
   - This forces fresh key generation

## Verification Checklist

After deployment:

- [ ] Service is running: `sudo systemctl status bluesky-push-notifier-dev.service`
- [ ] No errors in logs: `sudo journalctl -u bluesky-push-notifier-dev.service -n 50`
- [ ] Can enable notifications on device
- [ ] Can update preferences
- [ ] Can disable notifications
- [ ] Push notifications are delivered

## Rollback (If Needed)

If something goes wrong:

```bash
cd /home/ubuntu/bluesky-push-notifier

# Checkout previous version
git log --oneline -10  # Find commit before changes
git checkout PREVIOUS_COMMIT_HASH

# Rebuild
cargo build --release

# Restart
sudo systemctl restart bluesky-push-notifier-dev.service
```

## Production Deployment

Once tested on dev:

```bash
# Build release
cargo build --release

# Deploy to staging
sudo systemctl restart bluesky-push-notifier-stg.service

# Test on staging

# Deploy to production (when ready)
# Update production binary and restart production service
```

## Support

If issues persist:

1. Check all documentation in this directory:
   - `APP_ATTEST_FIX_V3_FINAL.md` - Complete technical details
   - `APP_ATTEST_FIX_SUMMARY.md` - V1 summary
   - `APP_ATTEST_FIX_V2.md` - V2 summary

2. Capture logs:
   ```bash
   sudo journalctl -u bluesky-push-notifier-dev.service -n 500 > appattest-logs.txt
   ```

3. Check database state:
   ```sql
   SELECT did, device_token, app_attest_counter, app_attest_key_id, 
          app_attest_last_verified_at, created_at, updated_at
   FROM user_devices;
   ```

## Files Modified

- `src/app_attest.rs` - Core validation logic
- `src/api.rs` - API endpoints and error handling
- `deploy-dev.sh` - Deployment script
- `quick-deploy.sh` - Quick deployment script
- `clear_device.sql` - Database helper
- Multiple .md documentation files

## Success Criteria

✅ Users can enable notifications  
✅ Users can update preferences  
✅ Users can disable notifications  
✅ System handles key mismatches gracefully  
✅ No infinite retry loops  
✅ Push notifications are delivered  
✅ No "invalid signature" errors (or auto-recovers from them)  

---

**Status**: Ready for deployment  
**Last Updated**: January 2025  
**Version**: V3 (Final)
