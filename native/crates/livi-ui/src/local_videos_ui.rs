// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::rc::Rc;

use livi_core_client::Client;
use livi_core_proto::message::Action;
use livi_core_proto::state::LocalVideos;
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::{AppWindow, LocalVideoRow, LocalVideosState};

fn clock(ms: f64) -> String {
    let seconds = (ms.max(0.0) / 1000.0) as u64;
    format!(
        "{}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

pub fn apply(ui: &AppWindow, videos: LocalVideos) {
    let s = ui.global::<LocalVideosState>();
    s.set_title(
        videos
            .directory
            .as_deref()
            .and_then(|d| std::path::Path::new(d).file_name())
            .map_or_else(
                || "Local videos".into(),
                |n| n.to_string_lossy().into_owned(),
            )
            .into(),
    );
    s.set_can_back(videos.directory.is_some());
    s.set_auto_next(videos.auto_next);
    s.set_loading(videos.loading);
    s.set_error(videos.error.unwrap_or_default().into());
    let rows: Vec<LocalVideoRow> = videos
        .entries
        .iter()
        .map(|e| {
            let detail = if e.directory {
                if e.path.starts_with("usb:") {
                    "USB drive · Tap to mount".into()
                } else {
                    "Folder".into()
                }
            } else if e.completed {
                "Watched · Play from start".into()
            } else if e.position_ms > 0.0 {
                format!("Resume at {}", clock(e.position_ms))
            } else {
                "Play video".into()
            };
            LocalVideoRow {
                name: e.name.clone().into(),
                detail: detail.into(),
                directory: e.directory,
                progress: if e.completed {
                    1.0
                } else {
                    e.duration_ms
                        .filter(|d| *d > 0.0)
                        .map_or(0.0, |d| (e.position_ms / d).clamp(0.0, 1.0) as f32)
                },
            }
        })
        .collect();
    // Status arrives several times a second. Keep the list model unless its rows
    // changed so playback updates don't reset the browser's scroll position.
    use slint::Model;
    let old = s.get_entries();
    if old.row_count() != rows.len()
        || rows
            .iter()
            .enumerate()
            .any(|(i, r)| old.row_data(i).as_ref() != Some(r))
    {
        s.set_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
    }
    let active = videos.current.is_some();
    if let Some(current) = videos.current {
        if !s.get_active() || s.get_filename().as_str() != current.name {
            s.set_controls_shown(true);
            s.set_controls_idle(0);
        }
        s.set_filename(current.name.into());
        s.set_playing(current.playing);
        s.set_seekable(current.seekable);
        s.set_position_ms(current.position_ms as f32);
        s.set_duration_ms(current.duration_ms.unwrap_or(0.0) as f32);
        let files: Vec<_> = videos.entries.iter().filter(|e| !e.directory).collect();
        s.set_has_next(
            files
                .iter()
                .position(|e| e.path == current.path)
                .is_some_and(|i| i + 1 < files.len()),
        );
    }
    s.set_active(active);
}

pub fn wire(ui: &AppWindow, client: &Client) {
    let s = ui.global::<LocalVideosState>();
    s.on_pick({
        let c = client.clone();
        move |index| {
            if let Some(e) = usize::try_from(index)
                .ok()
                .and_then(|i| c.mirror().state.local_videos.entries.get(i).cloned())
            {
                c.act(if e.directory {
                    Action::LocalVideoBrowse {
                        directory: Some(e.path),
                    }
                } else {
                    Action::LocalVideoOpen { path: e.path }
                });
            }
        }
    });
    s.on_back({
        let c = client.clone();
        move || c.act(Action::LocalVideoUp)
    });
    s.on_sources({
        let c = client.clone();
        move || c.act(Action::LocalVideoBrowse { directory: None })
    });
    s.on_refresh({
        let c = client.clone();
        move || {
            c.act(Action::LocalVideoBrowse {
                directory: c.mirror().state.local_videos.directory.clone(),
            })
        }
    });
    s.on_auto_next_changed({
        let c = client.clone();
        move |on| c.act(Action::LocalVideoAutoNext { on })
    });
    s.on_play_pause({
        let c = client.clone();
        move || {
            let playing = c
                .mirror()
                .state
                .local_videos
                .current
                .as_ref()
                .is_some_and(|v| v.playing);
            c.act(Action::LocalVideoPlay { playing: !playing });
        }
    });
    s.on_seek({
        let c = client.clone();
        move |fraction| {
            let duration = c
                .mirror()
                .state
                .local_videos
                .current
                .as_ref()
                .and_then(|v| v.duration_ms);
            if let Some(duration) = duration
                && fraction.is_finite()
            {
                c.act(Action::LocalVideoSeek {
                    position_ms: (duration * f64::from(fraction.clamp(0.0, 1.0))).round() as u32,
                });
            }
        }
    });
    s.on_close({
        let c = client.clone();
        move || c.act(Action::LocalVideoClose)
    });
    s.on_next({
        let c = client.clone();
        move || c.act(Action::LocalVideoNext)
    });
    s.on_restart({
        let c = client.clone();
        move || c.act(Action::LocalVideoRestart)
    });
}
