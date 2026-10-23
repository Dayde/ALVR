#!/usr/bin/env bash
# One-click launcher for the Linux ALVR client on B3-W1.
# Starts Monado through Envision if it isn't running, then runs the client and logs its output.

CLIENT_DIR="$HOME/claude-remote/alvr-client"
CLIENT_BIN="$CLIENT_DIR/src/target/release/alvr_client_linux"
LOG_DIR="$CLIENT_DIR/logs"
LOG_FILE="$LOG_DIR/client-$(date +%Y%m%d-%H%M%S).log"
MONADO_SOCKET="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/monado_comp_ipc"

fail() {
    echo
    echo "ERROR: $1"
    read -rp "Press Enter to close..."
    exit 1
}

[ -x "$CLIENT_BIN" ] || fail "Client not built: $CLIENT_BIN"

if pgrep -x alvr_client_lin >/dev/null; then
    fail "The ALVR client is already running."
fi

if ! pgrep -x monado-service >/dev/null; then
    echo "Starting Monado through Envision..."
    # Envision passes U_PACING_COMP_MIN_TIME_MS=8 to Monado (set in its global preferences)
    setsid envision -S >/dev/null 2>&1 &
    for _ in $(seq 1 60); do
        pgrep -x monado-service >/dev/null && [ -S "$MONADO_SOCKET" ] && break
        sleep 1
    done
    pgrep -x monado-service >/dev/null || fail "Monado didn't start. Start it in Envision, then run this again."
    sleep 3
fi

mkdir -p "$LOG_DIR"
echo "Monado is running. Starting the ALVR client (log: $LOG_FILE)"
echo "Close this window or press Ctrl+C to stop the client."
echo

export XR_RUNTIME_JSON=$HOME/.local/share/envision/prefixes/lighthouse_default/share/openxr/1/openxr_monado.json
export RUST_LOG=info
"$CLIENT_BIN" 2>&1 | tee "$LOG_FILE"

echo
echo "The client exited."
read -rp "Press Enter to close..."
