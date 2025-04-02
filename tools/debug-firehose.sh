#!/bin/bash
# Debug script for investigating Bluesky firehose issues

set -e

# Default values
SERVICE_URL="${BSKY_SERVICE_URL:-https://bsky.network}"
CURSOR="${CURSOR:-}"
TIMEOUT="${TIMEOUT:-30}"

# Help text
show_help() {
    echo "Usage: $0 [options]"
    echo ""
    echo "Debug the Bluesky firehose connection"
    echo ""
    echo "Options:"
    echo "  -s, --service URL    Service URL (default: $SERVICE_URL)"
    echo "  -c, --cursor CURSOR  Starting cursor (optional)"
    echo "  -t, --timeout SEC    Connection timeout in seconds (default: 30)"
    echo "  -h, --help           Show this help message"
}

# Parse command line arguments
while [[ $# -gt 0 ]]; do
    key="$1"
    case $key in
        -s|--service)
            SERVICE_URL="$2"
            shift 2
            ;;
        -c|--cursor)
            CURSOR="$2"
            shift 2
            ;;
        -t|--timeout)
            TIMEOUT="$2"
            shift 2
            ;;
        -h|--help)
            show_help
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            show_help
            exit 1
            ;;
    esac
done

echo "Debugging firehose connection to $SERVICE_URL"
echo "Timeout: $TIMEOUT seconds"

# Create WebSocket URL
WS_URL="wss://${SERVICE_URL}/xrpc/com.atproto.sync.subscribeRepos"
if [[ -n "$CURSOR" ]]; then
    echo "Using cursor: $CURSOR"
    WS_URL="${WS_URL}?cursor=${CURSOR}"
fi

echo "Connecting to: $WS_URL"
echo "Press Ctrl+C to exit"

# Use websocat if available, otherwise fallback to wscat
if command -v websocat &> /dev/null; then
    websocat -v --ping-interval 10 "$WS_URL" 
elif command -v wscat &> /dev/null; then
    wscat -n --connect "$WS_URL" --max-redirects 5 --ping-interval 10 --timeout $TIMEOUT
else
    echo "Error: Neither websocat nor wscat found. Please install one of them:"
    echo "  npm install -g wscat"
    echo "  or"
    echo "  cargo install websocat"
    exit 1
fi
