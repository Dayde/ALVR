#!/usr/bin/env python3
"""Usage: watch-unit.py UNIT KNOWN_STATE

Blocks (no polling) until the user unit's ActiveState differs from KNOWN_STATE,
prints the new state and exits. Also exits after TIMEOUT seconds so an orphaned
watcher can't linger forever.
"""
import sys

from gi.repository import Gio, GLib

TIMEOUT = 1800
SD = "org.freedesktop.systemd1"
MANAGER = "/org/freedesktop/systemd1"

unit, known = sys.argv[1], sys.argv[2]
bus = Gio.bus_get_sync(Gio.BusType.SESSION)


def call(path, iface, method, args, ret):
    return bus.call_sync(SD, path, iface, method, args, GLib.VariantType(ret) if ret else None,
                         Gio.DBusCallFlags.NONE, -1, None)


path = call(MANAGER, SD + ".Manager", "LoadUnit", GLib.Variant("(s)", (unit,)), "(o)").unpack()[0]


def state():
    return call(path, "org.freedesktop.DBus.Properties", "Get",
                GLib.Variant("(ss)", (SD + ".Unit", "ActiveState")), "(v)").unpack()[0]


loop = GLib.MainLoop()


def finish(s):
    print(s, flush=True)
    loop.quit()


def on_changed(*_):
    s = state()
    if s != known:
        finish(s)


bus.signal_subscribe(SD, "org.freedesktop.DBus.Properties", "PropertiesChanged", path, None,
                     Gio.DBusSignalFlags.NONE, on_changed)
# systemd only emits unit signals while someone is subscribed
call(MANAGER, SD + ".Manager", "Subscribe", None, None)

current = state()
if current != known:
    print(current)
    sys.exit()

GLib.timeout_add_seconds(TIMEOUT, lambda: finish(state()))
loop.run()
