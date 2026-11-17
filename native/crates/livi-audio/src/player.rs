// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! One CarPlay audio stream from its RTP packets to the sink.

pub mod uplink;

use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_audio as gst_audio;
use gstreamer_controller as gst_controller;
use gstreamer_controller::prelude::*;

use crate::stream::{Codec, RTP_HEADER_LEN, aac_raw_caps, alac_caps, reframe_aac, rtp_caps};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};

pub struct Config {
    pub codec: Codec,
    pub payload_type: u8,
    pub clock_rate: u32,
    pub channels: u8,
    pub latency_ms: u32,
    pub media_buffer_ms: u32,
    pub realtime: bool,
    pub device: Option<String>,
    pub label: String,
    pub codec_data: Vec<u8>,
}

impl Config {
    fn media_buffer_ms(&self) -> u32 {
        if self.realtime {
            0
        } else {
            self.media_buffer_ms.min(2000)
        }
    }

    fn buffered_pcm(&self) -> bool {
        matches!(self.codec, Codec::Pcm | Codec::PcmLe) && self.media_buffer_ms() > 0
    }
}

fn decoder_chain(codec: Codec) -> Option<&'static str> {
    match codec {
        Codec::Pcm | Codec::PcmLe => None,
        Codec::Opus => Some("rtpjitterbuffer latency={latency} ! rtpopusdepay ! opusdec"),
        Codec::AacRaw => Some("aacparse ! faad"),
        Codec::Alac | Codec::Alac24 => Some("avdec_alac"),
        Codec::AacEld => Some(
            "rtpjitterbuffer latency={latency} ! rtpmp4gdepay ! \
             audio/mpeg,rate={rate},channels=2 ! avdec_aac",
        ),
        #[cfg(target_os = "linux")]
        Codec::AacLc => Some("rtpjitterbuffer latency={latency} ! rtpmp4gdepay ! aacparse ! faad"),
        #[cfg(not(target_os = "linux"))]
        Codec::AacLc => {
            Some("rtpjitterbuffer latency={latency} ! rtpmp4gdepay ! aacparse ! avdec_aac")
        }
    }
}

const fn audio_sink() -> (&'static str, &'static str) {
    if cfg!(target_vendor = "apple") {
        ("osxaudiosink", "unique-id")
    } else if cfg!(target_os = "android") {
        ("aaudiosink", "device")
    } else {
        ("pulsesink", "device")
    }
}

const SINK_NAME: &str = "audio-sink";

fn sink_chain(cfg: &Config) -> String {
    let (element, _device_property) = audio_sink();
    if cfg.realtime {
        format!("{element} name={SINK_NAME} sync=false")
    } else {
        format!("{element} name={SINK_NAME} sync=false buffer-time=300000 latency-time=30000")
    }
}

pub fn pipeline_desc(cfg: &Config) -> String {
    let head = match decoder_chain(cfg.codec) {
        Some(chain) => {
            format!(
                "{} ! ",
                chain
                    .replace(
                        "{latency}",
                        &(cfg
                            .latency_ms
                            .max(100)
                            .saturating_add(cfg.media_buffer_ms()))
                        .to_string()
                    )
                    .replace("{rate}", &cfg.clock_rate.to_string())
            )
        }
        None => String::new(),
    };
    format!(
        "appsrc name=src is-live=true format=time do-timestamp={stamp} ! \
         {head}audioconvert ! clocksync sync=true ! volume name=vol ! \
         audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate=48000,channels=2 ! \
         queue name=out ! {sink}",
        stamp = cfg.codec != Codec::AacRaw && !cfg.buffered_pcm(),
        sink = sink_chain(cfg),
    )
}

struct UnitClock {
    next: Option<gst::ClockTime>,
    frame: gst::ClockTime,
}

impl UnitClock {
    fn schedule(
        &mut self,
        now: gst::ClockTime,
        headroom: gst::ClockTime,
        duration: gst::ClockTime,
    ) -> gst::ClockTime {
        let pts = match self.next {
            Some(next) if next >= now => next,
            _ => now + headroom,
        };
        self.next = Some(pts + duration);
        pts
    }
}

const UNIT_HEADROOM: gst::ClockTime = gst::ClockTime::from_mseconds(250);

fn unit_duration(asc: &[u8]) -> gst::ClockTime {
    const RATES: [u64; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let (Some(&a), Some(&b)) = (asc.first(), asc.get(1)) else {
        return gst::ClockTime::from_mseconds(40);
    };
    let index = (((a & 0x07) << 1) | (b >> 7)) as usize;
    let rate = RATES.get(index).copied().unwrap_or(48000);
    let samples = if b & 0x04 != 0 { 960 } else { 1024 };
    gst::ClockTime::from_nseconds(samples * 1_000_000_000 / rate)
}

#[derive(Default)]
struct VizAcc {
    samples: Vec<i16>,
    rate: u32,
}

pub struct Player {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
    volume: gst::Element,
    level: gst_controller::InterpolationControlSource,
    visualizer_enabled: Arc<AtomicBool>,
    visualizer: Arc<Mutex<VizAcc>>,
    codec: Codec,
    payload_type: u8,
    units: Mutex<UnitClock>,
    pcm_format: Option<(u32, u8)>,
    media_buffer: gst::ClockTime,
}

impl Player {
    pub fn new(cfg: &Config) -> Option<Self> {
        Self::from_desc(cfg, &pipeline_desc(cfg))
    }

    fn from_desc(cfg: &Config, desc: &str) -> Option<Self> {
        ensure_init();

        let pipeline = match gst::parse::launch(desc) {
            Ok(p) => p.downcast::<gst::Pipeline>().ok()?,
            Err(e) => {
                eprintln!("[cp_audio:{}] pipeline failed: {e}", cfg.label);
                return None;
            }
        };

        if let Some(device) = &cfg.device {
            let (_, device_property) = audio_sink();
            let sink = pipeline.by_name(SINK_NAME)?;
            sink.set_property(device_property, device.as_str());
        }

        let appsrc = pipeline
            .by_name("src")?
            .downcast::<gst_app::AppSrc>()
            .ok()?;
        let caps = match cfg.codec {
            Codec::AacRaw => aac_raw_caps(&cfg.codec_data, cfg.clock_rate, cfg.channels),
            Codec::Alac => alac_caps(cfg.clock_rate, cfg.channels, 16),
            Codec::Alac24 => alac_caps(cfg.clock_rate, cfg.channels, 24),
            _ => rtp_caps(cfg.codec, cfg.payload_type, cfg.clock_rate, cfg.channels),
        };
        let caps = gst::Caps::from_str(&caps).ok()?;
        appsrc.set_caps(Some(&caps));

        let volume = pipeline.by_name("vol")?;
        let level = gst_controller::InterpolationControlSource::new();
        level.set_mode(gst_controller::InterpolationMode::Linear);
        let binding = gst_controller::DirectControlBinding::new_absolute(&volume, "volume", &level);
        volume.add_control_binding(&binding).ok()?;
        volume.set_control_binding_disabled("volume", true);

        let visualizer_enabled = Arc::new(AtomicBool::new(false));
        let visualizer: Arc<Mutex<VizAcc>> = Default::default();
        if let Some(sink_pad) = volume.static_pad("sink") {
            let enabled = visualizer_enabled.clone();
            let acc = visualizer.clone();
            sink_pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                if enabled.load(Ordering::Relaxed)
                    && let Some(buffer) = info.buffer()
                    && let Some(caps) = pad.current_caps()
                    && let Ok(ainfo) = gst_audio::AudioInfo::from_caps(&caps)
                    && let Ok(map) = buffer.map_readable()
                {
                    let ch = ainfo.channels().max(1) as usize;
                    let mono: Vec<i16> = match ainfo.format() {
                        gst_audio::AudioFormat::S16le => map
                            .as_slice()
                            .as_chunks::<2>()
                            .0
                            .chunks(ch)
                            .map(|f| {
                                (f.iter().map(|b| i16::from_le_bytes(*b) as i32).sum::<i32>()
                                    / ch as i32) as i16
                            })
                            .collect(),
                        gst_audio::AudioFormat::F32le => map
                            .as_slice()
                            .as_chunks::<4>()
                            .0
                            .chunks(ch)
                            .map(|f| {
                                let avg = f.iter().map(|b| f32::from_le_bytes(*b)).sum::<f32>()
                                    / ch as f32;
                                (avg.clamp(-1.0, 1.0) * 32767.0) as i16
                            })
                            .collect(),
                        _ => return gst::PadProbeReturn::Ok,
                    };
                    if let Ok(mut a) = acc.lock() {
                        a.rate = ainfo.rate();
                        a.samples.extend_from_slice(&mono);
                        let cap = a.rate as usize / 5;
                        if cap > 0 && a.samples.len() > cap {
                            let overflow = a.samples.len() - cap;
                            a.samples.drain(..overflow);
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            });
        }

        if let Some(bus) = pipeline.bus() {
            let label = cfg.label.clone();
            bus.set_sync_handler(move |_, msg| {
                if let gst::MessageView::Error(e) = msg.view() {
                    eprintln!(
                        "[cp_audio:{label}] {} | {}",
                        e.error(),
                        e.debug().unwrap_or_default()
                    );
                }
                gst::BusSyncReply::Drop
            });
        }

        Some(Self {
            pipeline,
            appsrc,
            volume,
            level,
            visualizer_enabled,
            visualizer,
            codec: cfg.codec,
            payload_type: cfg.payload_type,
            pcm_format: cfg.buffered_pcm().then_some((cfg.clock_rate, cfg.channels)),
            media_buffer: gst::ClockTime::from_mseconds(u64::from(cfg.media_buffer_ms())),
            units: Mutex::new(UnitClock {
                next: None,
                frame: unit_duration(&cfg.codec_data),
            }),
        })
    }

    pub fn set_visualizer_enabled(&self, on: bool) {
        self.visualizer_enabled.store(on, Ordering::Relaxed);
        if !on && let Ok(mut a) = self.visualizer.lock() {
            a.samples.clear();
        }
    }

    pub fn take_visualizer(&self) -> Option<(Vec<u8>, u32)> {
        let mut a = self.visualizer.lock().ok()?;
        if a.samples.is_empty() {
            return None;
        }
        let bytes = core::mem::take(&mut a.samples)
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        Some((bytes, a.rate))
    }

    pub fn start(&self) {
        let _ = self.pipeline.set_state(gst::State::Playing);
    }

    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    pub fn push_rtp(&self, rtp: &[u8]) -> bool {
        let packet = match self.codec {
            Codec::AacLc | Codec::AacEld => reframe_aac(rtp, self.payload_type),
            Codec::Opus | Codec::AacRaw => rtp.to_vec(),
            Codec::Pcm | Codec::PcmLe | Codec::Alac | Codec::Alac24 => {
                rtp[RTP_HEADER_LEN.min(rtp.len())..].to_vec()
            }
        };
        self.push_buffer(gst::Buffer::from_slice(packet))
    }

    pub fn push_samples(&self, samples: &[u8]) -> bool {
        self.push_buffer(gst::Buffer::from_slice(samples.to_vec()))
    }

    fn push_buffer(&self, mut buffer: gst::Buffer) -> bool {
        let bytes = buffer.size() as u64;
        if (self.codec == Codec::AacRaw || self.pcm_format.is_some())
            && let Ok(mut clock) = self.units.lock()
            && let Some(b) = buffer.get_mut()
        {
            let now = self
                .pipeline
                .current_running_time()
                .unwrap_or(gst::ClockTime::ZERO);
            let (duration, headroom) = match self.pcm_format {
                Some((rate, channels)) if rate > 0 && channels > 0 => (
                    gst::ClockTime::from_nseconds(
                        bytes / (2 * u64::from(channels)) * 1_000_000_000 / u64::from(rate),
                    ),
                    self.media_buffer,
                ),
                _ => (clock.frame, UNIT_HEADROOM),
            };
            // Schedule PCM by sample count, rather than arrival time: a short
            // network stall must consume headroom instead of becoming an audible gap.
            let pts = clock.schedule(now, headroom, duration);
            b.set_pts(pts);
            b.set_duration(duration);
        }
        self.appsrc.push_buffer(buffer).is_ok()
    }

    pub fn set_volume(&self, level: f64, ms: u64) {
        let target = level.clamp(0.0, 10.0);
        let now = self.pipeline.current_running_time();

        if ms == 0 || now.is_none() {
            self.volume.set_control_binding_disabled("volume", true);
            self.volume.set_property("volume", target);
            return;
        }

        let now = now.unwrap();
        let from = self.volume.property::<f64>("volume");
        self.level.unset_all();
        self.level.set(now, from);
        self.level
            .set(now + gst::ClockTime::from_mseconds(ms), target);
        self.volume.set_control_binding_disabled("volume", false);
    }

    pub fn output_delay(&self) -> Option<std::time::Duration> {
        let sink = self.pipeline.by_name(SINK_NAME)?;
        if !sink.is::<gst_audio::AudioBaseSink>() {
            return None;
        }
        let queued = self
            .pipeline
            .by_name("out")
            .map_or(0, |q| q.property::<u64>("current-level-time"));
        // SAFETY: the element is a GstAudioBaseSink, its ring buffer lives as long as the sink.
        let ring_ns = unsafe {
            let base = sink.as_ptr() as *mut gst_audio::ffi::GstAudioBaseSink;
            let ring = (*base).ringbuffer;
            let next = (*base).next_sample;
            if ring.is_null()
                || next == u64::MAX
                || gst_audio::ffi::gst_audio_ring_buffer_is_acquired(ring) == 0
            {
                return None;
            }
            let rate = u64::try_from((*ring).spec.info.rate)
                .ok()
                .filter(|r| *r > 0)?;
            next.checked_sub(gst_audio::ffi::gst_audio_ring_buffer_samples_done(ring))?
                * 1_000_000_000
                / rate
        };
        Some(std::time::Duration::from_nanos(queued + ring_ns))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

pub fn ensure_init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = gst::init();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(codec: Codec, realtime: bool) -> Config {
        Config {
            codec,
            payload_type: 96,
            clock_rate: 44100,
            channels: 2,
            latency_ms: 1000,
            media_buffer_ms: 0,
            realtime,
            device: None,
            label: "music".into(),
            codec_data: Vec::new(),
        }
    }

    fn elements(desc: &str) -> Vec<String> {
        desc.split('!')
            .map(|p| p.split_whitespace().next().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn the_aac_chain_runs_from_appsrc_to_the_sink() {
        let desc = pipeline_desc(&cfg(Codec::AacLc, false));

        assert_eq!(
            elements(&desc),
            vec![
                "appsrc",
                "rtpjitterbuffer",
                "rtpmp4gdepay",
                "aacparse",
                if cfg!(target_os = "linux") {
                    "faad"
                } else {
                    "avdec_aac"
                },
                "audioconvert",
                "clocksync",
                "volume",
                "audioconvert",
                "audioresample",
                "audio/x-raw,format=S16LE,rate=48000,channels=2",
                "queue",
                audio_sink().0,
            ]
        );
    }

    #[test]
    fn the_sink_is_fed_the_same_way_on_both_systems() {
        let buffered = sink_chain(&cfg(Codec::AacLc, false));
        assert!(buffered.contains("sync=false"), "{buffered}");
        assert!(buffered.contains("buffer-time=300000"), "{buffered}");
        assert!(buffered.contains("latency-time=30000"), "{buffered}");
        let realtime = sink_chain(&cfg(Codec::Opus, true));
        assert!(realtime.contains("sync=false"), "{realtime}");
        assert!(!realtime.contains("buffer-time"), "{realtime}");
    }

    #[test]
    fn opus_depays_and_decodes_as_opus() {
        let desc = pipeline_desc(&cfg(Codec::Opus, false));
        let e = elements(&desc);

        assert!(e.contains(&"rtpopusdepay".to_string()));
        assert!(e.contains(&"opusdec".to_string()));
    }

    #[test]
    fn lpcm_goes_straight_from_the_source_to_the_converter() {
        let desc = pipeline_desc(&cfg(Codec::Pcm, false));
        let e = elements(&desc);

        assert_eq!(e[0], "appsrc");
        assert_eq!(e[1], "audioconvert");
        assert!(!desc.contains("rtpjitterbuffer"));
        assert!(!desc.contains("depay"));
    }

    #[test]
    fn the_jitter_buffer_takes_the_negotiated_depth() {
        let desc = pipeline_desc(&cfg(Codec::AacLc, false));

        assert!(desc.contains("rtpjitterbuffer latency=1000"));
    }

    #[test]
    fn a_depth_below_the_floor_is_raised() {
        let mut c = cfg(Codec::Opus, false);
        c.latency_ms = 10;

        assert!(pipeline_desc(&c).contains("rtpjitterbuffer latency=100"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_music_sink_gets_the_ring_and_period_a_realtime_one_does_not() {
        assert!(pipeline_desc(&cfg(Codec::AacLc, false)).contains("buffer-time=300000"));
        assert!(!pipeline_desc(&cfg(Codec::AacLc, true)).contains("buffer-time"));
    }

    #[test]
    fn the_desc_never_carries_the_device_string() {
        let mut c = cfg(Codec::Opus, false);
        c.device =
            Some("AppleUSBAudioEngine:Unknown Manufacturer:USB PnP Audio Device:131200:1".into());

        let desc = pipeline_desc(&c);
        assert!(desc.contains(&format!("name={SINK_NAME}")), "desc = {desc}");
        assert!(!desc.contains("unique-id="), "desc = {desc}");
        assert!(!desc.contains(" device="), "desc = {desc}");
        assert!(
            !desc.contains("USB PnP"),
            "device string leaked into desc: {desc}"
        );
    }

    #[test]
    fn every_link_carries_a_space_on_both_sides() {
        let desc = pipeline_desc(&cfg(Codec::AacLc, false));

        assert_eq!(desc.matches(" ! ").count(), desc.matches('!').count());
    }

    #[test]
    fn dab_units_go_through_faad_with_their_own_timestamps() {
        let desc = pipeline_desc(&cfg(Codec::AacRaw, false));
        assert!(desc.contains("do-timestamp=false"));
        assert_eq!(&elements(&desc)[..3], ["appsrc", "aacparse", "faad"]);
        assert!(pipeline_desc(&cfg(Codec::Opus, false)).contains("do-timestamp=true"));
    }

    #[test]
    fn unit_duration_follows_the_core_rate_and_frame_length() {
        assert_eq!(
            unit_duration(&[0x13, 0x14, 0x56, 0xe5, 0x98]).mseconds(),
            40
        );
        assert_eq!(unit_duration(&[0x11, 0x94]).mseconds(), 20);
        assert_eq!(unit_duration(&[]).mseconds(), 40);
    }

    #[test]
    fn media_jitter_buffer_adds_headroom_but_voice_keeps_its_existing_latency() {
        for codec in [Codec::AacLc, Codec::AacEld, Codec::Opus] {
            let mut config = cfg(codec, false);
            config.media_buffer_ms = 350;
            assert!(pipeline_desc(&config).contains("rtpjitterbuffer latency=1350"));
            config.realtime = true;
            assert!(pipeline_desc(&config).contains("rtpjitterbuffer latency=1000"));
        }
        let mut config = cfg(Codec::PcmLe, false);
        config.media_buffer_ms = 2000;
        assert!(config.buffered_pcm());
        assert!(pipeline_desc(&config).contains("do-timestamp=false"));
        config.realtime = true;
        assert!(!config.buffered_pcm());
        assert!(pipeline_desc(&config).contains("do-timestamp=true"));
    }

    #[test]
    fn pcm_timeline_absorbs_a_stall_and_rebuffers_only_after_headroom_is_exhausted() {
        let ms = gst::ClockTime::from_mseconds;
        let mut clock = UnitClock {
            next: None,
            frame: ms(10),
        };
        assert_eq!(clock.schedule(ms(0), ms(300), ms(10)), ms(300));
        assert_eq!(clock.schedule(ms(10), ms(300), ms(10)), ms(310));
        // The packets arrive in a burst after a 200 ms stall, but play 10 ms apart.
        assert_eq!(clock.schedule(ms(210), ms(300), ms(10)), ms(320));
        assert_eq!(clock.schedule(ms(210), ms(300), ms(10)), ms(330));
        assert_eq!(clock.schedule(ms(210), ms(300), ms(10)), ms(340));
        // An outage beyond the reserve starts a fresh buffered timeline.
        assert_eq!(clock.schedule(ms(1000), ms(300), ms(10)), ms(1300));
        assert_eq!(clock.schedule(ms(1000), ms(300), ms(10)), ms(1310));
    }

    #[test]
    fn pcm_samples_reach_a_fake_sink_with_contiguous_buffered_timestamps() {
        // No hardware audio device: run the actual producer/decoder pipeline with
        // clock waiting disabled so the timestamps can be checked deterministically.
        for codec in [Codec::Pcm, Codec::PcmLe] {
            let mut config = cfg(codec, false);
            config.clock_rate = 48000;
            config.media_buffer_ms = 500;
            let desc = pipeline_desc(&config)
                .replace(&sink_chain(&config), "fakesink name=audio-sink sync=false")
                .replace("clocksync sync=true", "clocksync sync=false");
            let player = Player::from_desc(&config, &desc).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            player
                .pipeline
                .by_name(SINK_NAME)
                .unwrap()
                .static_pad("sink")
                .unwrap()
                .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                    if let Some(buffer) = info.buffer() {
                        let _ = tx.send((buffer.pts().unwrap(), buffer.duration().unwrap()));
                    }
                    gst::PadProbeReturn::Ok
                });
            player.start();
            for _ in 0..2 {
                if codec == Codec::Pcm {
                    assert!(player.push_rtp(&[0; RTP_HEADER_LEN + 1920]));
                } else {
                    assert!(player.push_samples(&[0; 1920]));
                }
            }
            let first = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            let second = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            assert!(first.0 >= gst::ClockTime::from_mseconds(500));
            assert_eq!(first.1, gst::ClockTime::from_mseconds(10));
            assert_eq!(second.0 - first.0, first.1);
            assert_eq!(second.1, first.1);
        }
    }
}
