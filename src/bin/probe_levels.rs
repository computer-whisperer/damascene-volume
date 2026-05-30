use pipewire as pw;
use pw::{properties::properties, spa};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;
use std::{env, mem};

const DEFAULT_BUFFERS: u32 = 60;

struct MeterData {
    format: spa::param::audio::AudioInfoRaw,
    mainloop: pw::main_loop::MainLoopRc,
    buffers_seen: u32,
    buffers_limit: u32,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    pw::init();

    let target = env::args().nth(1).and_then(|arg| arg.parse::<u32>().ok());
    let buffers_limit = env::var("DAMASCENE_LEVEL_BUFFERS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(DEFAULT_BUFFERS);
    let capture_sink = env::var("DAMASCENE_LEVEL_CAPTURE_SINK")
        .ok()
        .map(|value| value != "0" && value != "false")
        .unwrap_or(true);

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let mut props = properties! {
        *pw::keys::APP_NAME => "damascene-volume-level-probe",
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => "damascene-volume.level-probe",
    };
    if capture_sink {
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }
    if let Some(target) = target {
        props.insert("target.object", target.to_string());
    }

    let stream = pw::stream::StreamBox::new(&core, "damascene-volume-level-probe", props)?;
    let data = MeterData {
        format: Default::default(),
        mainloop: mainloop.clone(),
        buffers_seen: 0,
        buffers_limit,
    };

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, data, _, state| {
            if let pw::stream::StreamState::Error(err) = state {
                eprintln!("stream error: {err}");
                data.mainloop.quit();
            }
        })
        .param_changed(|_, data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            if data.format.parse(param).is_ok() {
                println!(
                    "format rate:{} channels:{}",
                    data.format.rate(),
                    data.format.channels()
                );
            }
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let chunk = datas[0].chunk();
            let bytes = chunk.size() as usize;
            let channels = data.format.channels() as usize;
            if channels == 0 || bytes == 0 {
                return;
            }
            let Some(samples) = datas[0].data() else {
                return;
            };
            let sample_count = (bytes / mem::size_of::<f32>()).min(samples.len() / 4);
            if sample_count == 0 {
                return;
            }

            let mut peaks = vec![0.0_f32; channels];
            let mut sums = vec![0.0_f32; channels];
            let mut counts = vec![0_u32; channels];
            for sample_index in 0..sample_count {
                let start = sample_index * 4;
                let value = f32::from_le_bytes(samples[start..start + 4].try_into().unwrap());
                let channel = sample_index % channels;
                let abs = value.abs();
                peaks[channel] = peaks[channel].max(abs);
                sums[channel] += value * value;
                counts[channel] += 1;
            }

            data.buffers_seen += 1;
            print!("buffer {:03}:", data.buffers_seen);
            for channel in 0..channels {
                let rms = if counts[channel] == 0 {
                    0.0
                } else {
                    (sums[channel] / counts[channel] as f32).sqrt()
                };
                print!(" ch{channel} peak:{:.3} rms:{:.3}", peaks[channel], rms);
            }
            println!();

            if data.buffers_seen >= data.buffers_limit {
                data.mainloop.quit();
            }
        })
        .register()?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or("failed to build PipeWire format pod")?];

    stream.connect(
        spa::utils::Direction::Input,
        target,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    mainloop.run();
    Ok(())
}
