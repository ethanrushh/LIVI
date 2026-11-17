# Startup volume and fork updates

## Existing Linux volume handling

LIVI sends Linux playback through GStreamer's `pulsesink`, using PulseAudio or PipeWire's PulseAudio
server. The head unit volume is linked to the system by default. `pactl get-sink-volume` reads the
configured output, or the default output if that device is absent. Head unit changes write back with
`pactl set-sink-volume`. External volume changes are followed through `pactl subscribe`.

Before this change, the follower adopted the existing system volume at startup. There was no startup
ceiling, no pass over other outputs, and the follower ran concurrently with startup services. A device
restored to 100% could therefore remain at 100%. The saved head unit default was also 95%; it was not a
safety limit. Music, navigation, calls and other stream sliders control separate stream gains, not the
system output ceiling.

## Startup Volume Limit

Settings → Audio → **Startup Volume Limit** defaults to **15%** on Linux. In `config.json` this is
`"startupVolumeLimit": 0.15`, with values from `0.0` to `1.0`. Changes take effect on the next LIVI start.
Missing, malformed or out-of-range saved values fall back to 15%. Invalid live settings are rejected.

Before starting playback services, phone connections or system-volume writers, LIVI:

1. Finishes its host audio setup, including any WirePlumber restart, then lists **all registered output
   sinks**, including non-default and suspended outputs.
2. Reads each channel's raw volume and applies `min(current_volume, startup_volume_limit)`. Zero and
   quieter channels stay at their existing levels. The raw ceiling is rounded down.
3. Reads every output again to verify the limit, including channels whose first value was already low.
4. Retries failures or changing device lists up to ten times, waiting 500 ms between attempts. Each
   `pactl` invocation has a two-second timeout. If any output remains unverified, or no outputs exist,
   LIVI refuses to start playback and records the reason in its log.
5. Clamps the saved head unit level in memory and on disk before the volume link or vehicle bridge can
   write it back.

The startup check runs even when **Link Head Unit to System Volume** is off. It only issues output-sink
volume commands: input devices, microphones, source volumes and mute states are untouched. It does not
unmute an output.

A separate watcher also clamps newly discovered outputs and rechecks all outputs when its audio-server
subscription reconnects. A one-second poll retries failures and discovers late devices. Established
outputs can still be raised deliberately during the session; this is a startup limit, not a permanent
maximum-volume setting.

### Scope of the protection

This is a check before **LIVI playback**, not a guarantee that Linux can never produce loud sound.
Desktop login sounds or other applications can play before LIVI starts. Other clients, restored mixer
state, PulseAudio flat-volume behavior, driver changes or audio-server restarts can raise volume after
the check. Newly connected outputs can play before the watcher processes their arrival. Devices not
exposed as PulseAudio/PipeWire sinks, including direct ALSA playback, are outside this check.

An app-level startup clamp cannot meet an absolute requirement that unsafe output is impossible. That
requires a ceiling enforced before audio reaches the DAC, including while LIVI is stopped. Do not treat
these changes or their automated tests as proof of end-to-end hearing protection on a particular
DAC/amplifier chain.

## Build the branch for a Raspberry Pi

`nightly` is a rolling GitHub **release tag**, not a source branch. `startup-volume` starts at the
commit that was tagged nightly when this work began (`eaa9447c`). Updates download release assets;
they do not compile or pull a Git branch on the Pi.

After pushing `startup-volume` to the fork:

1. Enable Actions in `ethanrushh/LIVI` if the fork has not enabled them.
2. Open **Actions → build → Run workflow** and select **startup-volume** as the branch.
3. Enable **Build Linux arm64** and **Publish nightly release**. Disable the other platform builds if
   they are not needed. Run the workflow and wait for it to succeed.
4. Confirm the fork's `nightly` release title names `startup-volume` and the intended commit, and that
   it contains `LIVI-<version>-linux-arm64.AppImage`.

The build workflow publishes to `github.repository`, so this creates the fork's nightly release.
Firmware Pages updates are skipped in forks. Shared, checksum-pinned GStreamer and firmware build
dependencies still come from upstream's `assets` release; they are not the installed app or update feed.

Only one release can own the fork's `nightly` tag. Publishing nightly from another source branch replaces
that feed. Keep publishing from `startup-volume` while testing this feature. To update the Pi after
further commits, publish another nightly from this branch, then use LIVI's updater.

## Reinstall and keep updates on the fork

Stop the running LIVI instance before reinstalling so that it cannot overwrite the installer's config
changes. Then, as the regular Pi user:

```bash
curl -fL -o install.sh https://raw.githubusercontent.com/ethanrushh/LIVI/startup-volume/scripts/install/install.sh
LIVI_CHANNEL=nightly bash install.sh
```

The installer defaults to `ethanrushh/LIVI` for **both release and nightly** downloads and to
`startup-volume` for its scripts, package list and fallback templates. It saves:

```json
{
  "updateRepo": "ethanrushh/LIVI",
  "updateNightly": true
}
```

These settings survive a reboot and the updater replacing the AppImage. The updater also defaults to
this fork when loading an older config. Selecting release requires a published non-prerelease in the
fork; a missing fork release fails rather than falling back to upstream.

`LIVI_REPO`, `LIVI_INSTALLER_BRANCH` and `LIVI_CHANNEL` still override installer defaults. The chosen
repository/channel are saved for subsequent updates. Existing `UPDATE_REPO` and `UPDATE_FEED` runtime
environment overrides take precedence over `updateRepo`; remove old overrides if they point upstream.

## Hardware verification

With the amplifier disconnected or otherwise physically silenced, check startup with the DAC and an
additional non-default output initially above the ceiling, then below it, then at zero. Check unequal
channel levels, link disabled, delayed audio-server startup, a disconnected DAC and reconnection, and
an unavailable `pactl`. Inspect `pactl --format=json list sinks` and compare `pactl --format=json list
sources` before and after. Confirm all output channels are at or below the ceiling before enabling
playback and that source levels are unchanged. Repeat after a cold boot and after an updater restart.

Automated tests use simulated mixer state; they do not validate your Pi's boot ordering, desktop sounds,
DAC behavior or amplifier gain.
