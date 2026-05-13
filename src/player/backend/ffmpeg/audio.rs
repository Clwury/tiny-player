use super::*;

pub(super) struct AudioOutput {
    shared: Arc<AudioShared>,
    _stream: cpal::Stream,
    sample_rate: c_int,
    channels: c_int,
}

pub(super) struct AudioShared {
    pub(super) buffer: Mutex<AudioBuffer>,
    pub(super) ready: Condvar,
    pub(super) played_samples: AtomicU64,
    pub(super) control: Arc<FfmpegControl>,
}

pub(super) struct AudioBuffer {
    samples: Vec<f32>,
    read_pos: usize,
    write_pos: usize,
    len: usize,
}

impl AudioBuffer {
    pub(super) fn with_capacity(max_samples: usize) -> Self {
        Self {
            samples: vec![0.0; max_samples],
            read_pos: 0,
            write_pos: 0,
            len: 0,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(super) fn available_capacity(&self) -> usize {
        self.samples.len().saturating_sub(self.len)
    }

    pub(super) fn clear(&mut self) {
        self.read_pos = 0;
        self.write_pos = 0;
        self.len = 0;
    }

    pub(super) fn push_slice(&mut self, samples: &[f32]) -> usize {
        let writable = samples.len().min(self.available_capacity());
        if writable == 0 || self.samples.is_empty() {
            return 0;
        }

        let first = writable.min(self.samples.len() - self.write_pos);
        self.samples[self.write_pos..self.write_pos + first].copy_from_slice(&samples[..first]);
        self.write_pos = (self.write_pos + first) % self.samples.len();
        self.len += first;

        let remaining = writable - first;
        if remaining > 0 {
            self.samples[..remaining].copy_from_slice(&samples[first..first + remaining]);
            self.write_pos = remaining;
            self.len += remaining;
        }

        writable
    }

    pub(super) fn pop_sample(&mut self) -> Option<f32> {
        if self.len == 0 || self.samples.is_empty() {
            return None;
        }
        let sample = self.samples[self.read_pos];
        self.read_pos = (self.read_pos + 1) % self.samples.len();
        self.len -= 1;
        Some(sample)
    }
}

impl AudioOutput {
    pub(super) fn new(control: Arc<FfmpegControl>) -> std::result::Result<Self, String> {
        let host = cpal::default_host();
        let mut last_error = None;
        for candidate in output_device_candidates(&host)? {
            match Self::from_device(
                candidate.device,
                candidate.name.clone(),
                Arc::clone(&control),
            ) {
                Ok(output) => return Ok(output),
                Err(error) => {
                    tracing::warn!(
                        device = %candidate.name,
                        source = %candidate.source,
                        %error,
                        "native audio output device initialization failed"
                    );
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| "未找到系统音频输出设备".to_string()))
    }

    fn from_device(
        device: cpal::Device,
        device_name: String,
        control: Arc<FfmpegControl>,
    ) -> std::result::Result<Self, String> {
        let supported_config = device
            .default_output_config()
            .map_err(|error| format!("读取系统音频输出配置失败：{error}"))?;
        let sample_rate = c_int::try_from(supported_config.sample_rate())
            .map_err(|_| "系统音频采样率过大".to_string())?;
        let channels = c_int::from(supported_config.channels());
        let max_samples = usize::try_from(sample_rate)
            .ok()
            .and_then(|rate| rate.checked_mul(usize::try_from(channels).ok()?))
            .and_then(|samples| samples.checked_mul(AUDIO_BUFFER_SECONDS))
            .ok_or_else(|| "系统音频缓冲区过大".to_string())?;
        let shared = Arc::new(AudioShared {
            buffer: Mutex::new(AudioBuffer::with_capacity(max_samples)),
            ready: Condvar::new(),
            played_samples: AtomicU64::new(0),
            control,
        });
        let config: cpal::StreamConfig = supported_config.clone().into();
        let sample_format = supported_config.sample_format();
        tracing::debug!(
            device = %device_name,
            sample_rate,
            channels,
            ?sample_format,
            "selected native audio output config"
        );
        let stream = match sample_format {
            cpal::SampleFormat::I8 => {
                build_audio_output_stream::<i8>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::I16 => {
                build_audio_output_stream::<i16>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::I32 => {
                build_audio_output_stream::<i32>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::I64 => {
                build_audio_output_stream::<i64>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::U8 => {
                build_audio_output_stream::<u8>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::U16 => {
                build_audio_output_stream::<u16>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::U32 => {
                build_audio_output_stream::<u32>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::U64 => {
                build_audio_output_stream::<u64>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::F32 => {
                build_audio_output_stream::<f32>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::F64 => {
                build_audio_output_stream::<f64>(&device, &config, shared.clone())
            }
            sample_format => {
                return Err(format!("暂不支持的系统音频采样格式：{sample_format:?}"));
            }
        }
        .map_err(|error| format!("创建系统音频输出流失败：{error}"))?;
        stream
            .play()
            .map_err(|error| format!("启动系统音频输出流失败：{error}"))?;

        Ok(Self {
            shared,
            _stream: stream,
            sample_rate,
            channels,
        })
    }

    pub(super) fn sample_rate(&self) -> c_int {
        self.sample_rate
    }

    pub(super) fn channels(&self) -> c_int {
        self.channels
    }

    pub(super) fn push<F>(
        &self,
        samples: Vec<f32>,
        control: &FfmpegControl,
        mut on_wait: F,
    ) -> std::result::Result<(), String>
    where
        F: FnMut() -> std::result::Result<(), String>,
    {
        let mut offset = 0;
        while offset < samples.len() {
            if control.should_interrupt() {
                return Ok(());
            }
            let mut guard = self
                .shared
                .buffer
                .lock()
                .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
            while guard.available_capacity() == 0 && !control.should_interrupt() {
                let (next_guard, _) = self
                    .shared
                    .ready
                    .wait_timeout(guard, SCHEDULER_POLL_INTERVAL)
                    .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
                guard = next_guard;
                drop(guard);
                on_wait()?;
                guard = self
                    .shared
                    .buffer
                    .lock()
                    .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
            }
            if control.should_interrupt() {
                return Ok(());
            }
            let capacity = guard.available_capacity();
            if capacity == 0 {
                continue;
            }
            let end = (offset + capacity).min(samples.len());
            let written = guard.push_slice(&samples[offset..end]);
            offset += written;
            self.shared.ready.notify_all();
            drop(guard);
            on_wait()?;
        }
        Ok(())
    }

    pub(super) fn reset_clock(&self, timeline_nsecs: u64) {
        if let Ok(mut guard) = self.shared.buffer.lock() {
            guard.clear();
            self.shared.played_samples.store(0, Ordering::Relaxed);
            self.shared.ready.notify_all();
        }
        self.shared.played_samples.store(
            samples_for_duration(timeline_nsecs, self.sample_rate, self.channels),
            Ordering::Relaxed,
        );
    }

    pub(super) fn played_timeline_nsecs(&self) -> u64 {
        audio_samples_duration(
            usize::try_from(self.shared.played_samples.load(Ordering::Relaxed))
                .unwrap_or(usize::MAX),
            self.sample_rate,
            self.channels,
        )
        .as_nanos()
        .try_into()
        .unwrap_or(u64::MAX)
    }

    pub(super) fn wait_for_progress(
        &self,
        control: &FfmpegControl,
    ) -> std::result::Result<(), String> {
        let previous = self.shared.played_samples.load(Ordering::Relaxed);
        let mut guard = self
            .shared
            .buffer
            .lock()
            .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
        while self.shared.played_samples.load(Ordering::Relaxed) == previous
            && !guard.is_empty()
            && !control.should_interrupt()
        {
            let (next_guard, _) = self
                .shared
                .ready
                .wait_timeout(guard, SCHEDULER_POLL_INTERVAL)
                .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
            guard = next_guard;
        }
        Ok(())
    }

    pub(super) fn drain(&self, control: &FfmpegControl) -> std::result::Result<(), String> {
        let timeout = self
            .queued_duration()?
            .saturating_add(Duration::from_millis(250));
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return Ok(());
        };

        let mut guard = self
            .shared
            .buffer
            .lock()
            .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
        while !guard.is_empty() && !control.should_interrupt() {
            let now = Instant::now();
            if now >= deadline {
                tracing::debug!(
                    remaining_samples = guard.len(),
                    "timed out waiting for native audio output to drain"
                );
                break;
            }
            let wait_for = (deadline - now).min(SCHEDULER_POLL_INTERVAL);
            let (next_guard, _) = self
                .shared
                .ready
                .wait_timeout(guard, wait_for)
                .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
            guard = next_guard;
        }
        Ok(())
    }

    pub(super) fn queued_duration(&self) -> std::result::Result<Duration, String> {
        let queued_samples = self
            .shared
            .buffer
            .lock()
            .map_err(|_| "系统音频缓冲区已损坏".to_string())?
            .len();
        Ok(audio_samples_duration(
            queued_samples,
            self.sample_rate,
            self.channels,
        ))
    }
}

struct AudioDeviceCandidate {
    source: &'static str,
    name: String,
    device: cpal::Device,
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

fn output_device_candidates(
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

fn build_audio_output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<AudioShared>,
) -> std::result::Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let error_callback = |error| tracing::warn!(%error, "native audio output stream error");
    device.build_output_stream(
        config,
        move |data: &mut [T], _| fill_audio_output(data, &shared),
        error_callback,
        None,
    )
}

pub(super) fn fill_audio_output<T>(data: &mut [T], shared: &AudioShared)
where
    T: Sample + FromSample<f32>,
{
    let mut guard = shared.buffer.lock().expect("audio output buffer poisoned");
    if shared.control.is_paused() {
        for sample in data {
            *sample = T::from_sample(0.0);
        }
        shared.ready.notify_all();
        return;
    }

    let mut played = 0u64;
    for sample in data {
        let value = match guard.pop_sample() {
            Some(value) => {
                played = played.saturating_add(1);
                value
            }
            None => 0.0,
        }
        .clamp(-1.0, 1.0);
        *sample = T::from_sample(value);
    }
    drop(guard);

    if played > 0 {
        shared.played_samples.fetch_add(played, Ordering::Relaxed);
    }
    shared.ready.notify_all();
}

pub(super) fn frame_sample_format(
    frame: *mut ffi::AVFrame,
) -> std::result::Result<ffi::AVSampleFormat, String> {
    let format = unsafe { (*frame).format };
    match format {
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_U8 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_U8)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S16 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S16)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S32 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S32)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_DBL as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_DBL)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_U8P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_U8P)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S32P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S32P)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S64 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S64)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S64P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S64P)
        }
        _ => Err(format!("FFmpeg 音频帧采样格式无效：{format}")),
    }
}

pub(super) fn audio_sample_len(
    samples: c_int,
    channels: c_int,
) -> std::result::Result<usize, String> {
    if samples < 0 || channels <= 0 {
        return Err("音频帧尺寸无效".to_string());
    }
    usize::try_from(samples)
        .ok()
        .and_then(|samples| samples.checked_mul(usize::try_from(channels).ok()?))
        .ok_or_else(|| "音频帧过大".to_string())
}

pub(super) fn audio_samples_duration(
    samples: usize,
    sample_rate: c_int,
    channels: c_int,
) -> Duration {
    if samples == 0 || sample_rate <= 0 || channels <= 0 {
        return Duration::ZERO;
    }

    let denominator = (sample_rate as u128).saturating_mul(channels as u128);
    if denominator == 0 {
        return Duration::ZERO;
    }
    let nanos = (samples as u128).saturating_mul(1_000_000_000) / denominator;
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

pub(super) fn samples_for_duration(
    timeline_nsecs: u64,
    sample_rate: c_int,
    channels: c_int,
) -> u64 {
    if timeline_nsecs == 0 || sample_rate <= 0 || channels <= 0 {
        return 0;
    }

    let samples = (timeline_nsecs as u128)
        .saturating_mul(sample_rate as u128)
        .saturating_mul(channels as u128)
        / 1_000_000_000;
    u64::try_from(samples).unwrap_or(u64::MAX)
}

pub(super) fn zeroed_channel_layout() -> ffi::AVChannelLayout {
    unsafe { std::mem::zeroed() }
}
