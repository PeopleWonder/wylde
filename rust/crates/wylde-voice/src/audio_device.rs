//! Audio-device adapter — the single place in `wylde-voice` (and the
//! workspace) that touches `cpal` directly, so a future breaking `cpal` bump
//! is a one-file change rather than a sweep across `mic.rs` + `playback.rs`
//! (see #290, dependency isolation).
//!
//! `cpal` is effectively an audio-I/O *framework*: its `Device`, `Stream`,
//! `SampleFormat`, and typed callbacks permeate any capture/playback code.
//! Rather than leak those types, this module exposes a **normalized-`f32`**
//! interface:
//!
//! * capture hands the caller each driver buffer already downmixed to `f32`
//!   (the i16/u16 → f32 conversion lives here, not in `mic.rs`);
//! * playback pulls from a caller-supplied `f32` fill callback and converts to
//!   the device's native sample format here (the f32 → i16/u16 conversion
//!   lives here, not in `playback.rs`).
//!
//! The `!Send` `cpal::Stream` is owned by [`InputStream`] / [`OutputStream`];
//! callers must build and drop those on the same thread (the dedicated worker
//! threads in `mic.rs` / `playback.rs`), exactly as the audio driver requires.
//!
//! The sample-format conversions — the churny surface a `cpal` bump is most
//! likely to disturb — are pure functions with unit tests below. The
//! host/device/stream calls remain untestable on a headless CI runner (cpal
//! enumeration access-violates without an audio device), unchanged from before
//! this module existed.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

/// A device's human-readable name, if the host exposes one (for logs). cpal
/// 0.18 moved this off `Device` onto a structured `DeviceDescription`.
fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_string())
}

/// Error opening a device or building a stream. String-valued so the cpal
/// error types don't leak past this module.
#[derive(Debug, Clone)]
pub enum AudioError {
    /// The host reports no default input/output device.
    NoDevice,
    /// The device has no usable default config.
    NoSupportedConfig(String),
    /// Stream construction failed.
    Build(String),
    /// Stream play failed.
    Play(String),
    /// The device's default sample format is one we don't handle.
    UnsupportedFormat(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::NoDevice => write!(f, "no default device available"),
            AudioError::NoSupportedConfig(e) => write!(f, "device has no supported config: {e}"),
            AudioError::Build(e) => write!(f, "stream build failed: {e}"),
            AudioError::Play(e) => write!(f, "stream play failed: {e}"),
            AudioError::UnsupportedFormat(e) => write!(f, "unsupported sample format: {e}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// The negotiated hardware format for an opened device.
#[derive(Debug, Clone, Copy)]
pub struct DeviceFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

// ── Sample-format conversion (pure; the cpal-bump-sensitive surface) ──────
//
// These mirror the exact scaling the pre-adapter call sites used, so behaviour
// is byte-for-byte identical. Centralised + tested here.

/// i16 sample → normalized f32 in roughly [-1.0, 1.0].
fn i16_to_f32(s: i16) -> f32 {
    s as f32 / i16::MAX as f32
}

/// u16 sample (centred at `i16::MAX`) → normalized f32.
fn u16_to_f32(s: u16) -> f32 {
    (s as f32 - i16::MAX as f32) / i16::MAX as f32
}

/// Normalized f32 → i16, clamping out-of-range input.
fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

/// Normalized f32 → u16 (centred at `i16::MAX`), clamping to the u16 range.
fn f32_to_u16(s: f32) -> u16 {
    let scaled = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i32 + i16::MAX as i32;
    scaled.clamp(0, u16::MAX as i32) as u16
}

// ── Input (capture) ──────────────────────────────────────────────────────

/// A handle to the default input device plus its negotiated format. Opened on
/// any thread (it is `Send`); the stream it builds must live on the thread that
/// calls [`InputDevice::run`].
pub struct InputDevice {
    device: cpal::Device,
    sample_format: SampleFormat,
    format: DeviceFormat,
}

impl InputDevice {
    /// The hardware sample rate / channel count negotiated for this device.
    pub fn format(&self) -> DeviceFormat {
        self.format
    }

    /// The device's human-readable name, if the host exposes one (for logs).
    pub fn name(&self) -> Option<String> {
        device_name(&self.device)
    }

    /// Build and start the input stream. `on_samples` receives each driver
    /// buffer already converted to interleaved `f32` (the caller does its own
    /// downmix/resample). Returns the running [`InputStream`]; drop it to stop.
    ///
    /// Must be called on the thread that will own and drop the stream.
    pub fn run<F>(self, mut on_samples: F) -> Result<InputStream, AudioError>
    where
        F: FnMut(&[f32]) + Send + 'static,
    {
        let config = cpal::StreamConfig {
            channels: self.format.channels,
            sample_rate: self.format.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };
        let on_err = |e: cpal::Error| {
            tracing::warn!("wylde-voice: input cpal stream error: {e}");
        };
        let device = self.device;

        let stream = match self.sample_format {
            SampleFormat::F32 => device
                .build_input_stream(
                    config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| on_samples(data),
                    on_err,
                    None,
                )
                .map_err(|e| AudioError::Build(e.to_string()))?,
            SampleFormat::I16 => device
                .build_input_stream(
                    config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        let buf: Vec<f32> = data.iter().copied().map(i16_to_f32).collect();
                        on_samples(&buf);
                    },
                    on_err,
                    None,
                )
                .map_err(|e| AudioError::Build(e.to_string()))?,
            SampleFormat::U16 => device
                .build_input_stream(
                    config,
                    move |data: &[u16], _: &cpal::InputCallbackInfo| {
                        let buf: Vec<f32> = data.iter().copied().map(u16_to_f32).collect();
                        on_samples(&buf);
                    },
                    on_err,
                    None,
                )
                .map_err(|e| AudioError::Build(e.to_string()))?,
            other => return Err(AudioError::UnsupportedFormat(format!("{other:?}"))),
        };

        stream.play().map_err(|e| AudioError::Play(e.to_string()))?;
        Ok(InputStream { _stream: stream })
    }
}

/// A running input stream. `!Send`; owns the `cpal::Stream`. Dropping it stops
/// capture and releases the device on the owning thread.
pub struct InputStream {
    _stream: cpal::Stream,
}

/// Open the host default input device and read its negotiated format.
pub fn open_default_input() -> Result<InputDevice, AudioError> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or(AudioError::NoDevice)?;
    let supported = device
        .default_input_config()
        .map_err(|e| AudioError::NoSupportedConfig(e.to_string()))?;
    Ok(InputDevice {
        sample_format: supported.sample_format(),
        format: DeviceFormat {
            sample_rate: supported.sample_rate(),
            channels: supported.channels(),
        },
        device,
    })
}

/// Enumerate input device names plus the system default's name. Read-only host
/// query — does not open a stream. Names are de-duplicated and sorted.
pub fn input_device_names() -> Result<(Option<String>, Vec<String>), AudioError> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| device_name(&d));
    let mut names = Vec::new();
    let devices = host
        .input_devices()
        .map_err(|e| AudioError::NoSupportedConfig(e.to_string()))?;
    for device in devices {
        if let Some(name) = device_name(&device) {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    Ok((default_name, names))
}

// ── Output (playback) ─────────────────────────────────────────────────────

/// A handle to the default output device plus its negotiated format.
pub struct OutputDevice {
    device: cpal::Device,
    sample_format: SampleFormat,
    format: DeviceFormat,
}

impl OutputDevice {
    pub fn format(&self) -> DeviceFormat {
        self.format
    }

    /// The device's human-readable name, if the host exposes one (for logs).
    pub fn name(&self) -> Option<String> {
        device_name(&self.device)
    }

    /// Build and start the output stream. `fill` is handed each driver buffer
    /// as a mutable `f32` slice to write (interleaved, device channel count);
    /// this module converts to the device's native sample format. `fill` owns
    /// any cursor / end-of-buffer bookkeeping.
    ///
    /// Must be called on the thread that will own and drop the stream.
    pub fn run<F>(self, mut fill: F) -> Result<OutputStream, AudioError>
    where
        F: FnMut(&mut [f32]) + Send + 'static,
    {
        let config = cpal::StreamConfig {
            channels: self.format.channels,
            sample_rate: self.format.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };
        let on_err = |e: cpal::Error| {
            tracing::warn!("wylde-voice: output cpal stream error: {e}");
        };
        let device = self.device;

        let stream = match self.sample_format {
            SampleFormat::F32 => device
                .build_output_stream(
                    config,
                    move |out: &mut [f32], _: &cpal::OutputCallbackInfo| fill(out),
                    on_err,
                    None,
                )
                .map_err(|e| AudioError::Build(e.to_string()))?,
            SampleFormat::I16 => device
                .build_output_stream(
                    config,
                    move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        let mut scratch = vec![0.0_f32; out.len()];
                        fill(&mut scratch);
                        for (dst, &s) in out.iter_mut().zip(scratch.iter()) {
                            *dst = f32_to_i16(s);
                        }
                    },
                    on_err,
                    None,
                )
                .map_err(|e| AudioError::Build(e.to_string()))?,
            SampleFormat::U16 => device
                .build_output_stream(
                    config,
                    move |out: &mut [u16], _: &cpal::OutputCallbackInfo| {
                        let mut scratch = vec![0.0_f32; out.len()];
                        fill(&mut scratch);
                        for (dst, &s) in out.iter_mut().zip(scratch.iter()) {
                            *dst = f32_to_u16(s);
                        }
                    },
                    on_err,
                    None,
                )
                .map_err(|e| AudioError::Build(e.to_string()))?,
            other => return Err(AudioError::UnsupportedFormat(format!("{other:?}"))),
        };

        stream.play().map_err(|e| AudioError::Play(e.to_string()))?;
        Ok(OutputStream { _stream: stream })
    }
}

/// A running output stream. `!Send`; owns the `cpal::Stream`.
pub struct OutputStream {
    _stream: cpal::Stream,
}

/// Open the host default output device and read its negotiated format.
pub fn open_default_output() -> Result<OutputDevice, AudioError> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or(AudioError::NoDevice)?;
    let supported = device
        .default_output_config()
        .map_err(|e| AudioError::NoSupportedConfig(e.to_string()))?;
    Ok(OutputDevice {
        sample_format: supported.sample_format(),
        format: DeviceFormat {
            sample_rate: supported.sample_rate(),
            channels: supported.channels(),
        },
        device,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i16_roundtrip_endpoints() {
        assert_eq!(f32_to_i16(i16_to_f32(0)), 0);
        assert_eq!(f32_to_i16(i16_to_f32(i16::MAX)), i16::MAX);
        // -i16::MAX (not i16::MIN) is the reachable negative endpoint of the
        // symmetric /i16::MAX scaling — matches the pre-adapter behaviour.
        assert_eq!(f32_to_i16(i16_to_f32(-i16::MAX)), -i16::MAX);
    }

    #[test]
    fn f32_to_i16_clamps_out_of_range() {
        assert_eq!(f32_to_i16(2.0), i16::MAX);
        assert_eq!(f32_to_i16(-2.0), -i16::MAX);
        assert_eq!(f32_to_i16(0.0), 0);
    }

    #[test]
    fn u16_centre_is_silence() {
        // f32 silence (0.0) maps to the u16 centre (i16::MAX), and that centre
        // maps back to ~0.0 — the invariant playback relies on for silence pad.
        assert_eq!(f32_to_u16(0.0), i16::MAX as u16);
        assert!(u16_to_f32(i16::MAX as u16).abs() < 1e-6);
    }

    #[test]
    fn f32_to_u16_clamps_to_range() {
        assert_eq!(
            f32_to_u16(2.0),
            (i16::MAX as i32 + i16::MAX as i32).min(u16::MAX as i32) as u16
        );
        assert_eq!(f32_to_u16(-2.0), 0);
    }

    #[test]
    fn u16_to_f32_endpoints_match_pre_adapter() {
        // Exactly the formula mic.rs used inline for u16 capture.
        assert_eq!(u16_to_f32(0), (0.0 - i16::MAX as f32) / i16::MAX as f32);
        assert_eq!(
            u16_to_f32(u16::MAX),
            (u16::MAX as f32 - i16::MAX as f32) / i16::MAX as f32
        );
    }
}
