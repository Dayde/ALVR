#!/usr/bin/env bash
# Run by alvr-monado.service. Starts monado-service and watches its log: if the GPU reports a lost
# device (VK_ERROR_DEVICE_LOST, happens when Monado and KWin set up the displays at the same time),
# Monado shows no video but keeps running, so stop it and exit non-zero to let systemd restart it.
fifo="$XDG_RUNTIME_DIR/alvr-monado-log.fifo"
rm -f "$fifo" && mkfifo "$fifo"
# alvr-client.service waits for this file: Monado has found the devices and set up the display
ready="$XDG_RUNTIME_DIR/alvr-monado-ready"
rm -f "$ready"
# monado-service quits when stdin closes, so give it a pipe that stays open
"${MONADO_BIN:-/usr/bin/monado-service}" < <(exec sleep infinity) >"$fifo" 2>&1 &
pid=$!
trap 'kill -TERM $pid 2>/dev/null; wait $pid; exit 0' TERM INT
while IFS= read -r line; do
    printf '%s\n' "$line"
    case "$line" in
        *"Started vblank event thread!"*)
            touch "$ready" ;;
        *VK_ERROR_DEVICE_LOST*|*"ERROR [vblank_event_func]"*)
            echo "alvr-monado: display lost, restarting Monado"
            kill -TERM $pid
            for _ in $(seq 1 50); do kill -0 $pid 2>/dev/null || break; sleep 0.1; done
            kill -KILL $pid 2>/dev/null
            exit 1 ;;
        *"Cannot add device after setup"*)
            # A tracker (or a third controller) turned on after setup. Hand controllers connect to
            # their stand-in instead (patch 0003). Monado can't add other devices later, and since
            # the 2026-11-25 system upgrade a restarted Monado loses the display, so keep running.
            echo "alvr-monado: a device turned on after setup and is ignored until the next reboot" ;;
    esac
done < "$fifo"
wait $pid
