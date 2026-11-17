// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use livi_core_proto::config::DEFAULT_STARTUP_VOLUME_LIMIT;
use serde::Deserialize;
use tokio::sync::mpsc;

use super::{OutputEvent, pactl};

const NORMAL: f64 = 65536.0;
const RETRY: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_secs(1);
const ATTEMPTS: usize = 10;

#[derive(Deserialize)]
struct Channel {
    value: u32,
}

#[derive(Deserialize)]
struct Sink {
    index: u32,
    name: String,
    channel_map: String,
    volume: HashMap<String, Channel>,
}

impl Sink {
    fn id(&self) -> (u32, String) {
        (self.index, self.name.clone())
    }

    fn channels(&self) -> Result<Vec<u32>, String> {
        let channels: Option<Vec<_>> = self
            .channel_map
            .split(',')
            .map(|name| self.volume.get(name.trim()).map(|v| v.value))
            .collect();
        channels
            .filter(|v| !v.is_empty() && v.len() == self.volume.len())
            .ok_or_else(|| format!("{} has an unreadable channel volume", self.name))
    }
}

fn ceiling(limit: f64) -> u32 {
    let limit = if limit.is_finite() && (0.0..=1.0).contains(&limit) {
        limit
    } else {
        DEFAULT_STARTUP_VOLUME_LIMIT
    };
    // Round down: the raw PulseAudio value must never exceed the requested ceiling.
    (limit * NORMAL).floor() as u32
}

trait Outputs {
    async fn list(&self) -> Result<Vec<Sink>, String>;
    async fn set(&self, sink: &Sink, channels: &[u32]) -> Result<(), String>;
}

struct Pulse;

impl Outputs for Pulse {
    async fn list(&self) -> Result<Vec<Sink>, String> {
        // Sinks only. Never enumerate or change sources (microphones or monitors).
        let json = pactl::pactl(&["--format=json", "list", "sinks"])
            .await
            .map_err(|e| e.to_string())?;
        serde_json::from_str(&json).map_err(|e| format!("cannot read output volumes: {e}"))
    }

    async fn set(&self, sink: &Sink, channels: &[u32]) -> Result<(), String> {
        let mut args = vec!["set-sink-volume".to_string(), sink.index.to_string()];
        args.extend(channels.iter().map(u32::to_string));
        let args: Vec<_> = args.iter().map(String::as_str).collect();
        pactl::pactl(&args)
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    }
}

type Known = HashSet<(u32, String)>;

async fn clamp(outputs: &impl Outputs, known: &Known, max: u32) -> Result<Known, String> {
    let sinks = outputs.list().await?;
    if sinks.is_empty() {
        return Err("no audio outputs available to verify".into());
    }
    let mut checked = Known::new();
    let mut errors = Vec::new();
    for sink in &sinks {
        checked.insert(sink.id());
        if known.contains(&sink.id()) {
            continue;
        }
        let channels = match sink.channels() {
            Ok(channels) => channels,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        let clamped: Vec<_> = channels.iter().map(|v| (*v).min(max)).collect();
        if clamped != channels {
            if let Err(e) = outputs.set(sink, &clamped).await {
                errors.push(format!("{}: {e}", sink.name));
            } else {
                println!(
                    "[volume] startup clamp on {} to at most {:.0} %",
                    sink.name,
                    f64::from(max) / NORMAL * 100.0
                );
            }
        }
    }
    // A successful write is not enough: detect ignored writes and outputs arriving mid-pass.
    let verified = outputs.list().await?;
    let verified_ids: Known = verified.iter().map(Sink::id).collect();
    if verified_ids != checked {
        errors.push("audio outputs changed during the startup clamp".into());
    }
    for sink in &verified {
        if !known.contains(&sink.id()) {
            match sink.channels() {
                Ok(channels) if channels.iter().any(|v| *v > max) => {
                    errors.push(format!(
                        "{} still exceeds the startup volume limit",
                        sink.name
                    ));
                }
                Err(e) => errors.push(e),
                _ => {}
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(verified_ids)
}

pub struct Guard {
    known: Known,
    max: u32,
    events: mpsc::UnboundedReceiver<OutputEvent>,
    _watch: pactl::Watch,
}

/// Must complete before starting any playback, phone connections or volume writers.
pub async fn prepare(limit: f64) -> Result<Guard, String> {
    let (tx, events) = mpsc::unbounded_channel();
    let watch = pactl::watch(tx);
    let max = ceiling(limit);
    let mut error = String::new();
    for attempt in 0..ATTEMPTS {
        match clamp(&Pulse, &Known::new(), max).await {
            Ok(known) => {
                return Ok(Guard {
                    known,
                    max,
                    events,
                    _watch: watch,
                });
            }
            Err(e) => error = e,
        }
        if attempt + 1 < ATTEMPTS {
            tokio::time::sleep(RETRY).await;
        }
    }
    Err(format!(
        "startup volume could not be verified; refusing to start playback: {error}"
    ))
}

impl Guard {
    /// Best effort for late outputs and audio-server restarts. This is not an atomic
    /// audio-server policy: other applications can play before an event is handled.
    pub async fn follow(mut self) {
        let mut poll = tokio::time::interval(POLL);
        loop {
            tokio::select! {
                _ = poll.tick() => {}
                event = self.events.recv() => match event {
                    Some(OutputEvent::Reconnected) => self.known.clear(),
                    Some(OutputEvent::New) => {},
                    Some(OutputEvent::Changed) => continue,
                    None => return,
                },
            }
            match clamp(&Pulse, &self.known, self.max).await {
                Ok(known) => self.known = known,
                Err(e) => eprintln!("[volume] late output startup clamp failed, retrying: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        json: Mutex<serde_json::Value>,
        writes: Mutex<Vec<(u32, Vec<u32>)>>,
        ignore_writes: bool,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                json: Mutex::new(serde_json::json!([
                    {"index": 1, "name": "dac", "channel_map": "front-left,front-right",
                     "volume": {"front-left": {"value": 0}, "front-right": {"value": 98304}}},
                    {"index": 2, "name": "hdmi", "channel_map": "mono",
                     "volume": {"mono": {"value": 1000}}},
                    {"index": 3, "name": "suspended", "channel_map": "mono",
                     "volume": {"mono": {"value": 65536}}}
                ])),
                writes: Mutex::new(Vec::new()),
                ignore_writes: false,
            }
        }
    }

    impl Outputs for Fake {
        async fn list(&self) -> Result<Vec<Sink>, String> {
            serde_json::from_value(self.json.lock().unwrap().clone()).map_err(|e| e.to_string())
        }

        async fn set(&self, sink: &Sink, channels: &[u32]) -> Result<(), String> {
            self.writes
                .lock()
                .unwrap()
                .push((sink.index, channels.to_vec()));
            if !self.ignore_writes {
                let mut json = self.json.lock().unwrap();
                let row = json
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|v| v["index"] == sink.index)
                    .unwrap();
                for (name, value) in sink.channel_map.split(',').zip(channels) {
                    row["volume"][name]["value"] = (*value).into();
                }
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn all_outputs_and_channels_are_clamped_without_raising_quiet_channels() {
        let outputs = Fake::new();
        let known = clamp(&outputs, &Known::new(), ceiling(0.15)).await.unwrap();
        assert_eq!(known.len(), 3);
        assert_eq!(
            *outputs.writes.lock().unwrap(),
            [(1, vec![0, 9830]), (3, vec![9830])]
        );
        assert_eq!(
            outputs.json.lock().unwrap()[1]["volume"]["mono"]["value"],
            1000
        );
    }

    #[tokio::test]
    async fn zero_is_silent_and_an_ignored_write_fails_verification() {
        let mut outputs = Fake::new();
        outputs.ignore_writes = true;
        assert!(clamp(&outputs, &Known::new(), ceiling(0.0)).await.is_err());
        outputs.ignore_writes = false;
        clamp(&outputs, &Known::new(), ceiling(0.0)).await.unwrap();
        for sink in outputs.list().await.unwrap() {
            assert!(sink.channels().unwrap().iter().all(|v| *v == 0));
        }
    }

    #[tokio::test]
    async fn existing_outputs_may_be_raised_deliberately_but_new_ones_are_clamped() {
        let outputs = Fake::new();
        let known = clamp(&outputs, &Known::new(), ceiling(0.15)).await.unwrap();
        outputs.json.lock().unwrap()[0]["volume"]["front-right"]["value"] = 50000.into();
        {
            let mut json = outputs.json.lock().unwrap();
            json.as_array_mut().unwrap().push(serde_json::json!({
                "index": 4, "name": "bluetooth", "channel_map": "mono",
                "volume": {"mono": {"value": 65536}}
            }));
        }
        let known = clamp(&outputs, &known, ceiling(0.15)).await.unwrap();
        assert_eq!(known.len(), 4);
        assert_eq!(
            outputs.json.lock().unwrap()[0]["volume"]["front-right"]["value"],
            50000
        );
        assert_eq!(
            outputs.writes.lock().unwrap().last(),
            Some(&(4, vec![9830]))
        );
        // Reconnecting the audio server discards previous outputs and clamps them all.
        clamp(&outputs, &Known::new(), ceiling(0.15)).await.unwrap();
        assert_eq!(
            outputs.json.lock().unwrap()[0]["volume"]["front-right"]["value"],
            9830
        );
    }

    #[tokio::test]
    async fn missing_outputs_or_unreadable_channels_do_not_pass() {
        let outputs = Fake::new();
        outputs.json.lock().unwrap()[0]["channel_map"] = "missing".into();
        assert!(clamp(&outputs, &Known::new(), ceiling(0.15)).await.is_err());
        *outputs.json.lock().unwrap() = serde_json::json!([]);
        assert!(clamp(&outputs, &Known::new(), ceiling(0.15)).await.is_err());
    }

    #[test]
    fn ceiling_rounds_down_and_bad_limits_fall_back_to_fifteen_percent() {
        assert_eq!(ceiling(0.15), 9830);
        assert_eq!(ceiling(0.0), 0);
        assert_eq!(ceiling(1.0), 65536);
        for bad in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            assert_eq!(ceiling(bad), 9830);
        }
    }
}
