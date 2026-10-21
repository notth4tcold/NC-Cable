//! One virtual cable = two PipeWire nodes sharing a ring buffer.
//!
//! VB-CABLE exposes a render endpoint ("CABLE Input") and a capture endpoint
//! ("CABLE Output") from one kernel driver. On Linux the audio server owns the
//! devices, so the same pair is published as an `Audio/Sink` stream node and an
//! `Audio/Source` stream node. PipeWire does format and sample-rate conversion
//! for every client, as the driver does in its own copy routines.

use anyhow::Result;
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use pw::stream::{StreamFlags, StreamListener, StreamRc, StreamState};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::CableConfig;
use crate::ring::{self, Consumer, Producer, Ring};

pub struct Cable {
    pub cfg: CableConfig,
    pub ring: Arc<Ring>,
    pub input_connected: Arc<AtomicBool>,
    pub output_connected: Arc<AtomicBool>,
    _sink: StreamRc,
    _source: StreamRc,
    _sink_listener: StreamListener<Producer>,
    _source_listener: StreamListener<Consumer>,
}

fn channel_positions(channels: u32) -> Vec<u32> {
    use spa::sys::*;
    match channels {
        1 => vec![SPA_AUDIO_CHANNEL_MONO],
        2 => vec![SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR],
        4 => vec![
            SPA_AUDIO_CHANNEL_FL,
            SPA_AUDIO_CHANNEL_FR,
            SPA_AUDIO_CHANNEL_RL,
            SPA_AUDIO_CHANNEL_RR,
        ],
        6 => vec![
            SPA_AUDIO_CHANNEL_FL,
            SPA_AUDIO_CHANNEL_FR,
            SPA_AUDIO_CHANNEL_FC,
            SPA_AUDIO_CHANNEL_LFE,
            SPA_AUDIO_CHANNEL_RL,
            SPA_AUDIO_CHANNEL_RR,
        ],
        8 => vec![
            SPA_AUDIO_CHANNEL_FL,
            SPA_AUDIO_CHANNEL_FR,
            SPA_AUDIO_CHANNEL_FC,
            SPA_AUDIO_CHANNEL_LFE,
            SPA_AUDIO_CHANNEL_RL,
            SPA_AUDIO_CHANNEL_RR,
            SPA_AUDIO_CHANNEL_SL,
            SPA_AUDIO_CHANNEL_SR,
        ],
        n => (0..n).map(|i| SPA_AUDIO_CHANNEL_AUX0 + i).collect(),
    }
}

fn position_names(channels: u32) -> String {
    match channels {
        1 => "MONO".into(),
        2 => "FL,FR".into(),
        4 => "FL,FR,RL,RR".into(),
        6 => "FL,FR,FC,LFE,RL,RR".into(),
        8 => "FL,FR,FC,LFE,RL,RR,SL,SR".into(),
        n => (0..n)
            .map(|i| format!("AUX{i}"))
            .collect::<Vec<_>>()
            .join(","),
    }
}

fn format_pod(cfg: &CableConfig) -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(cfg.rate);
    info.set_channels(cfg.channels);
    let mut position = [0u32; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    for (slot, pos) in position.iter_mut().zip(channel_positions(cfg.channels)) {
        *slot = pos;
    }
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("serializing audio format")
    .0
    .into_inner()
}

fn node_props(cfg: &CableConfig, class: &str, suffix: &str) -> pw::properties::PropertiesBox {
    let slug = cfg.slug();
    let latency = format!("{}/{}", (cfg.latency / 3).max(32), cfg.rate);
    pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Duplex",
        *pw::keys::MEDIA_CLASS => class,
        *pw::keys::NODE_NAME => format!("{slug}_{}", suffix.to_lowercase()),
        *pw::keys::NODE_DESCRIPTION => format!("{} {suffix}", cfg.name),
        *pw::keys::NODE_NICK => format!("{} {suffix}", cfg.name),
        *pw::keys::NODE_LATENCY => latency,
        *pw::keys::AUDIO_CHANNELS => cfg.channels.to_string(),
        *pw::keys::AUDIO_RATE => cfg.rate.to_string(),
        "audio.position" => position_names(cfg.channels),
        // Keep both ends scheduled by the same driver even when nothing is linked,
        // so they share one clock and the ring never drifts.
        "node.want-driver" => "true",
        "node.pause-on-idle" => "false",
        "device.icon-name" => "audio-card",
        "nc-cable.name" => cfg.name.clone(),
    }
}

/// Reinterprets a mapped PipeWire buffer as f32 samples; F32LE buffers are always
/// 4-byte aligned in practice, anything else is treated as empty.
fn as_f32(bytes: &[u8]) -> &[f32] {
    // SAFETY: every bit pattern is a valid f32.
    let (head, samples, _) = unsafe { bytes.align_to::<f32>() };
    if head.is_empty() { samples } else { &[] }
}

fn as_f32_mut(bytes: &mut [u8]) -> &mut [f32] {
    // SAFETY: every bit pattern is a valid f32.
    let (head, samples, _) = unsafe { bytes.align_to_mut::<f32>() };
    if head.is_empty() { samples } else { &mut [] }
}

fn track_state(
    flag: &Arc<AtomicBool>,
    label: String,
) -> impl FnMut(&pw::stream::Stream, StreamState, StreamState) + use<> {
    let flag = flag.clone();
    move |_, _old, new| {
        flag.store(matches!(new, StreamState::Streaming), Ordering::Relaxed);
        if let StreamState::Error(e) = new {
            eprintln!("{label}: stream error: {e}");
        }
    }
}

impl Cable {
    pub fn new(core: &pw::core::CoreRc, cfg: CableConfig) -> Result<Self> {
        let ring = Ring::new(
            cfg.channels as usize,
            cfg.max_latency as usize + 8,
            cfg.latency as usize,
        );
        let (producer, consumer) = ring::split(&ring);
        let input_connected = Arc::new(AtomicBool::new(false));
        let output_connected = Arc::new(AtomicBool::new(false));
        let stride = cfg.channels as usize * size_of::<f32>();

        let sink = StreamRc::new(
            core.clone(),
            &format!("{} Input", cfg.name),
            node_props(&cfg, "Audio/Sink", "Input"),
        )?;
        let mut on_state = track_state(&input_connected, format!("{} Input", cfg.name));
        let sink_listener = sink
            .add_local_listener_with_user_data(producer)
            .state_changed(move |s, _, old, new| on_state(s, old, new))
            .process(|stream, producer| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                let offset = data.chunk().offset() as usize;
                let size = data.chunk().size() as usize;
                if let Some(bytes) = data.data() {
                    let end = (offset + size).min(bytes.len());
                    producer.push(as_f32(&bytes[offset.min(end)..end]));
                }
            })
            .register()?;

        let source = StreamRc::new(
            core.clone(),
            &format!("{} Output", cfg.name),
            node_props(&cfg, "Audio/Source", "Output"),
        )?;
        let mut on_state = track_state(&output_connected, format!("{} Output", cfg.name));
        let source_listener = source
            .add_local_listener_with_user_data(consumer)
            .state_changed(move |s, _, old, new| on_state(s, old, new))
            .process(move |stream, consumer| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let requested = buffer.requested() as usize;
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                let mut frames = 0;
                if let Some(bytes) = data.data() {
                    frames = bytes.len() / stride;
                    if requested > 0 {
                        frames = frames.min(requested);
                    }
                    consumer.pull(as_f32_mut(&mut bytes[..frames * stride]));
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as i32;
                *chunk.size_mut() = (frames * stride) as u32;
            })
            .register()?;

        let flags = StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS;
        let pod = format_pod(&cfg);
        sink.connect(
            spa::utils::Direction::Input,
            None,
            flags,
            &mut [Pod::from_bytes(&pod).unwrap()],
        )?;
        source.connect(
            spa::utils::Direction::Output,
            None,
            flags,
            &mut [Pod::from_bytes(&pod).unwrap()],
        )?;

        Ok(Cable {
            cfg,
            ring,
            input_connected,
            output_connected,
            _sink: sink,
            _source: source,
            _sink_listener: sink_listener,
            _source_listener: source_listener,
        })
    }
}
