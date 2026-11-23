# CarPiHAT Pro 5 for LIVI

This .NET 10 systemd service is for the Pi 5 and the PQ35 vehicle in
[Flutter-CarPi](https://github.com/ethanrushh/Flutter-CarPi/blob/2fb2b4eaafea99cb6e29bee880abe8ab36447ac6/lib/util/can/can_interpreter.dart).
It uses Microsoft’s `System.Device.Gpio` with libgpiod v2, and .NET's Linux SocketCAN
support. It reads CAN without transmitting or changing the link configuration.

| Signal | Behavior |
| --- | --- |
| BCM25 | The kernel holds the HAT’s power latch high and releases it at final poweroff. During testing without the overlay the service can hold the line high itself. |
| BCM22 | Drives the DSP enable high independently of ignition. |
| BCM12 | Switched 12 V sense. Stable low starts the shutdown countdown. High or a failed read cancels it. |
| CAN `0x5C1`, first byte | Previous `03`, next `02`, volume up `06`, volume down `07`, call `1A`, voice `2B`, release `00`. Repeated frames for a held button produce one action. |
| CAN `0x635`, second byte | Nonzero means switch illumination is on. Auto brightness uses the low level when lit. LIVI receives the corresponding `nightMode` value. |

Track changes use LIVI's media controls. Volume changes step from LIVI's current
master volume. Call and voice use LIVI's configured key bindings as taps. The
service sends relevant CAN changes to LIVI's telemetry API. It reconnects if
LIVI starts late or restarts, and discards missed wheel actions. CAN, display
and LIVI availability are independent of the GPIO shutdown countdown.

## Configuration

Copy `config.json` to `/etc/carpihat-pro5.json`. Restart the service to apply
changes.

- `ShutdownDelaySeconds` defaults to 40. `ShutdownEnabled` defaults to
  `false` for commissioning. The timer still runs and logs expiry in this
  mode. It logs at start and stop with the time remaining.
- `CanInterface` defaults to `can0`. The existing `can0.service` on CarPi
  configures the MCP2515 at 100 kbit/s. Configure the corresponding SocketCAN
  interface separately if you change the name.
- `Brightness` accepts `Auto`, `High`, `Low`, or `Disabled`. Auto leaves
  the panel alone until the first valid illumination frame. The high and low
  settings are percentages of the device’s `max_brightness` and default to
  100 and 12. CarPi's Waveshare panel uses `BacklightDevice: "10-0045"`,
  with a maximum of 255. Omit `BacklightDevice` to use the only sysfs
  backlight when exactly one exists.
- `WheelVolumeStepPercent` defaults to 4, as in Flutter-CarPi. Set it to 0 to
  disable steering-wheel volume changes.
- `LiviSocket` defaults to `$XDG_RUNTIME_DIR/livi/core.sock`. Run the service
  as the same Linux user as LIVI because the core checks the Unix socket’s
  peer UID. Vehicle telemetry uses `http://127.0.0.1:4000/telemetry`.

## Install on CarPi

CarPi already has .NET 10 under `/home/pi/.dotnet`, libgpiod v2, GPIO access
for `pi`, and write access to the Waveshare backlight via its `carpi` group.
The service requests poweroff with `sudo -n /usr/bin/systemctl --no-block poweroff`.
To check that permission without shutting down, run
`sudo -n -l /usr/bin/systemctl --no-block poweroff` as `pi`.

Publish with the .NET 10 SDK from this directory. Copy the published directory
to the Pi if you build on another machine.

```sh
dotnet publish -c Release -p:UseAppHost=false -o publish
```

With switched 12 V active, arrange the handover from the existing
`carpi.service`. Only one service can own BCM12/22/25. Disable the old
unit if it is installed. A previously removed unit can still be active
until the Pi reboots, as was observed on CarPi.

Add these lines under `[all]` in `/boot/firmware/config.txt` to assert the
outputs during boot and allow the kernel to release the latch after the OS
shuts down. Apply them on a controlled reboot with switched 12 V active.
The service checks for the running, bound, active-low overlay before it
will request a real poweroff. The firmware setting takes effect during boot.
The overlay can also affect reboot behavior.
[CarPiHAT Quick Start Guide](https://github.com/SamT-J/CarPiHat/wiki/Quick-Start-Guide#safe-shutdown-example)
· [Raspberry Pi overlay documentation](https://github.com/raspberrypi/firmware/blob/master/boot/overlays/README)

```ini
gpio=22,25=op,dh
dtoverlay=gpio-poweroff,gpiopin=25,active_low
```

On CarPi the published files, unit and test configuration are staged in
`~/CarPiHAT-Pro5`. Run these commands from that directory. Preserve your
configuration if updating an existing installation.

```sh
sudo install -d /opt/carpihat-pro5
sudo cp publish/* /opt/carpihat-pro5/
sudo install -m 0644 -C carpihat-pro5.service /etc/systemd/system/carpihat-pro5.service
sudo test -f /etc/carpihat-pro5.json ||
  sudo install -m 0644 config.json /etc/carpihat-pro5.json
sudo systemctl daemon-reload
sudo systemctl enable carpihat-pro5.service
```

After the controlled reboot, check both outputs with `pinctrl get 22,25` and
follow the journal with `journalctl -fu carpihat-pro5.service`. With
`ShutdownEnabled: false`, test ignition off and back on during the countdown,
then leave it off beyond the deadline and check for the single expiry log.
Set `ShutdownEnabled` to `true` and restart once ready to test a real shutdown.
