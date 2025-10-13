# App Attest Fix V2 - Counter=0 Issue

## The Problem Discovered

After fixing the initial challenge mismatch and assertion-during-attestation issues, we discovered a new problem:

**When users disable then re-enable notifications**, the flow breaks:

1. User enables notifications → Device registered with counter=0 (after attestation)
2. User disables notifications → Unregister tries to verify assertion with counter=0 → FAILS
3. User enables notifications again → Re-register tries to verify assertion with counter=0 → FAILS

### Why It Fails

App Attest assertions require the counter to **advance** (be greater than the previous counter):
- Stored counter: 0
- Assertion counter: 0  
- Check: `0 > 0` → FALSE → "invalid signature"

### Root Cause

When we save a device after attestation, we set `counter = 0`. This is correct because:
- Attestation doesn't use a counter
- Counter only applies to assertions
- First assertion should have counter = 1

BUT: Our code was trying to verify assertions even when counter=0, which means the device was attested but never actually used yet.

## The Fix

### Fix #1: Registration with Counter=0

When a device exists with `counter=0`, it means:
- Device was attested
- No assertions have been verified yet
- Device needs re-attestation (not assertion verification)

**Solution**: Check if counter=0 during registration, and if so, treat it as a fresh attestation case.

**Location**: `src/api.rs` lines 1160-1265

```rust
if previous_counter == 0 {
    // Device was attested but never used
    // Accept new attestation instead of requiring assertion
    let attestation = verify_attestation_async(...).await?;
    // Update with counter=0 again
}
```

### Fix #2: Unregister with Counter=0

When unregistering a device with `counter=0`, it means:
- Device was registered but never actually used
- No assertions were ever verified
- Safe to delete without assertion verification

**Solution**: Skip assertion verification if counter=0 during unregister.

**Location**: `src/api.rs` lines 1594-1659

```rust
if previous_counter == 0 {
    // Device was attested but never used
    // Allow deletion without assertion verification
    DELETE device;
    return success;
}
```

## Complete Flow Now

### Fresh Registration
```
1. Client sends attestation
2. Server verifies attestation ✓
3. Server stores: key, public_key, counter=0
4. Success
```

### Disable Notifications (Unregister) - Immediately After Registration
```
1. Client sends unregister with assertion
2. Server checks: counter=0
3. Server skips assertion verification (device never used)
4. Server deletes device
5. Success
```

### Re-Enable Notifications (Re-Register) - After Immediate Unregister
```
1. Client sends attestation (because device doesn't exist anymore)
2. Server creates new device
3. Server verifies attestation ✓
4. Server stores: key, public_key, counter=0
5. Success
```

OR if device still exists (edge case):
```
1. Client sends attestation
2. Server finds existing device with counter=0
3. Server treats as fresh attestation case
4. Server verifies attestation ✓
5. Server updates: key, public_key, counter=0
6. Success
```

### Normal Update Request (Preferences, Relationships) - After Device is Used
```
1. Client sends assertion
2. Server checks: counter > 0
3. Server verifies assertion ✓
4. Server updates: counter = counter + 1
5. Success
```

## Why This Works

The key insight is that **counter=0 is a special state** that means:
- "Device was attested but never used"
- Assertion verification doesn't make sense yet
- Device needs attestation, not assertion

Once a device successfully completes ANY operation (preferences update, relationship sync, etc.), the counter advances to 1+, and then normal assertion flow works.

## Testing Scenario

To test this fix:

1. **Enable notifications** (fresh)
   - Should succeed with attestation
   - Device stored with counter=0
   
2. **Disable notifications** (immediately)
   - Should succeed without assertion verification
   - Device deleted
   
3. **Enable notifications** (again)
   - Should succeed with new attestation
   - Device stored with counter=0
   
4. **Update a preference**
   - Should succeed with assertion
   - Device counter advances to 1
   
5. **Disable notifications** (after use)
   - Should succeed with assertion verification
   - Device deleted
   
6. **Enable notifications** (final time)
   - Should succeed with attestation
   - Device stored with counter=0

All steps should now work!

## Files Changed

1. **src/app_attest.rs** - Fixed challenge validation (from Fix V1)
2. **src/api.rs** - Multiple changes:
   - Removed assertion verification during initial attestation (Fix V1)
   - Added counter=0 handling in registration path (Fix V2)
   - Added counter=0 handling in unregister path (Fix V2)

## Deployment

```bash
cd /home/ubuntu/bluesky-push-notifier
cargo build --release
sudo systemctl restart bluesky-push-notifier-dev.service
sudo journalctl -u bluesky-push-notifier-dev.service -f
```

## Expected Log Messages

### Registration with counter=0
```
Device has counter=0 for DID ..., treating as fresh attestation case
✅ Re-attestation verified for DID ... (counter was 0)
```

### Unregister with counter=0
```
Device has counter=0 for DID ..., allowing unregister without assertion verification
Device unregistered successfully (counter was 0)
```

### Normal assertion verification (counter > 0)
```
🔍 ASSERTION: Using challenge for validation: ...
✅ ASSERTION: Verification succeeded with counter X
```

## Root Cause Summary

The fundamental issue was a mismatch between:
- **What attestation provides**: Proof that a key is valid (no counter involved)
- **What assertions provide**: Ongoing proof of key possession (counter-based)

Our initial fix correctly separated these, but didn't account for the **transitional state** where a device is attested (counter=0) but hasn't been used yet (no assertions verified).

The V2 fix recognizes counter=0 as this special transitional state and handles it appropriately.
