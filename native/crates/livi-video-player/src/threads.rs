// SPDX-FileCopyrightText: 2026 Ethan Rushbrook
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use gst::prelude::*;
use gstreamer as gst;

use crate::Say;

pub(crate) fn automatic(bin: &gst::Bin, say: &Say) {
    let added = say.clone();
    bin.connect_deep_element_added(move |_, _, element| configure_tree(element, &added));
    configure_tree(bin.upcast_ref(), say);
}

fn configure_tree(element: &gst::Element, say: &Say) {
    configure(element, say);
    // Some autoplugged bins already contain their converter when added to
    // playbin. Configure those children as well as elements added later.
    if let Some(bin) = element.downcast_ref::<gst::Bin>() {
        for child in bin.iterate_recurse().into_iter().flatten() {
            configure(&child, say);
        }
    }
}

fn configure(element: &gst::Element, say: &Say) {
    let Some(factory) = element.factory() else {
        return;
    };
    let name = factory.name();
    let property = match name.as_str() {
        "videoconvert" | "videoscale" | "videoconvertscale" => "n-threads",
        _ if factory
            .has_type(gst::ElementFactoryType::DECODER | gst::ElementFactoryType::MEDIA_VIDEO) =>
        {
            match name.as_str() {
                n if n.starts_with("avdec_") => "max-threads",
                "vp8dec" | "vp9dec" => "threads",
                "dav1ddec" => "n-threads",
                _ => return,
            }
        }
        _ => return,
    };
    let Some(spec) = element.find_property(property) else {
        return;
    };
    // These elements use zero to select the available CPU threads. Hardware
    // decoders retain their own configuration and remain preferred.
    if spec.value_type() == i32::static_type() {
        element.set_property(property, 0i32);
    } else if spec.value_type() == u32::static_type() {
        element.set_property(property, 0u32);
    } else {
        return;
    }
    say(&format!(
        "[gst_video] {name} {property}=0 (automatic CPU threads)"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer_app as gst_app;
    use std::sync::Arc;

    #[test]
    fn existing_and_later_nested_converters_use_automatic_threads() {
        crate::ensure_init();
        let pipeline = gst::Pipeline::new();
        let first = gst::ElementFactory::make("videoconvert").build().unwrap();
        pipeline.add(&first).unwrap();
        let say: Say = Arc::new(|_| {});
        automatic(pipeline.upcast_ref(), &say);
        assert_eq!(first.property::<u32>("n-threads"), 0);

        let nested = gst::Bin::new();
        let second = gst::ElementFactory::make("videoscale").build().unwrap();
        nested.add(&second).unwrap();
        pipeline.add(&nested).unwrap();
        assert_eq!(second.property::<u32>("n-threads"), 0);

        let later = gst::ElementFactory::make("videoconvert").build().unwrap();
        nested.add(&later).unwrap();
        assert_eq!(later.property::<u32>("n-threads"), 0);
    }

    #[test]
    fn playbin_converts_video_with_automatic_threads_without_a_sink_override() {
        crate::ensure_init();
        let player = gst::ElementFactory::make("playbin3")
            .property("uri", "appsrc://")
            .build()
            .unwrap();
        player.set_property_from_str("flags", "video");
        let sink = gst::parse::bin_from_description(
            "capsfilter caps=video/x-raw,format=I420 ! fakesink sync=false",
            true,
        )
        .unwrap();
        player.set_property("video-sink", sink);
        let say: Say = Arc::new(|_| {});
        automatic(player.downcast_ref::<gst::Bin>().unwrap(), &say);
        player.connect("source-setup", false, |args| {
            let source = args[1].get::<gst::Element>().unwrap();
            let source = source.downcast::<gst_app::AppSrc>().unwrap();
            source.set_caps(Some(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .field("width", 32i32)
                    .field("height", 32i32)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .build(),
            ));
            source.set_format(gst::Format::Time);
            let mut sent = false;
            source.set_callbacks(
                gst_app::AppSrcCallbacks::builder()
                    .need_data(move |source, _| {
                        if !sent {
                            sent = true;
                            let mut buffer = gst::Buffer::with_size(32 * 32 * 4).unwrap();
                            buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
                            source.push_buffer(buffer).unwrap();
                            source.end_of_stream().unwrap();
                        }
                    })
                    .build(),
            );
            None
        });
        player.set_state(gst::State::Playing).unwrap();
        let message = player.bus().unwrap().timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        let converters: Vec<_> = player
            .downcast_ref::<gst::Bin>()
            .unwrap()
            .iterate_recurse()
            .into_iter()
            .flatten()
            .filter_map(|element| {
                let factory = element.factory()?;
                matches!(
                    factory.name().as_str(),
                    "videoconvert" | "videoscale" | "videoconvertscale"
                )
                .then(|| {
                    (
                        factory.name().to_string(),
                        element.property::<u32>("n-threads"),
                    )
                })
            })
            .collect();
        player.set_state(gst::State::Null).unwrap();
        let message = message.expect("video conversion should finish");
        if let gst::MessageView::Error(error) = message.view() {
            panic!("{}: {:?}", error.error(), error.debug());
        }
        assert!(
            !converters.is_empty(),
            "playbin should autoplug a parallel converter"
        );
        assert!(
            converters.iter().all(|(_, threads)| *threads == 0),
            "{converters:?}"
        );
    }
}
