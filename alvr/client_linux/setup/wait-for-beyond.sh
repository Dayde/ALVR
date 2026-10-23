#!/usr/bin/env bash
# Wait (up to 60 s) for the Bigscreen Beyond (USB 35bd:0101) to enumerate before Monado starts
for _ in $(seq 1 60); do
    grep -qs '^35bd$' /sys/bus/usb/devices/*/idVendor && grep -qs '^0101$' /sys/bus/usb/devices/*/idProduct && exit 0
    sleep 1
done
echo "Bigscreen Beyond not found on USB, starting Monado anyway"
