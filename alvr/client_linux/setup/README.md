# Standalone Linux client setup

Files used to run `alvr_client_linux` unattended on a small Linux PC that drives a PC VR headset
(tested with a Bigscreen Beyond, two Index controllers and three base stations, on an AMD Radeon
780M). The headset is plugged into this machine; the game runs on another PC with the ALVR streamer.

```
Bigscreen Beyond <- DisplayPort/USB -- Monado (monado-service) <- OpenXR -- alvr_client_linux <- network -- ALVR streamer (PC)
```

At login, systemd starts Monado once the headset is on USB, then starts the client once Monado has
set up the headset display. Both restart on their own when something goes wrong.

## Files

| File | What it does |
|---|---|
| `systemd/alvr-monado.service` | Runs Monado with the settings below. Wants `alvr-client.service`, so the client (re)starts with it |
| `systemd/alvr-client.service` | Runs the client once Monado is ready (waits for `$XDG_RUNTIME_DIR/alvr-monado-ready`, up to 60 s). Restarts it 1 s after it exits |
| `wait-for-beyond.sh` | Waits up to 60 s for the Beyond (USB `35bd:0101`) to enumerate before Monado starts |
| `run-monado-service.sh` | Starts `monado-service` and reads its log: writes the ready flag on "Started vblank event thread!", and exits (so systemd restarts Monado) on `VK_ERROR_DEVICE_LOST`, vblank errors, or a controller/tracker that turned on after discovery |
| `run-alvr-client-service.sh` | Runs the client with the Monado runtime and logs to `logs/client-<time>.log` |
| `launch-alvr-client.sh` | Manual one-click launcher: starts Monado through Envision if it isn't running, then the client in a terminal |
| `alvr-client.desktop` | Menu entry for `launch-alvr-client.sh` |
| `plasmoid/org.dayde.alvrtoggle` | KDE Plasma panel widget that starts and stops `alvr-client.service` and shows its state. It waits for systemd's change signals instead of polling |
| `system/70-bigscreen-beyond.rules` | udev rule giving the logged-in user access to the Beyond's USB devices (vendor `35bd`) |
| `system/10-power-key.conf` | logind drop-in: the power button powers off even when KDE holds an inhibitor. The services then stop in order: client, then Monado |
| `monado/*.patch` | Changes to Monado this setup relies on, see below |

The scripts expect this layout: the client checkout in `~/claude-remote/alvr-client/src`, these
scripts in `~/claude-remote/alvr-client`, and a Monado build installed by
[Envision](https://gitlab.com/gabmus/envision) in the `lighthouse_default` prefix
(`~/.local/share/envision/prefixes/lighthouse_default`). Adjust the paths if yours differ.

## Monado settings

Set in `alvr-monado.service` (the same ones Envision passes for its "Lighthouse Driver (default)"
profile, plus a few for this setup):

| Variable | Why |
|---|---|
| `STEAMVR_LH_ENABLE=true` | Use SteamVR's Lighthouse driver (needs SteamVR installed) |
| `XRT_COMPOSITOR_COMPUTE=1` | Compute shader compositor |
| `U_PACING_COMP_MIN_TIME_MS=8` | The default compositor time budget is too short for the 780M: Monado presents every other vblank (45 fps) |
| `XRT_COMPOSITOR_SCALE_PERCENTAGE=100` | Above 100 the 780M can't keep 90 fps |
| `IPC_EXIT_ON_DISCONNECT=off` | Keep Monado running when the client restarts |
| `LH_EXPECTED_CONTROLLERS=2`, `LH_DISCOVER_WAIT_MS=20000` | End device discovery once the headset and 2 controllers are set up, 20 s at most (needs patch 0002) |

`monado-service` exits when its stdin closes, so the wrapper gives it a pipe that stays open.

## Monado patches

Apply to Envision's Monado checkout (`~/.local/share/envision/monado`) with `git apply`, then
rebuild from Envision.

- `0001`: skips the first 90 presents before registering for vblank events. Registering before the
  first modeset on the leased connector has landed fails in amdgpu ("dc_stream_state is NULL") and
  ends in `VK_ERROR_DEVICE_LOST`, which only a reboot recovers from. Also logs the first vblank.
- `0002`: `LH_EXPECTED_CONTROLLERS` ends Lighthouse discovery as soon as the headset and that many
  controllers or trackers are set up, instead of always waiting `LH_DISCOVER_WAIT_MS`. Base
  stations found after discovery are logged as info instead of the "Cannot add device" warning
  (they're never added as devices anyway).

## Install

```
cp systemd/*.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable alvr-monado.service alvr-client.service
```

The system files need root:

```
sudo cp system/70-bigscreen-beyond.rules /etc/udev/rules.d/
sudo mkdir -p /etc/systemd/logind.conf.d && sudo cp system/10-power-key.conf /etc/systemd/logind.conf.d/
```

On KDE, PowerDevil still holds the power key while no regular monitor is connected, so the button
does nothing on a headless box. Mask it for your user (you lose KDE's power settings applet):
`systemctl --user mask --now plasma-powerdevil.service`.

Panel widget: `cp -r plasmoid/org.dayde.alvrtoggle ~/.local/share/plasma/plasmoids/`, then add
"ALVR Client Toggle" to a panel.

Don't start Monado from Envision while `alvr-monado.service` runs: only one process can drive the
headset. Logs: `journalctl --user -u alvr-monado -u alvr-client` and the client logs in
`~/claude-remote/alvr-client/logs`.
