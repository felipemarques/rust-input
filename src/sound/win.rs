//! The shared sound on Windows (0.52.0), through WASAPI (cpal).
//!
//! The sound of this computer is recorded in loopback from the default
//! output, which keeps recording what plays also while the output is muted:
//! so its speakers are silenced by muting it. A marker file is written
//! before muting, so only a mute RustInput made is undone: when it stops
//! sending, and at the next start after a crash ([`clean_up`]).

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use cpal::{
    Data, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use windows::Win32::{
    Media::Audio::{
        Endpoints::IAudioEndpointVolume, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
    },
    System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx},
};

use super::wire::{CHANNELS, PCM_BYTES, RATE, SAMPLES};

/// samples at 48 kHz, both channels, at most (0.5 s)
const MOST: usize = RATE as usize;
const TURN: Duration = Duration::from_millis(5);

/// the file that says RustInput muted the output
fn marker() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| {
        PathBuf::from(d)
            .join("RustInput")
            .join("speakers-muted-by-rustinput")
    })
}

fn endpoint() -> Result<IAudioEndpointVolume, String> {
    unsafe {
        // already initialized in another mode is fine
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| format!("no output: {e}"))?;
        device
            .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            .map_err(|e| e.to_string())
    }
}

fn set_mute(mute: bool) -> Result<(), String> {
    let volume = endpoint()?;
    unsafe { volume.SetMute(mute, std::ptr::null()) }.map_err(|e| e.to_string())
}

/// unmutes the output a RustInput muted and did not unmute (crash)
pub(crate) fn clean_up() -> Result<(), String> {
    let marker = marker().ok_or("no folder for the marker")?;
    if marker.exists() {
        log::info!("sound: unmuting the speakers an earlier RustInput muted");
        set_mute(false)?;
        let _ = std::fs::remove_file(&marker);
    }
    Ok(())
}

/// the output muted by RustInput, unmuted when dropped; nothing when it
/// was muted already
struct Muted;

fn mute() -> Result<Option<Muted>, String> {
    let volume = endpoint()?;
    let muted = unsafe { volume.GetMute() }
        .map_err(|e| e.to_string())?
        .as_bool();
    if muted {
        return Ok(None);
    }
    if let Some(marker) = marker() {
        if let Some(dir) = marker.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&marker, b"").map_err(|e| format!("{}: {e}", marker.display()))?;
    }
    unsafe { volume.SetMute(true, std::ptr::null()) }.map_err(|e| e.to_string())?;
    log::info!("sound: the speakers are muted while the sound goes to another computer");
    Ok(Some(Muted))
}

impl Drop for Muted {
    fn drop(&mut self) {
        match set_mute(false) {
            Ok(()) => log::info!("sound: the speakers play again"),
            Err(e) => log::warn!("sound: the speakers could not be unmuted: {e}"),
        }
        if let Some(marker) = marker() {
            let _ = std::fs::remove_file(marker);
        }
    }
}

/// samples at 48 kHz stereo between a stream of cpal and RustInput
#[derive(Default)]
struct Ring {
    samples: Mutex<VecDeque<i16>>,
    more: Condvar,
}

/// linear resampling of stereo frames from one rate to another
struct Resampler {
    /// input frames per output frame
    step: f64,
    /// where the next output frame is, between `previous` and the next input
    at: f64,
    previous: [f32; 2],
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            step: f64::from(from) / f64::from(to),
            at: 0.0,
            previous: [0.0; 2],
        }
    }

    fn push(&mut self, frame: [f32; 2], out: &mut impl FnMut([f32; 2])) {
        while self.at < 1.0 {
            let t = self.at as f32;
            out([
                self.previous[0] + (frame[0] - self.previous[0]) * t,
                self.previous[1] + (frame[1] - self.previous[1]) * t,
            ]);
            self.at += self.step;
        }
        self.at -= 1.0;
        self.previous = frame;
    }
}

/// the samples of `data` as f32, whatever their format
fn floats(data: &Data) -> Vec<f32> {
    match data.sample_format() {
        SampleFormat::F32 => data.as_slice::<f32>().map(<[f32]>::to_vec),
        SampleFormat::I16 => data
            .as_slice::<i16>()
            .map(|s| s.iter().map(|&v| f32::from(v) / 32768.0).collect()),
        SampleFormat::I32 => data
            .as_slice::<i32>()
            .map(|s| s.iter().map(|&v| v as f32 / 2_147_483_648.0).collect()),
        SampleFormat::F64 => data
            .as_slice::<f64>()
            .map(|s| s.iter().map(|&v| v as f32).collect()),
        _ => None,
    }
    .unwrap_or_default()
}

fn to_i16(v: f32) -> i16 {
    (v * 32767.0).clamp(-32768.0, 32767.0) as i16
}

/// what records the sound this computer plays, at 48 kHz stereo
pub(crate) struct Capture {
    ring: Arc<Ring>,
    next: Instant,
    // the stream goes before the speakers are unmuted
    _stream: Stream,
    _muted: Option<Muted>,
}

impl Capture {
    pub(crate) fn new(keep_speakers: bool) -> Result<Self, String> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or("no output to record")?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.config();
        let channels = usize::from(config.channels.max(1));
        let ring = Arc::new(Ring::default());
        let mut resampler = Resampler::new(config.sample_rate, RATE);
        let stream = {
            let ring = ring.clone();
            device
                .build_input_stream_raw(
                    config,
                    format,
                    move |data: &Data, _: &_| {
                        let input = floats(data);
                        let mut samples = ring.samples.lock().expect("lock");
                        for frame in input.chunks_exact(channels) {
                            let stereo = [frame[0], frame[if channels > 1 { 1 } else { 0 }]];
                            resampler.push(stereo, &mut |[l, r]| {
                                samples.push_back(to_i16(l));
                                samples.push_back(to_i16(r));
                            });
                        }
                        while samples.len() > MOST {
                            samples.pop_front();
                        }
                        ring.more.notify_one();
                    },
                    |e| log::warn!("sound: recording: {e}"),
                    None,
                )
                .map_err(|e| format!("recording the output in loopback: {e}"))?
        };
        stream.play().map_err(|e| e.to_string())?;
        let muted = if keep_speakers { None } else { mute()? };
        Ok(Self {
            ring,
            next: Instant::now() + TURN,
            _stream: stream,
            _muted: muted,
        })
    }

    /// The next 5 ms. Windows records nothing while nothing plays: then
    /// silence, at the pace of the turns.
    pub(crate) fn read(&mut self, into: &mut [u8; PCM_BYTES]) -> Result<(), String> {
        let deadline = self.next;
        let now = Instant::now();
        // far behind (the computer slept): start the pace again from now
        self.next = if now > deadline + 20 * TURN {
            now + TURN
        } else {
            deadline + TURN
        };
        let mut samples = self.ring.samples.lock().expect("lock");
        while samples.len() < SAMPLES {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            samples = self.ring.more.wait_timeout(samples, left).expect("lock").0;
        }
        if samples.len() < SAMPLES {
            into.fill(0);
            return Ok(());
        }
        for (i, sample) in samples.drain(..SAMPLES).enumerate() {
            into[2 * i..2 * i + 2].copy_from_slice(&sample.to_le_bytes());
        }
        Ok(())
    }
}

/// plays at 48 kHz stereo on the default output
pub(crate) struct Player {
    ring: Arc<Ring>,
    _stream: Stream,
}

impl Player {
    pub(crate) fn new() -> Result<Self, String> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or("no output to play on")?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.config();
        let channels = usize::from(config.channels.max(1));
        let ring = Arc::new(Ring::default());
        // from 48 kHz to the rate of the output, frame by frame
        let mut resampler = Resampler::new(RATE, config.sample_rate);
        let mut pending: VecDeque<[f32; 2]> = VecDeque::new();
        let stream = {
            let ring = ring.clone();
            device
                .build_output_stream_raw(
                    config,
                    format,
                    move |data: &mut Data, _: &_| {
                        let frames = data.len() / channels;
                        {
                            let mut samples = ring.samples.lock().expect("lock");
                            while pending.len() < frames {
                                let frame = match (samples.pop_front(), samples.pop_front()) {
                                    (Some(l), Some(r)) => {
                                        [f32::from(l) / 32768.0, f32::from(r) / 32768.0]
                                    }
                                    _ => [0.0; 2],
                                };
                                resampler.push(frame, &mut |f| pending.push_back(f));
                            }
                        }
                        let mut write = |i: usize, v: f32| match data.sample_format() {
                            SampleFormat::F32 => {
                                if let Some(out) = data.as_slice_mut::<f32>() {
                                    out[i] = v;
                                }
                            }
                            SampleFormat::I16 => {
                                if let Some(out) = data.as_slice_mut::<i16>() {
                                    out[i] = to_i16(v);
                                }
                            }
                            SampleFormat::I32 => {
                                if let Some(out) = data.as_slice_mut::<i32>() {
                                    out[i] = (v.clamp(-1.0, 1.0) * 2_147_483_647.0) as i32;
                                }
                            }
                            _ => {}
                        };
                        for frame in 0..frames {
                            let [l, r] = pending.pop_front().unwrap_or([0.0; 2]);
                            for channel in 0..channels {
                                let v = match (channels, channel) {
                                    (1, _) => (l + r) / 2.0,
                                    (_, 0) => l,
                                    (_, 1) => r,
                                    _ => 0.0,
                                };
                                write(frame * channels + channel, v);
                            }
                        }
                    },
                    |e| log::warn!("sound: playing: {e}"),
                    None,
                )
                .map_err(|e| format!("playing: {e}"))?
        };
        stream.play().map_err(|e| e.to_string())?;
        Ok(Self {
            ring,
            _stream: stream,
        })
    }

    /// plays 5 ms; waits while the output holds 20 ms
    pub(crate) fn write(&mut self, pcm: &[u8; PCM_BYTES]) -> Result<(), String> {
        let started = Instant::now();
        loop {
            {
                let mut samples = self.ring.samples.lock().expect("lock");
                if samples.len() < 4 * SAMPLES {
                    for pair in pcm.chunks_exact(2) {
                        samples.push_back(i16::from_le_bytes([pair[0], pair[1]]));
                    }
                    return Ok(());
                }
            }
            if started.elapsed() > Duration::from_secs(1) {
                return Err("the output does not play".into());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

const _: () = assert!(CHANNELS == 2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_the_rate() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut n = 0;
        for _ in 0..44_100 {
            r.push([0.5, -0.5], &mut |_| n += 1);
        }
        assert!((47_990..=48_010).contains(&n), "{n}");
        let mut same = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        for v in [0.1, 0.2, 0.3] {
            same.push([v, v], &mut |f| out.push(f[0]));
        }
        assert_eq!(out.len(), 3);
    }
}
