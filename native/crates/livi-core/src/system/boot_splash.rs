// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::io::Cursor;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::{DynamicImage, ImageFormat, ImageReader, RgbaImage, imageops};
use livi_core_proto::config::Config;
use livi_core_proto::state::BootSplash;
use tokio::process::Command;
use tokio::sync::oneshot;

use super::privileged::Privileged;
use crate::server::Core;

const DEFAULT: &[u8] = include_bytes!("../../../../../scripts/install/pi/splash/livi-splash.png");
const INSTALLED_THEME: &str = "/usr/share/plymouth/themes/livi/livi.plymouth";
const RETRY: Duration = Duration::from_secs(30);
const REBUILD_TIMEOUT: Duration = Duration::from_secs(600);

fn selected(config: &Config) -> Option<&str> {
    [
        &config.carplay_icon256,
        &config.carplay_icon180,
        &config.carplay_icon120,
    ]
    .into_iter()
    .filter_map(|v| v.as_deref())
    .map(str::trim)
    .find(|v| !v.is_empty())
}

fn render(icon: Option<&str>) -> Result<Vec<u8>, String> {
    let Some(icon) = icon else {
        return Ok(DEFAULT.to_vec());
    };
    if icon.len() > 4 * 1024 * 1024 {
        return Err("The UI icon is too large for the boot splash".into());
    }
    let bytes = STANDARD
        .decode(icon)
        .map_err(|e| format!("Invalid UI icon: {e}"))?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|e| format!("Invalid UI icon PNG: {e}"))?;
    // The original LIVI logo occupies the middle ~256 px of a transparent 512 px
    // canvas. Keep that padding, scale and black Plymouth background for all icons.
    let icon = image
        .resize(256, 256, imageops::FilterType::Lanczos3)
        .to_rgba8();
    let mut canvas = RgbaImage::new(512, 512);
    imageops::overlay(
        &mut canvas,
        &icon,
        i64::from((512 - icon.width()) / 2),
        i64::from((512 - icon.height()) / 2),
    );
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(canvas)
        .write_to(&mut png, ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(png.into_inner())
}

fn publish(core: &Core, status: Option<BootSplash>) {
    core.hub.update(|s| s.system.boot_splash = status);
}

async fn apply(privileged: &Privileged, png: &[u8]) -> Result<bool, String> {
    let logo = privileged.user_data.join("splash/logo.png");
    livi_base::fs::write_atomic(&logo, png, 0o644).map_err(|e| e.to_string())?;
    // Leave an in-flight helper alive if the app is forcibly closed: rebuilding
    // the boot image should be allowed to finish. Normal shutdown waits below.
    let update = Command::new("sudo")
        .arg("-n")
        .arg(&privileged.helper)
        .arg("--update-boot-splash")
        .arg(&logo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let output = tokio::time::timeout(REBUILD_TIMEOUT, update)
        .await
        .map_err(|_| "Boot splash update timed out; it will be retried".to_string())?
        .map_err(|e| e.to_string())?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(3) => Ok(false),
        _ => {
            eprintln!(
                "[splash] helper: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Err("Boot splash update failed; it will be retried".into())
        }
    }
}

pub struct Watcher {
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Watcher {
    pub async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

pub fn start(core: Arc<Core>, privileged: Privileged) -> Watcher {
    let (stop, mut stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut state = core.hub.watch();
        let mut applied: Option<Option<String>> = None;
        let mut attempted: Option<Option<String>> = None;
        let mut retry = tokio::time::interval(RETRY);
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        retry.tick().await;
        loop {
            let icon = {
                let state = state.borrow_and_update();
                selected(&state.config).map(str::to_string)
            };
            if !Path::new(INSTALLED_THEME).is_file() {
                publish(&core, None);
                applied = None;
            } else if applied.as_ref() != Some(&icon) && attempted.as_ref() != Some(&icon) {
                attempted = Some(icon.clone());
                publish(
                    &core,
                    Some(BootSplash {
                        updating: true,
                        error: None,
                    }),
                );
                let input = icon.clone();
                let png = tokio::task::spawn_blocking(move || render(input.as_deref()))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|v| v);
                let result = match png {
                    Ok(png) => apply(&privileged, &png).await,
                    Err(e) => Err(e),
                };
                match result {
                    Ok(true) => {
                        let current = core.hub.config();
                        let up_to_date = selected(&current) == icon.as_deref();
                        applied = Some(icon);
                        println!(
                            "[splash] UI icon synchronized; the next boot uses the updated splash"
                        );
                        publish(
                            &core,
                            Some(BootSplash {
                                updating: !up_to_date,
                                error: None,
                            }),
                        );
                    }
                    Ok(false) => publish(&core, None),
                    Err(error) => {
                        eprintln!("[splash] {error}");
                        publish(
                            &core,
                            Some(BootSplash {
                                updating: false,
                                error: Some(error),
                            }),
                        );
                    }
                }
            }
            tokio::select! {
                _ = &mut stopped => return,
                change = state.changed() => { if change.is_err() { return; } },
                _ = retry.tick() => attempted = None,
            }
        }
    });
    Watcher { stop, task }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn encoded(image: RgbaImage) -> String {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        STANDARD.encode(bytes.into_inner())
    }

    #[test]
    fn custom_icons_keep_the_original_canvas_padding_and_transparency() {
        let icon = encoded(RgbaImage::from_pixel(256, 256, Rgba([20, 120, 200, 255])));
        let png = render(Some(&icon)).unwrap();
        let result = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(result.dimensions(), (512, 512));
        assert_eq!(result.get_pixel(0, 0)[3], 0);
        assert_eq!(result.get_pixel(127, 256)[3], 0);
        assert_eq!(*result.get_pixel(128, 128), Rgba([20, 120, 200, 255]));
        assert_eq!(*result.get_pixel(383, 383), Rgba([20, 120, 200, 255]));
        assert_eq!(result.get_pixel(384, 256)[3], 0);
    }

    #[test]
    fn rectangular_icons_fit_without_stretching_or_cropping() {
        let icon = encoded(RgbaImage::from_pixel(256, 128, Rgba([255, 255, 255, 255])));
        let result = image::load_from_memory(&render(Some(&icon)).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(result.get_pixel(256, 191)[3], 0);
        assert_eq!(result.get_pixel(256, 192)[3], 255);
        assert_eq!(result.get_pixel(256, 319)[3], 255);
        assert_eq!(result.get_pixel(256, 320)[3], 0);
    }

    #[test]
    fn reset_restores_the_original_asset_and_largest_icon_is_preferred() {
        assert_eq!(render(None).unwrap(), DEFAULT);
        let mut config = livi_core_proto::config::defaults();
        assert_eq!(selected(&config), None);
        config.carplay_icon120 = Some("small".into());
        config.carplay_icon256 = Some(" large ".into());
        assert_eq!(selected(&config), Some("large"));
        config.carplay_icon256 = None;
        assert_eq!(selected(&config), Some("small"));
        assert!(render(Some("invalid base64!")).is_err());
        assert!(render(Some(&STANDARD.encode(b"not PNG"))).is_err());
    }
}
