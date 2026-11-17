#!/usr/bin/env bash
set -euo pipefail

# No downloads, sudo, or host configuration changes: exercise only installer state.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_DIR="$(mktemp -d)"
trap 'rm -rf "$TEST_DIR"' EXIT

(
  unset LIVI_REPO LIVI_INSTALLER_BRANCH LIVI_CHANNEL LIVI_APP_CONFIG
  export LIVI_USER_DATA="$TEST_DIR/default"
  # shellcheck source=common.sh
  . "$ROOT/common.sh"
  [ "$LIVI_REPO" = ethanrushh/LIVI ]
  [ "$LIVI_BRANCH" = startup-volume ]
  [ "$LIVI_RAW" = https://raw.githubusercontent.com/ethanrushh/LIVI/startup-volume ]
  LIVI_CHANNEL=nightly
  livi_pick_channel
  [ "$LIVI_RELEASE_API" = https://api.github.com/repos/ethanrushh/LIVI/releases/tags/nightly ]
  livi_seed_update_config
  jq -e '.updateRepo == "ethanrushh/LIVI" and .updateNightly == true' "$LIVI_APP_CONFIG" >/dev/null

  # Reinstalling must preserve the user's safety limit and unrelated preferences.
  printf '%s\n' '{"startupVolumeLimit":0.07,"huVolume":0,"carName":"Pi","updateNightly":true}' > "$LIVI_APP_CONFIG"
  LIVI_CHANNEL=release
  livi_pick_channel
  [ "$LIVI_RELEASE_API" = https://api.github.com/repos/ethanrushh/LIVI/releases/latest ]
  livi_seed_update_config
  jq -e '.updateRepo == "ethanrushh/LIVI" and .updateNightly == false and
    .startupVolumeLimit == 0.07 and .huVolume == 0 and .carName == "Pi"' "$LIVI_APP_CONFIG" >/dev/null
)

(
  export LIVI_USER_DATA="$TEST_DIR/override"
  export LIVI_REPO=example/fork LIVI_INSTALLER_BRANCH=testing LIVI_CHANNEL=nightly
  unset LIVI_APP_CONFIG
  # shellcheck source=common.sh
  . "$ROOT/common.sh"
  [ "$LIVI_RAW" = https://raw.githubusercontent.com/example/fork/testing ]
  livi_pick_channel
  [ "$LIVI_RELEASE_API" = https://api.github.com/repos/example/fork/releases/tags/nightly ]
  livi_seed_update_config
  jq -e '.updateRepo == "example/fork" and .updateNightly == true' "$LIVI_APP_CONFIG" >/dev/null
  LIVI_CHANNEL=unknown
  if livi_pick_channel 2>/dev/null; then
    echo 'An invalid channel was accepted' >&2
    exit 1
  fi
)

echo 'Installer update config tests passed'
