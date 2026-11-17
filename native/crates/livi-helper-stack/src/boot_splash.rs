// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::Path;
use std::process::Command;

pub const THEME: &str = "/usr/share/plymouth/themes/livi";
const SCRIPT: &str = include_str!("../../../../scripts/install/pi/splash/livi.script");
const DESCRIPTOR: &str = include_str!("../../../../scripts/install/pi/splash/livi.plymouth");
const MARKER: &str = ".livi-splash.sha256";
const MAX_LOGO_BYTES: u64 = 2 * 1024 * 1024;

fn validate_logo(png: &[u8]) -> Result<(), String> {
    if png.len() < 45
        || png.len() as u64 > MAX_LOGO_BYTES
        || &png[..8] != b"\x89PNG\r\n\x1a\n"
        || &png[12..16] != b"IHDR"
        || u32::from_be_bytes(png[16..20].try_into().unwrap()) != 512
        || u32::from_be_bytes(png[20..24].try_into().unwrap()) != 512
    {
        return Err("the boot splash must be a 512 x 512 PNG".into());
    }
    Ok(())
}

fn sync_theme(
    dir: &Path,
    png: &[u8],
    rebuild: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    validate_logo(png)?;
    let digest = livi_base::hex::encode(&livi_crypto::sha256(&[
        png,
        SCRIPT.as_bytes(),
        DESCRIPTOR.as_bytes(),
    ]));
    let files = [
        ("logo.png", png),
        ("livi.script", SCRIPT.as_bytes()),
        ("livi.plymouth", DESCRIPTOR.as_bytes()),
    ];
    let current = files
        .iter()
        .all(|(name, content)| fs::read(dir.join(name)).is_ok_and(|v| v == *content));
    if current && fs::read_to_string(dir.join(MARKER)).is_ok_and(|v| v == digest) {
        return Ok(());
    }
    for (name, content) in files {
        livi_base::fs::write_atomic(&dir.join(name), content, 0o644).map_err(|e| e.to_string())?;
    }
    // Mark success only after the boot image has been rebuilt. An interrupted or
    // failed rebuild remains pending and is retried on the next request.
    rebuild()?;
    livi_base::fs::write_atomic(&dir.join(MARKER), digest, 0o644).map_err(|e| e.to_string())
}

pub fn update(src: &str) -> Result<bool, String> {
    let dir = Path::new(THEME);
    if !dir.join("livi.plymouth").is_file() {
        return Ok(false);
    }
    let program = livi_base::sys::tool("plymouth-set-default-theme");
    let active = Command::new(&program).output().map_err(|e| e.to_string())?;
    if !active.status.success() || String::from_utf8_lossy(&active.stdout).trim() != "livi" {
        return Ok(false);
    }
    // Serialize this helper across app instances and installer-triggered retries.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".livi-splash.lock"))
        .map_err(|e| e.to_string())?;
    lock.try_lock()
        .map_err(|e| format!("another boot splash update is running: {e}"))?;
    let mut png = Vec::new();
    File::open(src)
        .map_err(|e| e.to_string())?
        .take(MAX_LOGO_BYTES + 1)
        .read_to_end(&mut png)
        .map_err(|e| e.to_string())?;
    sync_theme(dir, &png, || {
        let output = Command::new(&program)
            .arg("--rebuild-initrd")
            .output()
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "Plymouth could not rebuild the initramfs: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    const DEFAULT: &[u8] = include_bytes!("../../../../scripts/install/pi/splash/livi-splash.png");

    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "livi-splash-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn updates_are_atomic_idempotent_and_restore_the_shared_style() {
        let dir = Temp::new();
        let calls = std::cell::Cell::new(0);
        let rebuild = || {
            calls.set(calls.get() + 1);
            Ok(())
        };
        sync_theme(&dir.0, DEFAULT, rebuild).unwrap();
        assert_eq!(fs::read(dir.0.join("logo.png")).unwrap(), DEFAULT);
        assert_eq!(
            fs::read_to_string(dir.0.join("livi.script")).unwrap(),
            SCRIPT
        );
        sync_theme(&dir.0, DEFAULT, rebuild).unwrap();
        assert_eq!(
            calls.get(),
            1,
            "unchanged images must not rebuild at every boot"
        );
        fs::write(dir.0.join("livi.script"), "old script").unwrap();
        sync_theme(&dir.0, DEFAULT, rebuild).unwrap();
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn failed_rebuilds_stay_pending_and_retry() {
        let dir = Temp::new();
        assert!(sync_theme(&dir.0, DEFAULT, || Err("disk full".into())).is_err());
        assert!(!dir.0.join(MARKER).exists());
        sync_theme(&dir.0, DEFAULT, || Ok(())).unwrap();
        assert!(dir.0.join(MARKER).exists());
        // If an installer restores a different logo, the next request repairs it.
        fs::write(dir.0.join("logo.png"), "older image").unwrap();
        let rebuilt = std::cell::Cell::new(false);
        sync_theme(&dir.0, DEFAULT, || {
            rebuilt.set(true);
            Ok(())
        })
        .unwrap();
        assert!(rebuilt.get());
    }

    #[test]
    fn a_changed_icon_retries_after_failure_and_reset_restores_the_original() {
        const OTHER: &[u8] = include_bytes!("../../../../assets/icons/linux/livi.png");
        let dir = Temp::new();
        sync_theme(&dir.0, DEFAULT, || Ok(())).unwrap();
        let original_marker = fs::read(dir.0.join(MARKER)).unwrap();
        assert!(sync_theme(&dir.0, OTHER, || Err("disk full".into())).is_err());
        assert_eq!(fs::read(dir.0.join(MARKER)).unwrap(), original_marker);
        let rebuilt = std::cell::Cell::new(false);
        sync_theme(&dir.0, OTHER, || {
            rebuilt.set(true);
            Ok(())
        })
        .unwrap();
        assert!(rebuilt.get());
        assert_eq!(fs::read(dir.0.join("logo.png")).unwrap(), OTHER);
        sync_theme(&dir.0, DEFAULT, || Ok(())).unwrap();
        assert_eq!(fs::read(dir.0.join("logo.png")).unwrap(), DEFAULT);
    }

    #[test]
    fn malformed_or_wrong_sized_images_are_rejected_before_any_write() {
        let dir = Temp::new();
        let mut wrong_size = DEFAULT.to_vec();
        wrong_size[16..20].copy_from_slice(&256u32.to_be_bytes());
        for image in [b"not a PNG".as_slice(), &wrong_size] {
            assert!(sync_theme(&dir.0, image, || panic!("must not rebuild")).is_err());
        }
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
    }
}
