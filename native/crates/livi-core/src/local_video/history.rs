// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use livi_base::fs::{PRIVATE, write_atomic};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Progress {
    pub position_ms: f64,
    pub duration_ms: Option<f64>,
    pub completed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct File {
    id: String,
    size: u64,
    modified_ns: u128,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct History {
    pub auto_next: bool,
    progress: BTreeMap<String, Progress>,
    files: BTreeMap<PathBuf, File>,
}

fn stamp(path: &Path) -> Result<(u64, u128), String> {
    let m = path.metadata().map_err(|e| e.to_string())?;
    if !m.is_file() {
        return Err("This is not a regular video file".into());
    }
    let modified = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    Ok((m.len(), modified))
}

// Bounded reads keep opening a multi-gigabyte movie quick. Content identity survives
// a USB mount-point change or a rename, while replacement files get their own record.
fn fingerprint(path: &Path, size: u64) -> Result<String, String> {
    const SAMPLE: u64 = 64 * 1024;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = size.to_le_bytes().to_vec();
    for at in [
        0,
        size.saturating_sub(SAMPLE) / 2,
        size.saturating_sub(SAMPLE),
    ] {
        file.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
        let mut sample = vec![0; size.saturating_sub(at).min(SAMPLE) as usize];
        file.read_exact(&mut sample).map_err(|e| e.to_string())?;
        bytes.extend(sample);
    }
    Ok(livi_crypto::sha256(&[&bytes])
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

impl History {
    pub fn include_files(&mut self, identified: Self) {
        self.files.extend(identified.files);
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("Cannot read video progress: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("Cannot read video progress: {e}")),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        write_atomic(path, bytes, PRIVATE).map_err(|e| format!("Cannot save video progress: {e}"))
    }

    pub fn identify(&mut self, path: &Path) -> Result<String, String> {
        let (size, modified_ns) = stamp(path)?;
        let id = fingerprint(path, size)?;
        self.files.insert(
            path.to_path_buf(),
            File {
                id: id.clone(),
                size,
                modified_ns,
            },
        );
        Ok(id)
    }

    pub fn for_path(&self, path: &Path) -> Option<&Progress> {
        let file = self.files.get(path)?;
        let (size, modified) = stamp(path).ok()?;
        (file.size == size && file.modified_ns == modified)
            .then(|| self.progress.get(&file.id))
            .flatten()
    }

    pub fn get(&self, id: &str) -> Progress {
        self.progress.get(id).cloned().unwrap_or_default()
    }

    pub fn remember(
        &mut self,
        id: &str,
        position_ms: f64,
        duration_ms: Option<f64>,
        completed: bool,
    ) -> bool {
        if !position_ms.is_finite() || position_ms < 0.0 {
            return false;
        }
        let duration_ms = duration_ms.filter(|d| d.is_finite() && *d > 0.0);
        let next = Progress {
            position_ms: duration_ms.map_or(position_ms, |d| position_ms.min(d)),
            duration_ms,
            completed,
        };
        if self.progress.get(id) == Some(&next) {
            return false;
        }
        self.progress.insert(id.into(), next);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::config_file::tests::TempDir;

    #[test]
    fn progress_and_autoplay_survive_restarts_and_mount_point_changes() {
        let dir = TempDir::new();
        let movie = dir.0.join("movie.mp4");
        std::fs::write(&movie, b"one movie").unwrap();
        let mut h = History::default();
        let id = h.identify(&movie).unwrap();
        h.remember(&id, 42_000.0, Some(100_000.0), false);
        h.auto_next = true;
        let db = dir.0.join("state/videos.json");
        h.save(&db).unwrap();
        let mut h = History::load(&db).unwrap();
        assert!(h.auto_next);
        assert_eq!(h.for_path(&movie).unwrap().position_ms, 42_000.0);
        let moved = dir.0.join("other mount.mp4");
        std::fs::rename(&movie, &moved).unwrap();
        assert_eq!(h.identify(&moved).unwrap(), id);
        assert_eq!(h.for_path(&moved).unwrap().position_ms, 42_000.0);
        std::fs::write(&moved, b"new movie").unwrap();
        let new_id = h.identify(&moved).unwrap();
        assert_ne!(new_id, id);
        assert_eq!(h.get(&new_id), Progress::default());
    }
}
