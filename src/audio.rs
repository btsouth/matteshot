//! WASAPI audio capture for recordings: system loopback (what you hear) or
//! the default microphone. Runs on its own thread, ships interleaved f32
//! PCM chunks over a channel; the video loop muxes them into the MP4.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use anyhow::{Context, Result};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

#[derive(Clone, Copy, PartialEq)]
pub enum Source {
    System,
    Mic,
}

pub struct Format {
    pub rate: u32,
    pub channels: u16,
}

pub struct Chunk {
    /// Interleaved f32 samples.
    pub samples: Vec<f32>,
}

/// AAC accepts 44.1/48 kHz and 1-2 channels only; high-rate devices (192 kHz
/// interfaces are common) must be converted. Linear interpolation with state
/// carried across chunks so the stream stays continuous.
pub struct Resampler {
    ratio: f64,
    pos: f64,
    channels: usize,
    out_channels: usize,
    tail: Vec<f32>,
}

impl Resampler {
    pub fn new(from: u32, to: u32, channels: u16, out_channels: u16) -> Self {
        Resampler {
            ratio: from as f64 / to as f64,
            pos: 0.0,
            channels: channels as usize,
            out_channels: out_channels as usize,
            tail: Vec::new(),
        }
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(input);
        let frames = buf.len() / self.channels;
        if frames < 2 {
            self.tail = buf;
            return Vec::new();
        }
        let mut out = Vec::new();
        while self.pos + 1.0 < frames as f64 {
            let i = self.pos.floor() as usize;
            let f = (self.pos - i as f64) as f32;
            for c in 0..self.out_channels {
                let sc = c.min(self.channels - 1);
                let a = buf[i * self.channels + sc];
                let b = buf[(i + 1) * self.channels + sc];
                out.push(a + (b - a) * f);
            }
            self.pos += self.ratio;
        }
        // Keep the unconsumed remainder (plus one frame of context).
        let consumed = self.pos.floor() as usize;
        if consumed > 0 {
            self.tail = buf[consumed * self.channels..].to_vec();
            self.pos -= consumed as f64;
        } else {
            self.tail = buf;
        }
        out
    }
}

/// Encoder-friendly rate/channels for a device format.
pub fn encode_format(dev: &Format) -> Format {
    let rate = if dev.rate % 44100 == 0 && dev.rate % 48000 != 0 { 44100 } else { 48000 };
    Format { rate, channels: dev.channels.min(2).max(1) }
}

/// Open the device on the calling thread just long enough to learn the mix
/// format, so the sink writer can be configured before capture starts.
pub fn probe_format(source: Source) -> Result<Format> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let flow = if source == Source::System { eRender } else { eCapture };
        let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let fmt = client.GetMixFormat()?;
        let format = Format { rate: (*fmt).nSamplesPerSec, channels: (*fmt).nChannels };
        CoTaskMemFree(Some(fmt as *const _));
        Ok(format)
    }
}

/// Capture until `stop`. Sends chunks; silent-flag packets become zeros so
/// the timeline stays continuous.
pub fn capture_thread(source: Source, tx: Sender<Chunk>, stop: Arc<AtomicBool>) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let flow = if source == Source::System { eRender } else { eCapture };
        let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let fmt = client.GetMixFormat()?;
        let channels = (*fmt).nChannels as usize;
        let bits = (*fmt).wBitsPerSample;
        let block = (*fmt).nBlockAlign as usize;

        let flags = if source == Source::System { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, fmt, None)
            .context("audio init")?;
        CoTaskMemFree(Some(fmt as *const _));
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start().context("audio start")?;

        while !stop.load(Ordering::Relaxed) {
            loop {
                let packet = capture.GetNextPacketSize().unwrap_or(0);
                if packet == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut fl = 0u32;
                if capture.GetBuffer(&mut data, &mut frames, &mut fl, None, None).is_err() {
                    break;
                }
                let n = frames as usize * channels;
                let mut samples = vec![0f32; n];
                if fl & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 && !data.is_null() {
                    if bits == 32 {
                        let src = std::slice::from_raw_parts(data as *const f32, n);
                        samples.copy_from_slice(src);
                    } else if bits == 16 {
                        let src = std::slice::from_raw_parts(
                            data as *const i16,
                            frames as usize * block / 2,
                        );
                        for (d, s) in samples.iter_mut().zip(src.iter()) {
                            *d = *s as f32 / 32768.0;
                        }
                    }
                }
                let _ = capture.ReleaseBuffer(frames);
                if tx.send(Chunk { samples }).is_err() {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = client.Stop();
    }
    Ok(())
}
