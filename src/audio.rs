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
    let rate = if dev.rate.is_multiple_of(44100) && !dev.rate.is_multiple_of(48000) {
        44100
    } else {
        48000
    };
    Format { rate, channels: dev.channels.clamp(1, 2) }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_resampling_reproduces_the_input_exactly() {
        let mut r = Resampler::new(48_000, 48_000, 1, 1);
        let input = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let out = r.process(&input);
        // The final frame is always held back as interpolation context for
        // the next chunk, so a 1.0 ratio reproduces every frame but the last.
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn downsampling_by_two_keeps_every_other_frame() {
        let mut r = Resampler::new(48_000, 24_000, 1, 1);
        let input: Vec<f32> = (0..10).map(|v| v as f32).collect();
        let out = r.process(&input);
        assert_eq!(out, vec![0.0, 2.0, 4.0, 6.0, 8.0]);
    }

    #[test]
    fn upsampling_by_two_interpolates_the_halfway_point() {
        let mut r = Resampler::new(24_000, 48_000, 1, 1);
        let input = vec![0.0, 10.0, 20.0, 30.0];
        let out = r.process(&input);
        // Frame i lands exactly on input[i]; the odd frames fall halfway
        // between consecutive input samples.
        assert_eq!(out, vec![0.0, 5.0, 10.0, 15.0, 20.0, 25.0]);
    }

    #[test]
    fn splitting_the_same_input_across_chunks_matches_one_shot_processing() {
        let input: Vec<f32> = (0..12).map(|v| v as f32).collect();

        let mut whole = Resampler::new(48_000, 48_000, 1, 1);
        let one_shot = whole.process(&input);

        let mut chunked = Resampler::new(48_000, 48_000, 1, 1);
        let mut piecewise = Vec::new();
        for chunk in input.chunks(4) {
            piecewise.extend(chunked.process(chunk));
        }

        assert_eq!(piecewise, one_shot, "a chunk boundary must not disturb the resampled stream");
    }

    #[test]
    fn a_fractional_position_spanning_a_chunk_boundary_still_interpolates_correctly() {
        // A 1.5x ratio deliberately never lands on an integer frame position,
        // so this exercises exactly what the tail exists for: a chunk
        // boundary landing mid-interpolation, between one sample already
        // consumed and one that has not arrived yet.
        let input: Vec<f32> = (0..12).map(|v| v as f32).collect();

        let mut whole = Resampler::new(48_000, 32_000, 1, 1);
        let one_shot = whole.process(&input);
        assert_eq!(one_shot, vec![0.0, 1.5, 3.0, 4.5, 6.0, 7.5, 9.0, 10.5]);

        let mut chunked = Resampler::new(48_000, 32_000, 1, 1);
        let mut piecewise = Vec::new();
        for chunk in input.chunks(5) {
            piecewise.extend(chunked.process(chunk));
        }

        assert_eq!(piecewise, one_shot, "a chunk boundary must not disturb the resampled stream");
    }

    #[test]
    fn a_chunk_too_short_to_interpolate_is_held_for_the_next_one() {
        let mut r = Resampler::new(48_000, 48_000, 1, 1);
        assert_eq!(
            r.process(&[1.0]),
            Vec::<f32>::new(),
            "one frame alone has no pair to interpolate against"
        );
        // The held-back frame plus a follow-up chunk should now resolve.
        let out = r.process(&[2.0, 3.0]);
        assert_eq!(out, vec![1.0, 2.0]);
    }

    #[test]
    fn a_surround_source_downmixes_to_the_front_stereo_pair() {
        let mut r = Resampler::new(48_000, 48_000, 6, 2);
        // Two frames of 6 channels: front-L, front-R, center, LFE, rear-L,
        // rear-R. The center/LFE/rear channels are poisoned with 9.0, which
        // must never reach the output.
        let input = vec![1.0, 2.0, 9.0, 9.0, 9.0, 9.0, 3.0, 4.0, 9.0, 9.0, 9.0, 9.0];
        let out = r.process(&input);
        assert_eq!(out, vec![1.0, 2.0], "only the front L/R channels should survive");
    }

    #[test]
    fn encode_format_prefers_44_1k_only_when_the_device_is_a_multiple_of_it_and_not_48k() {
        for (device_rate, expected) in [
            (44_100, 44_100),
            (48_000, 48_000),
            (88_200, 44_100),  // 2x44.1k
            (96_000, 48_000),  // 2x48k
            (192_000, 48_000), // common high-rate interface; not a 44.1k multiple
        ] {
            let out = encode_format(&Format { rate: device_rate, channels: 2 });
            assert_eq!(out.rate, expected, "for device rate {device_rate}");
        }
    }

    #[test]
    fn encode_format_clamps_channels_into_what_aac_accepts() {
        for (device_channels, expected) in [(0u16, 1u16), (1, 1), (2, 2), (6, 2), (8, 2)] {
            let out = encode_format(&Format { rate: 48_000, channels: device_channels });
            assert_eq!(out.channels, expected, "for device channels {device_channels}");
        }
    }
}
