use super::{DeviceTrait, HostTrait, env};

pub(in crate::player::backend::ffmpeg::audio) struct AudioDeviceCandidate {
    pub(in crate::player::backend::ffmpeg::audio) source: &'static str,
    pub(in crate::player::backend::ffmpeg::audio) name: String,
    pub(in crate::player::backend::ffmpeg::audio) device: cpal::Device,
}

impl AudioDeviceCandidate {
    fn new(source: &'static str, name: String, device: cpal::Device) -> Self {
        Self {
            source,
            name,
            device,
        }
    }
}

pub(in crate::player::backend::ffmpeg::audio) fn output_device_candidates(
    host: &cpal::Host,
) -> std::result::Result<Vec<AudioDeviceCandidate>, String> {
    let mut devices = match host.output_devices() {
        Ok(devices) => devices
            .map(|device| {
                let name = device_name(&device);
                (name, device)
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            tracing::warn!(%error, "failed to enumerate native audio output devices");
            Vec::new()
        }
    };
    tracing::debug!(
        available_output_devices = ?devices.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        "available native audio output devices"
    );

    let mut candidates = Vec::new();
    if let Ok(requested) = env::var("TINY_AUDIO_DEVICE") {
        let requested = requested.trim();
        if !requested.is_empty() {
            let requested_lower = requested.to_lowercase();
            if let Some((name, device)) = take_output_device(&mut devices, |name| {
                name.to_lowercase().contains(&requested_lower)
            }) {
                tracing::debug!(
                    requested_device = requested,
                    selected_device = %name,
                    "selected requested native audio output device"
                );
                candidates.push(AudioDeviceCandidate::new("requested", name, device));
            } else {
                tracing::warn!(
                    requested_device = requested,
                    "requested native audio output device was not found"
                );
            }
        }
    }

    if let Some((name, device)) = take_output_device(&mut devices, preferred_audio_service_device) {
        tracing::debug!(
            selected_device = %name,
            "selected preferred native audio service device"
        );
        candidates.push(AudioDeviceCandidate::new("preferred", name, device));
    }

    if let Some(device) = host.default_output_device() {
        let name = device_name(&device);
        devices.retain(|(device_name, _)| device_name != &name);
        if !candidates.iter().any(|candidate| candidate.name == name) {
            tracing::debug!(
                default_device = %name,
                "selected default native audio output device"
            );
            candidates.push(AudioDeviceCandidate::new("default", name, device));
        }
    }

    let (mut normal_devices, null_devices): (Vec<_>, Vec<_>) = devices
        .into_iter()
        .partition(|(name, _)| !null_audio_device(name));
    candidates.extend(
        normal_devices
            .drain(..)
            .map(|(name, device)| AudioDeviceCandidate::new("enumerated", name, device)),
    );
    candidates.extend(
        null_devices
            .into_iter()
            .map(|(name, device)| AudioDeviceCandidate::new("null-fallback", name, device)),
    );

    if candidates.is_empty() {
        return Err("未找到系统音频输出设备".to_string());
    }
    Ok(candidates)
}

fn take_output_device<P>(
    devices: &mut Vec<(String, cpal::Device)>,
    predicate: P,
) -> Option<(String, cpal::Device)>
where
    P: Fn(&str) -> bool,
{
    let index = devices.iter().position(|(name, _)| predicate(name))?;
    Some(devices.remove(index))
}

fn preferred_audio_service_device(name: &str) -> bool {
    let name = name.to_lowercase();
    name.contains("pipewire") || name.contains("pulse")
}

fn null_audio_device(name: &str) -> bool {
    let name = name.to_lowercase();
    name == "null" || name.contains("discard")
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|error| format!("<读取设备名称失败：{error}>"))
}
