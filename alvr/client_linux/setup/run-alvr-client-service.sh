#!/usr/bin/env bash
# Run by alvr-client.service (systemd user unit) at login. exec keeps the client as the main
# process so systemd's SIGTERM at shutdown reaches it and it closes the session cleanly.
CLIENT_DIR="$HOME/claude-remote/alvr-client"
mkdir -p "$CLIENT_DIR/logs"
export XR_RUNTIME_JSON=$HOME/.local/share/envision/prefixes/lighthouse_default/share/openxr/1/openxr_monado.json
export RUST_LOG=info
exec "$CLIENT_DIR/src/target/release/alvr_client_linux" >"$CLIENT_DIR/logs/client-$(date +%Y%m%d-%H%M%S).log" 2>&1
