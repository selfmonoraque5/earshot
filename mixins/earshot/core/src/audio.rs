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
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};

use crate::codec::SAMPLE_RATE;
use crate::resample::Resampler;

/// Mono 48 kHz samples from the microphone.
pub type CaptureQueue = Arc<Mutex<VecDeque<f32>>>;
/// Stereo 48 kHz frames for the speakers.
pub type PlaybackQueue = Arc<Mutex<VecDeque<[f32; 2]>>>;

const CAPTURE_LIMIT: usize = SAMPLE_RATE as usize;

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

/// Running microphone and speaker streams. Dropping it stops them.
pub struct AudioIo {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// What each side is using, or why it isn't (for the settings page and logs).
    pub input: Result<String, String>,
    pub output: Result<String, String>,
}

impl AudioIo {
    /// Opens the named devices, or the system defaults for `None` or a name that's gone.
    pub fn start(input: Option<String>, output: Option<String>, capture: CaptureQueue, playback: PlaybackQueue) -> AudioIo {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("earshot-audio".into())
            .spawn(move || {
                let host = cpal::default_host();
                // A chosen device that won't open (on Linux, ALSA lists plugins such as `jack` or
                // `oss` that often don't work) falls back to the system default, saying why.
                let inp = open_input(&host, input.as_deref(), capture.clone()).or_else(|e| match input.as_deref() {
                    Some(name) => open_input(&host, None, capture)
                        .map(|(d, s)| (format!("{d} (\"{name}\" failed: {e})"), s))
                        .map_err(|e2| format!("{e}; default: {e2}")),
                    None => Err(e),
                });
                let out = open_output(&host, output.as_deref(), playback.clone()).or_else(|e| match output.as_deref() {
                    Some(name) => open_output(&host, None, playback)
                        .map(|(d, s)| (format!("{d} (\"{name}\" failed: {e})"), s))
                        .map_err(|e2| format!("{e}; default: {e2}")),
                    None => Err(e),
                });
                let _ = tx.send((
                    inp.as_ref().map(|(n, _)| n.clone()).map_err(Clone::clone),
                    out.as_ref().map(|(n, _)| n.clone()).map_err(Clone::clone),
                ));
                // Keep the streams alive until asked to stop.
                while !stop2.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                drop(inp);
                drop(out);
            })
            .ok();
        let (input, output) = rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| (Err("audio thread didn't start".into()), Err("audio thread didn't start".into())));
        AudioIo { stop, thread, input, output }
    }
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

fn open_input(host: &cpal::Host, name: Option<&str>, capture: CaptureQueue) -> Result<(String, cpal::Stream), String> {
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
    stream.play().map_err(|e| e.to_string())?;
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

fn open_output(host: &cpal::Host, name: Option<&str>, playback: PlaybackQueue) -> Result<(String, cpal::Stream), String> {
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
    stream.play().map_err(|e| e.to_string())?;
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
