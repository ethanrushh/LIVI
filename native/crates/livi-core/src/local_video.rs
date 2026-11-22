// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

mod history;
mod library;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use livi_core_proto::message::MediaControl;
use livi_core_proto::state::{Front, LocalVideo, LocalVideoEntry, LocalVideos};
use livi_media::compositor::CompositorControl;
use livi_media::output::{Output, OutputEvent};
use livi_media::planes::VideoPlane;
use livi_output_proto::{LOCAL_VIDEO_MAX, LOCAL_VIDEO_MIN, UrlStatus};
use tokio::sync::{mpsc, oneshot};

use crate::hub::Hub;
use history::History;
use library::Root;

pub enum Command {
    Browse(Option<String>),
    Open(String),
    Media(MediaControl),
    Play(bool),
    Seek(u32),
    Close,
    Up,
    Next,
    Restart,
    AutoNext(bool),
    Visible(bool),
}

trait Playback: Send {
    async fn open(&mut self, uri: &str, device: &str) -> Result<u32, String>;
    fn play(&self, playing: bool);
    fn seek(&self, seconds: f64);
    fn stop(&mut self);
}

struct NativePlayback {
    output: Output,
    compositor: CompositorControl,
    plane: Option<Arc<VideoPlane>>,
    next: u32,
}

impl Playback for NativePlayback {
    async fn open(&mut self, uri: &str, device: &str) -> Result<u32, String> {
        let id = self.next;
        self.next = if id == LOCAL_VIDEO_MAX {
            LOCAL_VIDEO_MIN
        } else {
            id + 1
        };
        let plane = VideoPlane::local(id, self.output.clone(), self.compositor.clone());
        if !plane.play(plane.turn(), uri, device).await {
            return Err("The video player could not start".into());
        }
        self.plane = Some(plane);
        Ok(id)
    }
    fn play(&self, playing: bool) {
        if let Some(p) = &self.plane {
            p.set_rate(if playing { 1.0 } else { 0.0 });
        }
    }
    fn seek(&self, seconds: f64) {
        if let Some(p) = &self.plane {
            p.seek(seconds);
        }
    }
    fn stop(&mut self) {
        if let Some(p) = self.plane.take() {
            p.stop();
        }
    }
}

struct Active {
    plane: u32,
    identity: String,
    resume: Option<f64>,
    seeking: Option<(f64, Instant)>,
    position_known: bool,
    last_status: Instant,
}

struct Player<B> {
    hub: Arc<Hub>,
    backend: B,
    history: History,
    history_path: PathBuf,
    roots: Vec<Root>,
    state: LocalVideos,
    active: Option<Active>,
    queue: Vec<String>,
    visible: bool,
    dirty: bool,
}

impl<B: Playback> Player<B> {
    fn publish(&self) {
        self.hub.update(|s| s.local_videos = self.state.clone());
    }

    async fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let (history, path) = (self.history.clone(), self.history_path.clone());
        match tokio::task::spawn_blocking(move || history.save(&path)).await {
            Ok(Ok(())) => self.dirty = false,
            result => {
                self.state.error = Some(match result {
                    Ok(Err(e)) => e,
                    Err(e) => format!("Cannot save video progress: {e}"),
                    _ => unreachable!(),
                })
            }
        }
    }

    fn decorate(&self, entries: &mut [LocalVideoEntry]) {
        for entry in entries {
            if let Some(p) = self.history.for_path(Path::new(&entry.path)) {
                entry.position_ms = p.position_ms;
                entry.duration_ms = p.duration_ms;
                entry.completed = p.completed;
            }
        }
    }

    async fn browse(&mut self, directory: Option<String>) -> Result<(), String> {
        self.state.loading = true;
        self.state.error = None;
        self.publish();
        self.roots = library::roots().await;
        let directory = match directory {
            Some(path) if path.starts_with("usb:") => {
                let mounted = library::mount(&path).await?;
                self.roots = library::roots().await;
                Some(mounted.to_string_lossy().into_owned())
            }
            other => other,
        };
        let entries = if let Some(dir) = &directory {
            let path = library::resolve(Path::new(dir), &self.roots)?;
            tokio::task::spawn_blocking(move || library::listing(&path))
                .await
                .map_err(|e| e.to_string())??
        } else {
            self.roots
                .iter()
                .map(|r| LocalVideoEntry {
                    path: r.path.clone(),
                    name: r.name.clone(),
                    directory: true,
                    ..Default::default()
                })
                .collect()
        };
        let mut entries = entries;
        self.decorate(&mut entries);
        self.state.directory = directory;
        self.state.entries = entries;
        self.state.loading = false;
        Ok(())
    }

    async fn open(&mut self, path: String, resume: bool) -> Result<(), String> {
        if !self.visible {
            return Err("Open the Videos page before playing a file".into());
        }
        if self.roots.is_empty() {
            self.roots = library::roots().await;
        }
        let file = library::resolve(Path::new(&path), &self.roots)?;
        if !library::is_video(&file) || !file.is_file() {
            return Err("Choose a supported video file".into());
        }
        let uri = reqwest::Url::from_file_path(&file)
            .map_err(|_| "Invalid video file path")?
            .to_string();
        let identified = file.clone();
        let (history, identity, entries) = tokio::task::spawn_blocking(move || {
            let mut history = History::default();
            let identity = history.identify(&identified)?;
            let parent = identified.parent().ok_or("Video has no parent folder")?;
            let entries = library::listing(parent)?;
            Ok::<_, String>((history, identity, entries))
        })
        .await
        .map_err(|e| e.to_string())??;
        // Validation precedes closing the current video, so an unavailable file
        // leaves the current player and its resume position intact.
        self.close(false).await;
        self.history.include_files(history);
        self.dirty = true;
        let saved = self.history.get(&identity);
        let position = if resume && !saved.completed {
            saved.position_ms
        } else {
            0.0
        };
        let device = self.hub.config().audio_output_device.unwrap_or_default();
        let id = self.backend.open(&uri, &device).await?;
        self.active = Some(Active {
            plane: id,
            identity,
            resume: (position > 0.0).then_some(position),
            seeking: None,
            position_known: false,
            last_status: Instant::now(),
        });
        self.state.current = Some(LocalVideo {
            path: file.to_string_lossy().into_owned(),
            name: file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            position_ms: position,
            duration_ms: saved.duration_ms,
            ..Default::default()
        });
        self.state.error = None;
        self.state.loading = true;
        self.queue = entries
            .iter()
            .filter(|e| !e.directory)
            .map(|e| e.path.clone())
            .collect();
        let mut entries = entries;
        self.decorate(&mut entries);
        self.state.directory = file.parent().map(|p| p.to_string_lossy().into_owned());
        self.state.entries = entries;
        Ok(())
    }

    fn remember(&mut self, completed: bool) {
        let Some((active, current)) = self.active.as_ref().zip(self.state.current.as_ref()) else {
            return;
        };
        if !completed && !active.position_known {
            return;
        }
        self.dirty |= self.history.remember(
            &active.identity,
            current.position_ms,
            current.duration_ms,
            completed,
        );
    }

    async fn close(&mut self, completed: bool) {
        self.remember(completed);
        self.backend.stop();
        self.active = None;
        self.state.current = None;
        self.state.loading = false;
        self.save().await;
        let mut entries = std::mem::take(&mut self.state.entries);
        self.decorate(&mut entries);
        self.state.entries = entries;
    }

    async fn next(&mut self) -> Result<(), String> {
        let next = self
            .state
            .current
            .as_ref()
            .and_then(|c| self.queue.iter().position(|p| *p == c.path))
            .and_then(|i| self.queue.get(i + 1))
            .cloned();
        match next {
            Some(path) => self.open(path, false).await,
            None => {
                self.close(false).await;
                Ok(())
            }
        }
    }

    fn seek(&mut self, position_ms: f64) {
        if !position_ms.is_finite() || position_ms < 0.0 {
            return;
        }
        if let Some((active, current)) = self.active.as_mut().zip(self.state.current.as_mut()) {
            if !current.seekable {
                return;
            }
            let position = current
                .duration_ms
                .map_or(position_ms, |d| position_ms.min(d));
            active.resume = None;
            active.position_known = true;
            active.seeking = Some((position, Instant::now()));
            current.position_ms = position;
            self.backend.seek(position / 1000.0);
        }
    }

    async fn status(&mut self, plane: u32, status: UrlStatus) -> Result<(), String> {
        let Some(active) = self.active.as_mut().filter(|a| a.plane == plane) else {
            return Ok(());
        };
        active.last_status = Instant::now();
        if status.failed {
            self.close(false).await;
            return Err("Video playback failed. Check that the drive is connected and the file's codecs are supported.".into());
        }
        let Some(current) = &mut self.state.current else {
            return Ok(());
        };
        self.state.loading = !status.ready && !status.ended;
        current.playing = status.playing && !status.ended;
        current.seekable = status.seekable.is_some();
        if let Some(duration) = status.duration.filter(|d| d.is_finite() && *d > 0.0) {
            current.duration_ms = Some(duration * 1000.0);
        }
        if status.ready
            && current.seekable
            && let Some(resume) = active.resume.take()
        {
            let target = current
                .duration_ms
                .map_or(resume, |d| resume.min((d - 1000.0).max(0.0)));
            self.backend.seek(target / 1000.0);
            active.seeking = Some((target, Instant::now()));
            current.position_ms = target;
        }
        if let Some(position) = status.position.filter(|p| p.is_finite() && *p >= 0.0) {
            let position_ms = position * 1000.0;
            let hold = active.resume.is_some()
                || active.seeking.is_some_and(|(target, at)| {
                    (target - position_ms).abs() > 2000.0 && at.elapsed() < Duration::from_secs(5)
                });
            if status.ready && !hold {
                active.seeking = None;
                active.position_known = true;
                current.position_ms = position_ms;
            }
        }
        if status.ended {
            // Only EOS counts as completion; pausing, closing, losing a drive and
            // decoder failures must never start another file or mark it watched.
            let next = self
                .state
                .auto_next
                .then(|| {
                    self.queue
                        .iter()
                        .position(|p| *p == current.path)
                        .and_then(|i| self.queue.get(i + 1))
                        .cloned()
                })
                .flatten();
            if let Some(duration) = current.duration_ms {
                current.position_ms = duration;
            }
            self.close(true).await;
            if let Some(path) = next {
                self.open(path, false).await?;
            }
        } else {
            self.remember(false);
        }
        Ok(())
    }

    async fn command(&mut self, command: Command) -> Result<(), String> {
        match command {
            Command::Browse(dir) => self.browse(dir).await?,
            Command::Open(path) => self.open(path, true).await?,
            Command::Play(playing) => {
                self.backend.play(playing);
                self.remember(false);
                self.save().await;
            }
            Command::Seek(ms) => self.seek(f64::from(ms)),
            Command::Close => self.close(false).await,
            Command::Up => {
                let parent = self
                    .state
                    .directory
                    .as_ref()
                    .and_then(|d| Path::new(d).parent())
                    .and_then(|p| library::resolve(p, &self.roots).ok())
                    .map(|p| p.to_string_lossy().into_owned());
                self.browse(parent).await?;
            }
            Command::Next => self.next().await?,
            Command::Restart => self.seek(0.0),
            Command::AutoNext(on) => {
                self.state.auto_next = on;
                self.history.auto_next = on;
                self.dirty = true;
                self.save().await;
            }
            Command::Visible(visible) => {
                self.visible = visible;
                if !visible {
                    self.close(false).await;
                } else if self.state.directory.is_none() {
                    self.browse(None).await?;
                }
            }
            Command::Media(control) => match control {
                MediaControl::Next => self.next().await?,
                MediaControl::Prev => self.seek(0.0),
                MediaControl::Play => self.backend.play(true),
                MediaControl::Pause => {
                    self.backend.play(false);
                    self.remember(false);
                    self.save().await;
                }
                MediaControl::PlayPause => self
                    .backend
                    .play(!self.state.current.as_ref().is_some_and(|c| c.playing)),
            },
        }
        Ok(())
    }
}

pub async fn run(
    hub: Arc<Hub>,
    output: Output,
    compositor: CompositorControl,
    mut commands: mpsc::UnboundedReceiver<Command>,
    history_path: PathBuf,
    mut stop: oneshot::Receiver<()>,
) {
    let history = History::load(&history_path);
    let error = history.as_ref().err().cloned();
    let history = history.unwrap_or_default();
    let mut player = Player {
        hub: hub.clone(),
        backend: NativePlayback {
            output: output.clone(),
            compositor,
            plane: None,
            next: LOCAL_VIDEO_MIN,
        },
        state: LocalVideos {
            auto_next: history.auto_next,
            error,
            ..Default::default()
        },
        history,
        history_path,
        roots: Vec::new(),
        active: None,
        queue: Vec::new(),
        visible: false,
        dirty: false,
    };
    player.publish();
    let mut events = output.subscribe();
    let mut state = hub.watch();
    let mut checkpoint = tokio::time::interval(Duration::from_secs(5));
    checkpoint.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let result = tokio::select! {
            _ = &mut stop => { player.close(false).await; player.publish(); return; }
            command = commands.recv() => match command {
                Some(command) => player.command(command).await,
                None => { player.close(false).await; return; }
            },
            event = events.recv() => match event {
                Ok(OutputEvent::UrlStatus { plane, status }) => player.status(plane, status).await,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => { player.close(false).await; return; }
                _ => Ok(()),
            },
            changed = state.changed() => {
                if changed.is_err() { player.close(false).await; return; }
                let front = state.borrow_and_update().front.main;
                if front != Front::Livi { player.close(false).await; }
                Ok(())
            },
            _ = checkpoint.tick() => {
                player.save().await;
                if player.active.as_ref().is_some_and(|a| a.last_status.elapsed() > Duration::from_secs(20)) {
                    player.close(false).await;
                    Err("The video player stopped responding".into())
                } else { Ok(()) }
            },
        };
        if let Err(error) = result {
            player.state.error = Some(error);
            player.state.loading = false;
        }
        player.publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::config_file::tests::TempDir;
    use livi_core_proto::state::State;

    #[derive(Default)]
    struct TestPlayback {
        uris: Vec<String>,
        seek: std::sync::Mutex<Option<f64>>,
    }

    impl Playback for TestPlayback {
        async fn open(&mut self, uri: &str, _: &str) -> Result<u32, String> {
            self.uris.push(uri.to_string());
            Ok(self.uris.len() as u32)
        }
        fn play(&self, _: bool) {}
        fn seek(&self, seconds: f64) {
            *self.seek.lock().unwrap() = Some(seconds);
        }
        fn stop(&mut self) {}
    }

    #[tokio::test]
    async fn browse_resume_then_advance_on_eos_without_accepting_old_status() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("Videos/Music");
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("Track #2.mp4");
        let second = dir.join("Track #10.mp4");
        std::fs::write(&first, "first video").unwrap();
        std::fs::write(&second, "second video").unwrap();
        let history_path = tmp.0.join("state/local-videos.json");
        let path = first.to_str().unwrap().to_string();
        let hub = Arc::new(Hub::new(State::default()));
        let mut player = Player {
            hub,
            backend: TestPlayback::default(),
            history: History::default(),
            history_path: history_path.clone(),
            roots: vec![Root {
                path: dir.to_str().unwrap().into(),
                name: "Music".into(),
            }],
            state: LocalVideos::default(),
            active: None,
            queue: Vec::new(),
            visible: true,
            dirty: false,
        };
        player.command(Command::Open(path.clone())).await.unwrap();
        assert!(player.backend.uris[0].ends_with("Track%20%232.mp4"));
        let playing = UrlStatus {
            ready: true,
            playing: true,
            position: Some(42.0),
            duration: Some(100.0),
            seekable: Some((0.0, 100.0)),
            ..Default::default()
        };
        player.status(1, playing).await.unwrap();
        player.command(Command::AutoNext(true)).await.unwrap();
        player.command(Command::Close).await.unwrap();
        assert_eq!(player.backend.uris.len(), 1, "closing does not advance");

        player.history = History::load(&history_path).unwrap();
        player.command(Command::Open(path)).await.unwrap();
        player
            .status(
                2,
                UrlStatus {
                    position: Some(0.0),
                    ..playing
                },
            )
            .await
            .unwrap();
        assert_eq!(*player.backend.seek.lock().unwrap(), Some(42.0));
        player
            .status(
                2,
                UrlStatus {
                    ended: true,
                    ..playing
                },
            )
            .await
            .unwrap();
        assert_eq!(
            player.state.current.as_ref().unwrap().path,
            second.to_str().unwrap()
        );
        assert!(player.state.entries[0].completed);
        player
            .status(
                2,
                UrlStatus {
                    ended: true,
                    ..playing
                },
            )
            .await
            .unwrap();
        assert_eq!(
            player.backend.uris.len(),
            3,
            "a late EOS cannot advance the next player"
        );
        assert!(
            player
                .status(
                    3,
                    UrlStatus {
                        failed: true,
                        ..playing
                    }
                )
                .await
                .is_err()
        );
        assert!(player.state.current.is_none());
        assert!(
            !player.state.entries[1].completed,
            "a decode failure does not mark the file watched"
        );
    }
}
