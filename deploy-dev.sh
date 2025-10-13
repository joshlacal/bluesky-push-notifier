#!/bin/bash
set -e

echo "🔨 Building release binary..."
cd /home/ubuntu/bluesky-push-notifier
cargo build --release

echo "✅ Build complete"
echo ""
echo "🔄 Restarting dev service..."
sudo systemctl restart bluesky-push-notifier-dev.service

echo "⏳ Waiting for service to start..."
sleep 3

echo ""
echo "📊 Service status:"
sudo systemctl status bluesky-push-notifier-dev.service --no-pager || true

echo ""
echo "📝 Recent logs:"
sudo journalctl -u bluesky-push-notifier-dev.service -n 30 --no-pager

echo ""
echo "✅ Deployment complete!"
echo ""
echo "📖 What was fixed:"
echo "  1. Challenge mismatch in assertion verification"
echo "  2. Invalid signature during initial attestation"
echo "  3. Counter=0 handling for disable/re-enable flow"
echo ""
echo "To follow logs in real-time, run:"
echo "  sudo journalctl -u bluesky-push-notifier-dev.service -f"
echo ""
echo "To filter for App Attest messages:"
echo "  sudo journalctl -u bluesky-push-notifier-dev.service -f | grep -E 'ASSERTION|ATTESTATION|counter=0|🔍|✅|❌'"
echo ""
echo "Test scenario:"
echo "  1. Enable notifications → Should see '✅ Attestation verified'"
echo "  2. Disable immediately → Should see 'counter=0, allowing unregister'"
echo "  3. Re-enable → Should see '✅ Re-attestation verified (counter was 0)'"
