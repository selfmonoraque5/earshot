//! Microphone and speakers, through cpal.
//!
//! The streams live on a thread of their own (cpal streams can't move between threads on every
//! platform) and talk to the rest of Earshot through two queues of 48 kHz audio:
//! - capture: mono samples from the microphone, at most one second kept;
//! - playback: interleaved stereo frames the mixer has made, drained by the speakers.
//!
//! Any device rate and channel count is converted with [`Resampler`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};

use crate::codec::SAMPLE_RATE;
use crate::resample::Resampler;

/// Mono 48 kHz samples from the microphone.
pub type CaptureQueue = Arc<Mutex<VecDeque<f32>>>;
/// Stereo 48 kHz frames for the speakers.
pub type PlaybackQueue = Arc<Mutex<VecDeque<[f32; 2]>>>;

const CAPTURE_LIMIT: usize = SAMPLE_RATE as usize;
const DEVICE_RETRY: Duration = Duration::from_secs(1);

/// Names of the input devices, for the settings page.
pub fn input_devices() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|ds| ds.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

/// Names of the output devices, for the settings page.
pub fn output_devices() -> Vec<String> {
    cpal::default_host()
        .output_devices()
        .map(|ds| ds.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

/// A device thread owns its CPAL stream. Dropping it stops that stream.
pub(crate) struct AudioIo {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioIo {
    /// `open` builds a paused stream; `start` plays it only if `D` is still wanted, checked
    /// under the same lock that muting and switching off take, so a stream never starts after
    /// the player has muted or switched voice chat off.
    pub(crate) fn start<D: PartialEq>(
        name: &str,
        wanted: impl Fn() -> Option<D> + Send + 'static,
        open: impl Fn(&cpal::Host, &D) -> Result<(String, cpal::Stream), String> + Send + 'static,
        start: impl Fn(&D, &cpal::Stream) -> Result<bool, String> + Send + 'static,
        status: impl Fn(String) + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let mut current = None;
                let mut stream = None;
                let mut retry_at = Instant::now();
                while !stopping.load(Ordering::Relaxed) {
                    let next = wanted();
                    if next != current {
                        drop(stream.take());
                        current = next;
                        retry_at = Instant::now();
                        status("closed".into());
                    }
                    if stream.is_none()
                        && let Some(device) = &current
                        && Instant::now() >= retry_at
                    {
                        // Built paused: the player may mute, disconnect or change devices while it opens.
                        let opened = open(&cpal::default_host(), device);
                        if stopping.load(Ordering::Relaxed) {
                            break;
                        }
                        match opened.and_then(|(name, handle)| Ok((start(device, &handle)?, name, handle))) {
                            Ok((true, name, handle)) => {
                                stream = Some(handle);
                                status(name);
                            }
                            // No longer wanted: dropped unstarted; the next round sees the new request.
                            Ok((false, ..)) => current = None,
                            Err(e) => {
                                retry_at = Instant::now() + DEVICE_RETRY;
                                status(format!("none ({e})"));
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
            .ok();
        Self { stop, thread }
    }
}

/// Plays a built stream for `device` only while it is still the one `wanted` (read under the
/// caller's lock): false, and `play` is never called, once the player has muted or switched off.
pub(crate) fn start_if_wanted<D: PartialEq>(
    device: &D,
    wanted: Option<D>,
    play: impl FnOnce() -> Result<(), String>,
) -> Result<bool, String> {
    if wanted.as_ref() != Some(device) {
        return Ok(false);
    }
    play().map(|()| true)
}

impl Drop for AudioIo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn pick(devices: impl Iterator<Item = cpal::Device>, name: Option<&str>, default: Option<cpal::Device>) -> Option<cpal::Device> {
    if let Some(name) = name {
        if let Some(d) = devices.into_iter().find(|d| d.name().ok().as_deref() == Some(name)) {
            return Some(d);
        }
    }
    default
}

// A chosen device that fails falls back to the system default, retaining the original error.
fn with_fallback(
    name: Option<&str>, open: impl Fn(Option<&str>) -> Result<(String, cpal::Stream), String>,
) -> Result<(String, cpal::Stream), String> {
    open(name).or_else(|e| match name {
        Some(name) => open(None)
            .map(|(device, stream)| (format!("{device} (\"{name}\" failed: {e})"), stream))
            .map_err(|e2| format!("{e}; default: {e2}")),
        None => Err(e),
    })
}

pub(crate) fn open_input(host: &cpal::Host, name: Option<&str>, capture: CaptureQueue) -> Result<(String, cpal::Stream), String> {
    with_fallback(name, |name| try_open_input(host, name, capture.clone()))
}

fn try_open_input(host: &cpal::Host, name: Option<&str>, capture: CaptureQueue) -> Result<(String, cpal::Stream), String> {
    let devices = host.input_devices().map_err(|e| e.to_string())?;
    let device = pick(devices, name, host.default_input_device()).ok_or("no microphone found")?;
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let config = supported.config();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_input::<f32>(&device, &config, capture),
        SampleFormat::I16 => build_input::<i16>(&device, &config, capture),
        SampleFormat::U16 => build_input::<u16>(&device, &config, capture),
        SampleFormat::I32 => build_input::<i32>(&device, &config, capture),
        other => return Err(format!("unsupported microphone sample format {other:?}")),
    }?;
    // Left paused: `AudioIo`'s `start` plays it.
    let _ = stream.pause();
    Ok((device.name().unwrap_or_else(|_| "microphone".into()), stream))
}

fn build_input<T>(device: &cpal::Device, config: &cpal::StreamConfig, capture: CaptureQueue) -> Result<cpal::Stream, String>
where
    T: SizedSample + Sample,
    f32: FromSample<T>,
{
    let channels = config.channels.max(1) as usize;
    let mut resampler = Resampler::<1>::new(config.sample_rate.0, SAMPLE_RATE);
    let mut pending: VecDeque<[f32; 1]> = VecDeque::new();
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                // Downmix to mono, convert to 48 kHz, queue.
                for frame in data.chunks(channels) {
                    let sum: f32 = frame.iter().map(|s| f32::from_sample_(*s)).sum();
                    pending.push_back([sum / channels as f32]);
                }
                let mut out = Vec::with_capacity(pending.len() * 2);
                while let Some([s]) = resampler.pull(&mut || pending.pop_front()) {
                    out.push(s);
                }
                if let Ok(mut q) = capture.lock() {
                    q.extend(out);
                    let excess = q.len().saturating_sub(CAPTURE_LIMIT);
                    q.drain(..excess);
                }
            },
            |e| eprintln!("[earshot] microphone error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

pub(crate) fn open_output(host: &cpal::Host, name: Option<&str>, playback: PlaybackQueue) -> Result<(String, cpal::Stream), String> {
    with_fallback(name, |name| try_open_output(host, name, playback.clone()))
}

fn try_open_output(host: &cpal::Host, name: Option<&str>, playback: PlaybackQueue) -> Result<(String, cpal::Stream), String> {
    let devices = host.output_devices().map_err(|e| e.to_string())?;
    let device = pick(devices, name, host.default_output_device()).ok_or("no speakers found")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let config = supported.config();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_output::<f32>(&device, &config, playback),
        SampleFormat::I16 => build_output::<i16>(&device, &config, playback),
        SampleFormat::U16 => build_output::<u16>(&device, &config, playback),
        SampleFormat::I32 => build_output::<i32>(&device, &config, playback),
        other => return Err(format!("unsupported speaker sample format {other:?}")),
    }?;
    let _ = stream.pause();
    Ok((device.name().unwrap_or_else(|_| "speakers".into()), stream))
}

fn build_output<T>(device: &cpal::Device, config: &cpal::StreamConfig, playback: PlaybackQueue) -> Result<cpal::Stream, String>
where
    T: SizedSample + Sample + FromSample<f32>,
{
    let channels = config.channels.max(1) as usize;
    let mut resampler = Resampler::<2>::new(SAMPLE_RATE, config.sample_rate.0);
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let Ok(mut q) = playback.lock() else {
                    data.fill(T::EQUILIBRIUM);
                    return;
                };
                for frame in data.chunks_mut(channels) {
                    // An empty queue plays silence; the resampler keeps its place.
                    let [l, r] = resampler.pull(&mut || q.pop_front()).unwrap_or([0.0, 0.0]);
                    match frame.len() {
                        1 => frame[0] = T::from_sample_((l + r) * 0.5),
                        _ => {
                            frame[0] = T::from_sample_(l);
                            frame[1] = T::from_sample_(r);
                            for extra in &mut frame[2..] {
                                *extra = T::EQUILIBRIUM;
                            }
                        }
                    }
                }
            },
            |e| eprintln!("[earshot] speaker error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_a_device_that_failed_to_open() {
        let (attempts, received) = std::sync::mpsc::channel();
        let _audio = AudioIo::start(
            "earshot-test-device",
            || Some(()),
            move |_, _| {
                attempts.send(Instant::now()).unwrap();
                Err("device temporarily unavailable".into())
            },
            |_, _| panic!("opening the test device always fails"),
            |_| {},
        );
        let first = received.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(second.duration_since(first) >= DEVICE_RETRY);
    }
}
