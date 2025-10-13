# Client-Side Fix Required

## The Problem

The client has logic that prevents sending attestation when it has an existing key, even during new registration.

**Location**: `NotificationManager.swift` around line 1340-1370

**Current buggy code**:
```swift
var shouldIncludeAttestation = forceKeyRotation || info == nil || isNewRegistration || shouldForceRefresh

let keyIdentifier: String
if let existingKey = info?.keyIdentifier, !forceKeyRotation {
    // When using an existing key, we can't send attestation (already consumed)
    // Only send attestation for brand new keys or when explicitly forcing refresh
    if !shouldForceRefresh {
        shouldIncludeAttestation = false  // ← BUG: Disables attestation even for new registration
    }
    keyIdentifier = existingKey
}
```

## The Fix

Change the logic to preserve `shouldIncludeAttestation` for new registrations:

```swift
var shouldIncludeAttestation = forceKeyRotation || info == nil || isNewRegistration || shouldForceRefresh

let keyIdentifier: String
if let existingKey = info?.keyIdentifier, !forceKeyRotation {
    // Only disable attestation if this is NOT a new registration
    // During new registration, we must send attestation even with an existing key
    if !shouldForceRefresh && !isNewRegistration {
        shouldIncludeAttestation = false
    }
    keyIdentifier = existingKey
}
```

**The key change**: Add `&& !isNewRegistration` to the condition.

## Why This Happens

1. User tries to register → Registration fails (server issue, network, etc.)
2. Client caches the App Attest key in UserDefaults
3. User tries again → `info?.keyIdentifier` exists
4. Client thinks "I have a key, don't send attestation"
5. But this is still a NEW registration (device not in server database)
6. Server requires attestation for new devices
7. Registration fails: "attestation payload required"

## Alternative: Clear State on Failure

Another option is to clear the App Attest state when registration fails:

```swift
// In registerDeviceToken() after server rejects:
if statusCode == 400 && errorMessage.contains("attestation payload required") {
    notificationLogger.info("Server requires attestation - clearing cached App Attest state")
    await clearAppAttestState()
    // Retry will generate fresh attestation
}
```

## Recommended Approach

**Use BOTH fixes**:

1. **Fix the logic** to allow attestation during new registration (primary fix)
2. **Clear state on failure** as a fallback (defensive programming)

This ensures the client recovers from edge cases automatically.

## Testing

After applying the fix:

1. Delete and reinstall app
2. Enable notifications → Should succeed with attestation
3. If it fails, try again → Should now succeed (because state clears on failure)

## Code Locations

**File**: `Catbird/Core/Managers/NotificationManager.swift`

**Function**: `prepareAppAttestPayload()`

**Approximate line numbers**:
- Main logic: 1340-1370
- Alternative fix location: In `registerDeviceToken()` error handling around line 750-850
