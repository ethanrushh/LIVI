// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Duration;

use livi_core_proto::state::LocalVideoEntry;
use serde::Deserialize;

#[derive(Clone, Debug)]
pub(super) struct Root {
    pub path: String,
    pub name: String,
}

pub fn is_video(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        // These containers have demuxers in the Linux AppImage.
        matches!(
            e.to_ascii_lowercase().as_str(),
            "mp4" | "m4v" | "mov" | "ts" | "m2ts" | "mts"
        )
    })
}

fn name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

// Compare digit runs numerically so track 2 plays before track 10, without parsing
// arbitrarily long numbers or depending on directory enumeration order.
fn natural(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let (mut a, mut b) = (a.as_str(), b.as_str());
    while !a.is_empty() && !b.is_empty() {
        let digits = a.as_bytes()[0].is_ascii_digit() && b.as_bytes()[0].is_ascii_digit();
        if digits {
            let ai = a.bytes().take_while(u8::is_ascii_digit).count();
            let bi = b.bytes().take_while(u8::is_ascii_digit).count();
            let (an, bn) = (
                a[..ai].trim_start_matches('0'),
                b[..bi].trim_start_matches('0'),
            );
            let cmp = an.len().cmp(&bn.len()).then_with(|| an.cmp(bn));
            if cmp != Ordering::Equal {
                return cmp;
            }
            a = &a[ai..];
            b = &b[bi..];
        } else {
            let (ac, bc) = (a.chars().next().unwrap(), b.chars().next().unwrap());
            let cmp = ac.cmp(&bc);
            if cmp != Ordering::Equal {
                return cmp;
            }
            a = &a[ac.len_utf8()..];
            b = &b[bc.len_utf8()..];
        }
    }
    a.len().cmp(&b.len())
}

pub(super) fn listing(dir: &Path) -> Result<Vec<LocalVideoEntry>, String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("Cannot open {}: {e}", dir.display()))?;
    let mut entries: Vec<_> = read
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let name = name(&path);
            let kind = e.file_type().ok()?;
            let directory = kind.is_dir();
            if name.starts_with('.') || (!directory && (!kind.is_file() || !is_video(&path))) {
                return None;
            }
            Some(LocalVideoEntry {
                path: path.to_str()?.into(),
                name,
                directory,
                ..Default::default()
            })
        })
        .collect();
    entries.sort_by(|a, b| {
        b.directory
            .cmp(&a.directory)
            .then_with(|| natural(&a.name, &b.name))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(entries)
}

pub(super) fn resolve(path: &Path, roots: &[Root]) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
    let allowed = roots
        .iter()
        .filter(|r| !r.path.starts_with("usb:"))
        .filter_map(|r| Path::new(&r.path).canonicalize().ok())
        .any(|root| canonical.starts_with(root));
    if !allowed {
        return Err("Choose a file inside Videos or a mounted media drive".into());
    }
    Ok(canonical)
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Devices {
    blockdevices: Vec<Device>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Device {
    path: String,
    tran: Option<String>,
    rm: bool,
    fstype: Option<String>,
    label: Option<String>,
    mountpoints: Vec<Option<String>>,
    children: Vec<Device>,
}

fn usb_roots(devices: Vec<Device>, parent_usb: bool, roots: &mut Vec<Root>) {
    for d in devices {
        let usb = parent_usb || d.rm || d.tran.as_deref() == Some("usb");
        if usb
            && d.fstype
                .as_deref()
                .is_some_and(|f| !f.is_empty() && f != "crypto_LUKS" && f != "swap")
        {
            let mounted = d
                .mountpoints
                .into_iter()
                .flatten()
                .find(|p| p.starts_with('/') && p != "/");
            let title = d
                .label
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| name(Path::new(&d.path)));
            roots.push(Root {
                path: mounted.unwrap_or_else(|| format!("usb:{}", d.path)),
                name: title,
            });
        }
        usb_roots(d.children, usb, roots);
    }
}

pub(super) async fn roots() -> Vec<Root> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let videos = PathBuf::from(home).join("Videos");
        if tokio::fs::create_dir_all(&videos).await.is_ok()
            && let Some(path) = videos.to_str()
        {
            roots.push(Root {
                path: path.into(),
                name: "Videos".into(),
            });
        }
    }
    if cfg!(target_os = "linux") {
        let output = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::process::Command::new("lsblk")
                .args([
                    "--json",
                    "--tree",
                    "--paths",
                    "--output",
                    "PATH,TRAN,RM,FSTYPE,LABEL,MOUNTPOINTS",
                ])
                .kill_on_drop(true)
                .output(),
        )
        .await;
        if let Ok(Ok(o)) = output
            && o.status.success()
            && let Ok(devices) = serde_json::from_slice::<Devices>(&o.stdout)
        {
            usb_roots(devices.blockdevices, false, &mut roots);
        }
    }
    // Also expose manually mounted disks and desktop mount locations. No recursive
    // scan is needed; the browser walks folders as the user opens them.
    for path in ["/media", "/run/media", "/mnt", "/Volumes"] {
        if Path::new(path).is_dir() {
            roots.push(Root {
                path: path.into(),
                name: format!("Drives ({path})"),
            });
        }
    }
    roots.dedup_by(|a, b| a.path == b.path);
    roots
}

pub(super) async fn mount(path: &str) -> Result<PathBuf, String> {
    // Re-discover instead of trusting a device path supplied over the control socket.
    if !roots()
        .await
        .iter()
        .any(|r| r.path == path && r.path.starts_with("usb:/dev/"))
    {
        return Err("This USB drive is no longer available".into());
    }
    let device = path.strip_prefix("usb:").ok_or("Invalid USB drive")?;
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new("udisksctl")
            .args([
                "mount",
                "--block-device",
                device,
                "--options",
                "ro",
                "--no-user-interaction",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "USB drive mount timed out".to_string())?
    .map_err(|e| {
        format!("Cannot mount USB drive: {e}. Install udisks2 or mount the drive through the OS.")
    })?;
    if !output.status.success() {
        return Err(format!(
            "Cannot mount USB drive: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // UDisks exposes its mount-point through lsblk; do not parse its localized prose.
    let output = tokio::process::Command::new("lsblk")
        .args(["--json", "--paths", "--output", "PATH,MOUNTPOINTS", device])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    let devices: Devices = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    devices
        .blockdevices
        .into_iter()
        .find(|d| d.path == device)
        .and_then(|d| d.mountpoints.into_iter().flatten().next())
        .map(PathBuf::from)
        .ok_or_else(|| "The USB drive mounted but its location is unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::config_file::tests::TempDir;

    #[test]
    fn folders_and_videos_are_sorted_for_playlist_playback() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.0.join("album")).unwrap();
        for file in [
            "track 10.MP4",
            "track 2.mov",
            "track 01.mp4",
            "notes.txt",
            ".hidden.mp4",
        ] {
            std::fs::write(dir.0.join(file), b"video").unwrap();
        }
        let entries = listing(&dir.0).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["album", "track 01.mp4", "track 2.mov", "track 10.MP4"]
        );
        let nested = dir.0.join("album/further");
        std::fs::create_dir(&nested).unwrap();
        let roots = [Root {
            path: dir.0.display().to_string(),
            name: "test".into(),
        }];
        assert_eq!(resolve(&nested, &roots).unwrap(), nested);
        assert!(resolve(Path::new("/etc/passwd"), &roots).is_err());
        std::os::unix::fs::symlink("/etc", dir.0.join("outside")).unwrap();
        assert!(resolve(&dir.0.join("outside/passwd"), &roots).is_err());
    }

    #[test]
    fn usb_partitions_inherit_the_disks_transport_and_internal_disks_stay_hidden() {
        let devices: Devices = serde_json::from_value(serde_json::json!({"blockdevices": [
            {"path":"/dev/sda", "tran":"usb", "children":[
                {"path":"/dev/sda1", "fstype":"exfat", "label":"Movies", "mountpoints":[null]},
                {"path":"/dev/sda2", "fstype":"ext4", "mountpoints":["/media/pi/Music"]}
            ]},
            {"path":"/dev/mmcblk0", "tran":"mmc", "children":[
                {"path":"/dev/mmcblk0p2", "fstype":"ext4", "mountpoints":["/"]}
            ]}
        ]}))
        .unwrap();
        let mut roots = Vec::new();
        usb_roots(devices.blockdevices, false, &mut roots);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].path, "usb:/dev/sda1");
        assert_eq!(roots[1].path, "/media/pi/Music");
    }
}
