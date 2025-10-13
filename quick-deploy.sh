#!/bin/bash
set -e

echo "🔨 Building..."
cd /home/ubuntu/bluesky-push-notifier
cargo build --release 2>&1 | tail -20

if [ $? -eq 0 ]; then
    echo ""
    echo "✅ Build successful!"
    echo ""
    echo "🔄 Restarting service..."
    sudo systemctl restart bluesky-push-notifier-dev.service
    
    echo "⏳ Waiting 3 seconds..."
    sleep 3
    
    echo ""
    echo "📊 Service status:"
    sudo systemctl status bluesky-push-notifier-dev.service --no-pager -l | head -20
    
    echo ""
    echo "✅ Deployment complete!"
    echo ""
    echo "📝 To follow logs:"
    echo "  sudo journalctl -u bluesky-push-notifier-dev.service -f"
else
    echo ""
    echo "❌ Build failed!"
    exit 1
fi
