#!/usr/bin/env bash
# Run by alvr-client.service (systemd user unit) at login. exec keeps the client as the main
# process so systemd's SIGTERM at shutdown reaches it and it closes the session cleanly.
CLIENT_DIR="$HOME/claude-remote/alvr-client"
CLIENT_BIN="$HOME/b3w1/current/alvr_client_linux"
[ -x "$CLIENT_BIN" ] || CLIENT_BIN="$CLIENT_DIR/src/target/release/alvr_client_linux"
mkdir -p "$CLIENT_DIR/logs"
export XR_RUNTIME_JSON=$HOME/.local/share/envision/prefixes/lighthouse_b3w1/share/openxr/1/openxr_monado.json
export RUST_LOG=info
exec "$CLIENT_BIN" >"$CLIENT_DIR/logs/client-$(date +%Y%m%d-%H%M%S).log" 2>&1
