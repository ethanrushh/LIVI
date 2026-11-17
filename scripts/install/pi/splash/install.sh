#!/usr/bin/env bash
# Install the LIVI Plymouth boot splash on Raspberry Pi OS.
# Run as root (or via sudo) on the Pi
set -euo pipefail

THEME_NAME="livi"
THEME_DIR="/usr/share/plymouth/themes/${THEME_NAME}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOGO_SRC="${SCRIPT_DIR}/livi-splash.png"

CONFIG_TXT=""
CMDLINE_TXT=""

if [[ $EUID -ne 0 ]]; then
  echo "Run with sudo: sudo $0" >&2
  exit 1
fi

if [[ ! -f "${LOGO_SRC}" || ! -f "${SCRIPT_DIR}/livi.script" || ! -f "${SCRIPT_DIR}/livi.plymouth" ]]; then
  echo "Missing LIVI splash resources (PNG, script or theme descriptor)." >&2
  exit 1
fi

CONFIG_TXT="/boot/firmware/config.txt"
CMDLINE_TXT="/boot/firmware/cmdline.txt"
if [[ ! -f "${CONFIG_TXT}" ]] || [[ ! -f "${CMDLINE_TXT}" ]]; then
  echo "Expected ${CONFIG_TXT} and ${CMDLINE_TXT} (Pi OS Trixie)" >&2
  exit 1
fi

echo "[1/5] Installing Plymouth"
apt-get update -qq
apt-get install -y plymouth plymouth-themes

echo "[2/5] Writing theme to ${THEME_DIR}"
install -d -m 0755 "${THEME_DIR}"
# Keep a chosen UI icon when the installer is rerun. LIVI synchronizes it again
# when the app starts, including restoring the original icon after Reset.
if [[ ! -f "${THEME_DIR}/logo.png" ]]; then
  install -m 0644 "${LOGO_SRC}" "${THEME_DIR}/logo.png"
fi

install -m 0644 "${SCRIPT_DIR}/livi.plymouth" "${THEME_DIR}/${THEME_NAME}.plymouth"
install -m 0644 "${SCRIPT_DIR}/livi.script" "${THEME_DIR}/${THEME_NAME}.script"

echo "[3/5] Activating theme + rebuilding initramfs"
plymouth-set-default-theme "${THEME_NAME}" -R

# disable_fw_kms_setup=1 means KMS comes up late; let plymouth wait for it
install -d -m 0755 /etc/plymouth
cat > /etc/plymouth/plymouthd.conf <<EOF
[Daemon]
Theme=${THEME_NAME}
ShowDelay=0
DeviceTimeout=30
EOF

echo "[4/5] Patching ${CONFIG_TXT}"
# Pi rainbow off (we run plymouth instead)
if ! grep -qE '^\s*disable_splash=1' "${CONFIG_TXT}"; then
  echo "disable_splash=1" >> "${CONFIG_TXT}"
fi
# Firmware mode-set must run early; otherwise plymouth renders into offline HDMI
sed -i 's/^disable_fw_kms_setup=1$/# disable_fw_kms_setup=1     # disabled by the LIVI splash for early HDMI/' "${CONFIG_TXT}"

echo "[5/5] Patching ${CMDLINE_TXT}"
cp -a "${CMDLINE_TXT}" "${CMDLINE_TXT}.bak.$(date +%s)"
LINE=$(tr -d '\n' < "${CMDLINE_TXT}")

add_flag() {
  local flag="$1"
  case " ${LINE} " in
    *" ${flag} "*) ;;
    *) LINE="${LINE} ${flag}" ;;
  esac
}

add_flag "quiet"
add_flag "splash"
add_flag "plymouth.ignore-serial-consoles"
add_flag "loglevel=0"

# Keeps the mode LIVI pinned (video=) so plymouth does not render at EDID
# preferred (often 4K). LIVI_SPLASH_VIDEO overrides it.
detect_video_mode() {
  local current
  current=$(grep -o "video=[^[:space:]]*" <<< "${LINE}" | head -n 1)
  [[ -n "${current}" ]] || return 1
  printf "%s" "${current#video=}"
}

VIDEO_MODE="${LIVI_SPLASH_VIDEO:-$(detect_video_mode || true)}"
if [[ -n "${VIDEO_MODE}" ]]; then
  echo "      video mode = ${VIDEO_MODE}"
  LINE=$(echo "${LINE}" | sed -E 's/[[:space:]]*video=[^[:space:]]+//g')
  LINE="${LINE} video=${VIDEO_MODE}"
else
  echo "      no video mode pinned (plymouth will use EDID preferred)"
fi
add_flag "logo.nologo"
add_flag "vt.global_cursor_default=0"

echo "${LINE}" > "${CMDLINE_TXT}"

echo
echo "Done. Reboot to see the new splash."
