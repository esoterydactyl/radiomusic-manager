//! Reads Radio Music audio (WAV, AIFF/AIFC, headerless RAW) and renders trimmed and/or normalized
//! copies for the card.
//!
//! The Radio Music plays only the **left** channel of a stereo file, so waveforms, previews and
//! loudness measurements all use channel 0. Output of a processed file is always a PCM WAV with
//! the source's channels, sample rate and bit depth.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Frames read per block while streaming; keeps memory flat for very large files.
const BLOCK_FRAMES: u64 = 32 * 1024;
const WAV_HEADER_BYTES: u64 = 44;
/// Peak target for both normalize modes.
const PEAK_CEILING_DB: f64 = -1.0;
/// Loudness target for `Rms` mode (RMS of the left channel).
const RMS_TARGET_DB: f64 = -18.0;
/// Never boost by more than this; beyond it we'd mostly be amplifying noise.
const MAX_GAIN_DB: f64 = 30.0;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum NormMode {
    /// Scale so the loudest sample sits at -1 dBFS.
    Peak,
    /// Scale towards -18 dBFS RMS without letting the peak pass -1 dBFS.
    Rms,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct Trim {
    pub start: f64,
    pub end: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Container {
    Wav,
    Aiff,
    Raw,
}

#[derive(Clone, Debug)]
pub struct PcmFile {
    pub path: PathBuf,
    pub container: Container,
    pub data_offset: u64,
    pub data_len: u64,
    pub channels: u16,
    pub bits: u16,
    pub rate: u32,
    big_endian: bool,
    /// 8-bit WAV is unsigned; 8-bit AIFF is signed.
    unsigned8: bool,
}

fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn u16be(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn u32be(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// AIFF stores the sample rate as an 80-bit IEEE extended float.
fn extended_to_f64(b: &[u8]) -> f64 {
    let exp = i32::from(u16be(b) & 0x7fff);
    let mant = u64::from_be_bytes([b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9]]);
    if exp == 0 && mant == 0 {
        return 0.0;
    }
    (mant as f64) * 2f64.powi(exp - 16383 - 63)
}

fn check_pcm_shape(bits: u16, channels: u16, rate: u32) -> Result<(), String> {
    if !matches!(bits, 8 | 16 | 24 | 32) {
        return Err(format!("{bits}-bit audio isn't supported"));
    }
    if channels == 0 || channels > 8 {
        return Err(format!("{channels} channels isn't supported"));
    }
    if rate == 0 {
        return Err("The file has no sample rate".into());
    }
    Ok(())
}

impl PcmFile {
    pub fn open(path: &Path) -> Result<Self, String> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let mut f = File::open(path).map_err(|e| format!("Could not open {}: {e}", path.display()))?;
        let len = f.metadata().map_err(|e| e.to_string())?.len();

        if ext == "raw" {
            // Headerless: the firmware assumes 44.1kHz, 16-bit mono.
            return Ok(PcmFile {
                path: path.to_path_buf(),
                container: Container::Raw,
                data_offset: 0,
                data_len: len - len % 2,
                channels: 1,
                bits: 16,
                rate: 44_100,
                big_endian: false,
                unsigned8: false,
            });
        }

        let mut head = [0u8; 12];
        f.read_exact(&mut head).map_err(|_| "Not an audio file".to_string())?;
        match (&head[0..4], &head[8..12]) {
            (b"RIFF", b"WAVE") => Self::parse_wav(path, &mut f, len),
            (b"FORM", b"AIFF") | (b"FORM", b"AIFC") => Self::parse_aiff(path, &mut f, len, &head[8..12] == b"AIFC"),
            _ => Err("Not a WAV or AIFF file".into()),
        }
    }

    fn parse_wav(path: &Path, f: &mut File, len: u64) -> Result<Self, String> {
        let mut pos = 12u64;
        let mut fmt: Option<(u16, u16, u32, u16)> = None; // tag, channels, rate, bits
        while pos + 8 <= len {
            f.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
            let mut h = [0u8; 8];
            f.read_exact(&mut h).map_err(|e| e.to_string())?;
            let size = u64::from(u32le(&h[4..8]));
            let body = pos + 8;
            match &h[0..4] {
                b"fmt " => {
                    let mut b = vec![0u8; size.min(40) as usize];
                    f.read_exact(&mut b).map_err(|e| e.to_string())?;
                    if b.len() < 16 {
                        return Err("The fmt chunk is too short".into());
                    }
                    let mut tag = u16le(&b[0..2]);
                    if tag == 0xFFFE && b.len() >= 26 {
                        tag = u16le(&b[24..26]); // WAVE_FORMAT_EXTENSIBLE: the real tag is the sub-format
                    }
                    fmt = Some((tag, u16le(&b[2..4]), u32le(&b[4..8]), u16le(&b[14..16])));
                }
                b"data" => {
                    let (tag, channels, rate, bits) = fmt.ok_or("The data chunk came before fmt")?;
                    if tag != 1 {
                        return Err(if tag == 3 { "Floating-point WAV isn't supported".into() } else { format!("WAV format {tag} isn't supported") });
                    }
                    check_pcm_shape(bits, channels, rate)?;
                    // Streaming writers leave the size as 0 or 0xFFFFFFFF; trust the file length instead.
                    let avail = len - body;
                    let data_len = if size == 0 || size > avail { avail } else { size };
                    let bpf = u64::from(channels) * u64::from(bits) / 8;
                    return Ok(PcmFile {
                        path: path.to_path_buf(),
                        container: Container::Wav,
                        data_offset: body,
                        data_len: data_len - data_len % bpf,
                        channels,
                        bits,
                        rate,
                        big_endian: false,
                        unsigned8: bits == 8,
                    });
                }
                _ => {}
            }
            pos = body + size + (size & 1); // chunks are padded to an even length
        }
        Err("No audio data found".into())
    }

    fn parse_aiff(path: &Path, f: &mut File, len: u64, aifc: bool) -> Result<Self, String> {
        let mut pos = 12u64;
        let mut comm: Option<(u16, u16, u32, bool)> = None; // channels, bits, rate, little-endian
        while pos + 8 <= len {
            f.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
            let mut h = [0u8; 8];
            f.read_exact(&mut h).map_err(|e| e.to_string())?;
            let size = u64::from(u32be(&h[4..8]));
            let body = pos + 8;
            match &h[0..4] {
                b"COMM" => {
                    let mut b = vec![0u8; size.min(64) as usize];
                    f.read_exact(&mut b).map_err(|e| e.to_string())?;
                    if b.len() < 18 {
                        return Err("The COMM chunk is too short".into());
                    }
                    let mut little = false;
                    if aifc {
                        match b.get(18..22) {
                            Some(b"NONE") | Some(b"twos") => {}
                            Some(b"sowt") => little = true,
                            other => return Err(format!("Compressed AIFF ({}) isn't supported", String::from_utf8_lossy(other.unwrap_or(b"?")))),
                        }
                    }
                    comm = Some((u16be(&b[0..2]), u16be(&b[6..8]), extended_to_f64(&b[8..18]).round() as u32, little));
                }
                b"SSND" => {
                    let (channels, bits, rate, little) = comm.ok_or("The sound data came before COMM")?;
                    check_pcm_shape(bits, channels, rate)?;
                    let mut o = [0u8; 8];
                    f.read_exact(&mut o).map_err(|e| e.to_string())?;
                    let offset = u64::from(u32be(&o[0..4]));
                    let start = body + 8 + offset;
                    let avail = len.saturating_sub(start);
                    let declared = size.saturating_sub(8 + offset);
                    let data_len = declared.min(avail);
                    let bpf = u64::from(channels) * u64::from(bits) / 8;
                    return Ok(PcmFile {
                        path: path.to_path_buf(),
                        container: Container::Aiff,
                        data_offset: start,
                        data_len: data_len - data_len % bpf,
                        channels,
                        bits,
                        rate,
                        big_endian: !little,
                        unsigned8: false,
                    });
                }
                _ => {}
            }
            pos = body + size + (size & 1);
        }
        Err("No audio data found".into())
    }

    pub fn format_name(&self) -> &'static str {
        match self.container {
            Container::Wav => "WAV",
            Container::Aiff => "AIFF",
            Container::Raw => "RAW",
        }
    }

    pub fn bytes_per_frame(&self) -> u64 {
        u64::from(self.channels) * u64::from(self.bits) / 8
    }

    pub fn frames(&self) -> u64 {
        self.data_len / self.bytes_per_frame()
    }

    pub fn duration_secs(&self) -> f64 {
        self.frames() as f64 / f64::from(self.rate)
    }

    fn sample(&self, b: &[u8]) -> f32 {
        match self.bits {
            8 => {
                if self.unsigned8 {
                    (f32::from(b[0]) - 128.0) / 128.0
                } else {
                    f32::from(b[0] as i8) / 128.0
                }
            }
            16 => {
                let v = if self.big_endian { i16::from_be_bytes([b[0], b[1]]) } else { i16::from_le_bytes([b[0], b[1]]) };
                f32::from(v) / 32768.0
            }
            24 => {
                let raw = if self.big_endian {
                    (i32::from(b[0] as i8) << 16) | (i32::from(b[1]) << 8) | i32::from(b[2])
                } else {
                    (i32::from(b[2] as i8) << 16) | (i32::from(b[1]) << 8) | i32::from(b[0])
                };
                raw as f32 / 8_388_608.0
            }
            _ => {
                let v = if self.big_endian { i32::from_be_bytes([b[0], b[1], b[2], b[3]]) } else { i32::from_le_bytes([b[0], b[1], b[2], b[3]]) };
                v as f32 / 2_147_483_648.0
            }
        }
    }

    /// Read `count` frames starting at `start`, as interleaved samples in -1.0..1.0.
    pub fn read_frames(&self, f: &mut File, start: u64, count: u64) -> std::io::Result<Vec<f32>> {
        let count = count.min(self.frames().saturating_sub(start));
        let bpf = self.bytes_per_frame();
        let mut bytes = vec![0u8; (count * bpf) as usize];
        f.seek(SeekFrom::Start(self.data_offset + start * bpf))?;
        f.read_exact(&mut bytes)?;
        let width = usize::from(self.bits / 8);
        Ok(bytes.chunks_exact(width).map(|b| self.sample(b)).collect())
    }

    fn encode(&self, x: f32, out: &mut Vec<u8>) {
        let x = x.clamp(-1.0, 1.0);
        match self.bits {
            8 => out.push((x * 128.0).round().clamp(-128.0, 127.0) as i16 as i8 as u8 ^ 0x80),
            16 => out.extend_from_slice(&((x * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes()),
            24 => {
                let v = (x * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
                out.extend_from_slice(&v.to_le_bytes()[0..3]);
            }
            _ => out.extend_from_slice(&((f64::from(x) * 2_147_483_648.0).round().clamp(-2_147_483_648.0, 2_147_483_647.0) as i32).to_le_bytes()),
        }
    }
}

/// One min/max pair of the left channel per bucket, for drawing a waveform.
pub fn peaks(pf: &PcmFile, buckets: usize) -> Result<Vec<[f32; 2]>, String> {
    let frames = pf.frames();
    if frames == 0 || buckets == 0 {
        return Ok(Vec::new());
    }
    let mut out = vec![[0.0f32, 0.0f32]; buckets];
    let mut seen = vec![false; buckets];
    let mut f = File::open(&pf.path).map_err(|e| e.to_string())?;
    let ch = usize::from(pf.channels);
    let mut start = 0u64;
    while start < frames {
        let block = pf.read_frames(&mut f, start, BLOCK_FRAMES).map_err(|e| e.to_string())?;
        for (i, frame) in block.chunks_exact(ch).enumerate() {
            let b = (((start + i as u64) as u128 * buckets as u128) / frames as u128) as usize;
            let v = frame[0];
            if !seen[b] {
                out[b] = [v, v];
                seen[b] = true;
            } else {
                out[b][0] = out[b][0].min(v);
                out[b][1] = out[b][1].max(v);
            }
        }
        start += BLOCK_FRAMES;
    }
    Ok(out)
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq)]
pub struct Levels {
    /// Linear peak of the left channel (0.0 to 1.0).
    pub peak: f64,
    /// Linear RMS of the left channel.
    pub rms: f64,
}

fn frame_range(pf: &PcmFile, trim: Option<Trim>) -> Result<(u64, u64), String> {
    let total = pf.frames();
    let Some(t) = trim else { return Ok((0, total)) };
    if !(t.start.is_finite() && t.end.is_finite()) || t.start < 0.0 || t.end <= t.start {
        return Err("Trim end must be after its start".into());
    }
    let rate = f64::from(pf.rate);
    let start = ((t.start * rate).round() as u64).min(total);
    let end = ((t.end * rate).round() as u64).min(total);
    if end <= start {
        return Err("Trim leaves no audio".into());
    }
    Ok((start, end))
}

/// Peak and RMS of the left channel over the trimmed range (or the whole file).
pub fn analyze(pf: &PcmFile, trim: Option<Trim>) -> Result<Levels, String> {
    let (start, end) = frame_range(pf, trim)?;
    let mut f = File::open(&pf.path).map_err(|e| e.to_string())?;
    let ch = usize::from(pf.channels);
    let (mut peak, mut sum_sq, mut n) = (0f64, 0f64, 0u64);
    let mut at = start;
    while at < end {
        let block = pf.read_frames(&mut f, at, BLOCK_FRAMES.min(end - at)).map_err(|e| e.to_string())?;
        for frame in block.chunks_exact(ch) {
            let v = f64::from(frame[0]);
            peak = peak.max(v.abs());
            sum_sq += v * v;
            n += 1;
        }
        at += BLOCK_FRAMES;
    }
    Ok(Levels { peak, rms: if n == 0 { 0.0 } else { (sum_sq / n as f64).sqrt() } })
}

fn db_to_lin(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// Linear gain that `mode` applies to audio with these levels.
pub fn gain_for(levels: Levels, mode: NormMode) -> f64 {
    if levels.peak < 1e-9 {
        return 1.0; // silence: nothing to scale
    }
    let ceiling = db_to_lin(PEAK_CEILING_DB);
    let gain = match mode {
        NormMode::Peak => ceiling / levels.peak,
        NormMode::Rms if levels.rms < 1e-9 => 1.0,
        NormMode::Rms => (db_to_lin(RMS_TARGET_DB) / levels.rms).min(ceiling / levels.peak),
    };
    gain.min(db_to_lin(MAX_GAIN_DB))
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RenderOpts {
    pub trim: Option<Trim>,
    pub normalize: Option<NormMode>,
}

/// Bytes the rendered WAV will occupy.
pub fn rendered_size(pf: &PcmFile, trim: Option<Trim>) -> Result<u64, String> {
    let (start, end) = frame_range(pf, trim)?;
    Ok(WAV_HEADER_BYTES + (end - start) * pf.bytes_per_frame())
}

fn wav_header(pf: &PcmFile, frames: u64) -> [u8; 44] {
    let data = (frames * pf.bytes_per_frame()) as u32;
    let block_align = pf.bytes_per_frame() as u16;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&pf.channels.to_le_bytes());
    h[24..28].copy_from_slice(&pf.rate.to_le_bytes());
    h[28..32].copy_from_slice(&(pf.rate * u32::from(block_align)).to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&pf.bits.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

/// Write a trimmed and/or normalized copy of `pf` to `dest` as a PCM WAV.
///
/// `on_chunk(bytes_written)` runs after each block; returning `false` cancels, which removes the
/// partial file and returns `Interrupted`.
pub fn render(pf: &PcmFile, dest: &Path, opts: RenderOpts, mut on_chunk: impl FnMut(u64) -> bool) -> std::io::Result<()> {
    let other = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, m);
    let (start, end) = frame_range(pf, opts.trim).map_err(other)?;
    let gain = match opts.normalize {
        Some(mode) => gain_for(analyze(pf, opts.trim).map_err(other)?, mode) as f32,
        None => 1.0,
    };

    let mut src = File::open(&pf.path)?;
    let mut out = File::create(dest)?;
    out.write_all(&wav_header(pf, end - start))?;
    let mut written = WAV_HEADER_BYTES;
    let mut at = start;
    let mut buf = Vec::new();
    while at < end {
        let block = pf.read_frames(&mut src, at, BLOCK_FRAMES.min(end - at))?;
        buf.clear();
        for &x in &block {
            pf.encode(x * gain, &mut buf);
        }
        out.write_all(&buf)?;
        written += buf.len() as u64;
        at += BLOCK_FRAMES;
        if !on_chunk(written) {
            drop(out);
            let _ = std::fs::remove_file(dest);
            return Err(std::io::ErrorKind::Interrupted.into());
        }
    }
    out.flush()
}

/// A 16-bit mono WAV of the left channel from `start_secs`, at most `max_secs` long, for playback
/// in the webview (which can't read RAW files and would need whole large files in memory).
pub fn preview_wav(pf: &PcmFile, start_secs: f64, max_secs: f64) -> Result<Vec<u8>, String> {
    let rate = f64::from(pf.rate);
    let start = ((start_secs.max(0.0) * rate) as u64).min(pf.frames());
    let count = ((max_secs.max(0.0) * rate) as u64).min(pf.frames() - start);
    let mut f = File::open(&pf.path).map_err(|e| e.to_string())?;
    let ch = usize::from(pf.channels);

    let mut pcm = Vec::with_capacity(count as usize * 2);
    let mut at = start;
    while at < start + count {
        let block = pf.read_frames(&mut f, at, BLOCK_FRAMES.min(start + count - at)).map_err(|e| e.to_string())?;
        for frame in block.chunks_exact(ch) {
            pcm.extend_from_slice(&((frame[0].clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
        }
        at += BLOCK_FRAMES;
    }

    let mono16 = PcmFile { channels: 1, bits: 16, ..pf.clone() };
    let mut out = wav_header(&mono16, pcm.len() as u64 / 2).to_vec();
    out.extend_from_slice(&pcm);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rmm-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// 16-bit little-endian PCM WAV with the given interleaved samples.
    fn wav16(channels: u16, rate: u32, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        v.extend_from_slice(&(channels * 2).to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&data);
        v
    }

    /// 80-bit extended float for a whole-number rate.
    fn extended(rate: u32) -> [u8; 10] {
        let exp = 31 - rate.leading_zeros() as u16; // floor(log2(rate))
        let mant = u64::from(rate) << (63 - exp);
        let mut b = [0u8; 10];
        b[0..2].copy_from_slice(&(16383 + exp).to_be_bytes());
        b[2..10].copy_from_slice(&mant.to_be_bytes());
        b
    }

    fn aiff16(channels: u16, rate: u32, samples: &[i16], aifc: Option<&[u8; 4]>) -> Vec<u8> {
        let data: Vec<u8> = samples
            .iter()
            .flat_map(|s| if aifc == Some(b"sowt") { s.to_le_bytes() } else { s.to_be_bytes() })
            .collect();
        let frames = samples.len() as u32 / u32::from(channels);
        let mut comm = Vec::new();
        comm.extend_from_slice(&channels.to_be_bytes());
        comm.extend_from_slice(&frames.to_be_bytes());
        comm.extend_from_slice(&16u16.to_be_bytes());
        comm.extend_from_slice(&extended(rate));
        if let Some(kind) = aifc {
            comm.extend_from_slice(kind);
            comm.extend_from_slice(&[0, 0]); // empty pascal string, padded
        }
        let mut body = Vec::new();
        body.extend_from_slice(if aifc.is_some() { b"AIFC" } else { b"AIFF" });
        body.extend_from_slice(b"COMM");
        body.extend_from_slice(&(comm.len() as u32).to_be_bytes());
        body.extend_from_slice(&comm);
        body.extend_from_slice(b"SSND");
        body.extend_from_slice(&(8 + data.len() as u32).to_be_bytes());
        body.extend_from_slice(&[0u8; 8]);
        body.extend_from_slice(&data);
        let mut v = b"FORM".to_vec();
        v.extend_from_slice(&(body.len() as u32).to_be_bytes());
        v.extend_from_slice(&body);
        v
    }

    fn write(name: &str, bytes: &[u8]) -> PathBuf {
        let p = tmp(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn all_samples(pf: &PcmFile) -> Vec<f32> {
        let mut f = File::open(&pf.path).unwrap();
        pf.read_frames(&mut f, 0, pf.frames()).unwrap()
    }

    #[test]
    fn extended_float_round_trips_common_rates() {
        for rate in [8000, 22050, 44100, 48000, 96000] {
            assert_eq!(extended_to_f64(&extended(rate)).round() as u32, rate);
        }
    }

    #[test]
    fn reads_a_stereo_wav() {
        let p = write("stereo.wav", &wav16(2, 44100, &[1000, -1000, 2000, -2000, 3000, -3000]));
        let pf = PcmFile::open(&p).unwrap();
        assert_eq!((pf.channels, pf.bits, pf.rate, pf.frames()), (2, 16, 44100, 3));
        let s = all_samples(&pf);
        assert!((s[0] - 1000.0 / 32768.0).abs() < 1e-6 && (s[1] + 1000.0 / 32768.0).abs() < 1e-6);
        assert!((pf.duration_secs() - 3.0 / 44100.0).abs() < 1e-9);
    }

    #[test]
    fn skips_unrelated_chunks_and_odd_padding_before_data() {
        let mut v = wav16(1, 8000, &[100, 200, 300, 400]);
        // Insert an odd-sized LIST chunk (plus its pad byte) between fmt and data.
        let at = v.windows(4).position(|w| w == b"data").unwrap();
        let mut list = b"LIST".to_vec();
        list.extend_from_slice(&3u32.to_le_bytes());
        list.extend_from_slice(&[1, 2, 3, 0]);
        v.splice(at..at, list);
        let pf = PcmFile::open(&write("list.wav", &v)).unwrap();
        assert_eq!(pf.frames(), 4);
        assert!((all_samples(&pf)[3] - 400.0 / 32768.0).abs() < 1e-6);
    }

    #[test]
    fn clamps_a_streaming_wav_whose_data_size_is_wrong() {
        let mut v = wav16(1, 8000, &[1, 2, 3, 4]);
        let at = v.windows(4).position(|w| w == b"data").unwrap() + 4;
        v[at..at + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert_eq!(PcmFile::open(&write("stream.wav", &v)).unwrap().frames(), 4);
    }

    #[test]
    fn reads_aiff_and_aifc_sowt() {
        let samples = [1000i16, -2000, 3000, -4000];
        let a = PcmFile::open(&write("a.aif", &aiff16(2, 22050, &samples, None))).unwrap();
        assert_eq!((a.channels, a.rate, a.frames(), a.container), (2, 22050, 2, Container::Aiff));
        assert!((all_samples(&a)[1] + 2000.0 / 32768.0).abs() < 1e-6, "big-endian samples");

        let c = PcmFile::open(&write("c.aifc", &aiff16(2, 48000, &samples, Some(b"sowt")))).unwrap();
        assert_eq!(c.rate, 48000);
        assert!((all_samples(&c)[2] - 3000.0 / 32768.0).abs() < 1e-6, "little-endian AIFC");
    }

    #[test]
    fn rejects_what_it_cannot_play() {
        assert!(PcmFile::open(&write("x.wav", b"not audio at all")).is_err());
        let mut fl = wav16(1, 8000, &[0, 0]);
        let at = fl.windows(4).position(|w| w == b"fmt ").unwrap() + 8;
        fl[at..at + 2].copy_from_slice(&3u16.to_le_bytes());
        assert!(PcmFile::open(&write("float.wav", &fl)).unwrap_err().contains("Floating"));
    }

    #[test]
    fn raw_is_44k_16bit_mono() {
        let p = write("a.raw", &[0u8; 8820]);
        let pf = PcmFile::open(&p).unwrap();
        assert_eq!((pf.channels, pf.bits, pf.rate, pf.frames()), (1, 16, 44100, 4410));
    }

    #[test]
    fn eight_bit_wav_is_unsigned_and_roundtrips() {
        let mut v = wav16(1, 8000, &[0, 0]);
        // Rewrite as 8-bit: fmt bits/blockalign/byterate, data bytes.
        let at = v.windows(4).position(|w| w == b"fmt ").unwrap() + 8;
        v[at + 12..at + 14].copy_from_slice(&1u16.to_le_bytes());
        v[at + 14..at + 16].copy_from_slice(&8u16.to_le_bytes());
        let d = v.windows(4).position(|w| w == b"data").unwrap();
        v.truncate(d);
        v.extend_from_slice(b"data");
        v.extend_from_slice(&3u32.to_le_bytes());
        v.extend_from_slice(&[128, 255, 0]);
        let pf = PcmFile::open(&write("e.wav", &v)).unwrap();
        let s = all_samples(&pf);
        assert!(s[0].abs() < 1e-6 && s[1] > 0.99 && s[2] < -0.99);
        let mut out = Vec::new();
        pf.encode(s[1], &mut out);
        pf.encode(s[2], &mut out);
        pf.encode(0.0, &mut out);
        assert_eq!(out, vec![255, 0, 128]);
    }

    #[test]
    fn peaks_use_the_left_channel_in_even_buckets() {
        // Left is a ramp; right is loud noise that must be ignored.
        let mut s = Vec::new();
        for i in 0..1000i16 {
            s.push(i * 10);
            s.push(32000);
        }
        let pf = PcmFile::open(&write("ramp.wav", &wav16(2, 8000, &s))).unwrap();
        let p = peaks(&pf, 10).unwrap();
        assert_eq!(p.len(), 10);
        assert!(p.iter().all(|b| b[1] < 0.31), "right channel is ignored: {p:?}");
        assert!(p[9][1] > 0.29 && p[0][1] < 0.05, "ramp rises across buckets");
        assert!(peaks(&pf, 0).unwrap().is_empty());
    }

    #[test]
    fn preview_is_mono_left_16bit_and_windowed() {
        let mut s = Vec::new();
        for i in 0..8000i16 {
            s.push(i);
            s.push(-1);
        }
        let pf = PcmFile::open(&write("prev.wav", &wav16(2, 8000, &s))).unwrap();
        let wav = preview_wav(&pf, 0.5, 0.25).unwrap();
        let prev = PcmFile::open(&write("prev-out.wav", &wav)).unwrap();
        assert_eq!((prev.channels, prev.bits, prev.rate), (1, 16, 8000));
        assert_eq!(prev.frames(), 2000, "0.25 s at 8 kHz");
        assert!((all_samples(&prev)[0] - 4000.0 / 32768.0).abs() < 1e-3, "starts 0.5 s in, left channel");
        // A window past the end is clamped, not an error.
        assert_eq!(PcmFile::open(&write("prev2.wav", &preview_wav(&pf, 0.9, 5.0).unwrap())).unwrap().frames(), 800);
    }

    fn sine(amp: f32, n: usize) -> Vec<i16> {
        (0..n).map(|i| ((i as f32 * 0.05).sin() * amp * 32767.0) as i16).collect()
    }

    #[test]
    fn analyze_measures_peak_and_rms() {
        let pf = PcmFile::open(&write("sine.wav", &wav16(1, 8000, &sine(0.5, 8000)))).unwrap();
        let l = analyze(&pf, None).unwrap();
        assert!((l.peak - 0.5).abs() < 0.01, "{l:?}");
        assert!((l.rms - 0.5 / 2f64.sqrt()).abs() < 0.01, "{l:?}");
        // Only the trimmed range is measured.
        let silent_then_loud: Vec<i16> = std::iter::repeat(0).take(4000).chain(sine(0.8, 4000)).collect();
        let pf2 = PcmFile::open(&write("half.wav", &wav16(1, 8000, &silent_then_loud))).unwrap();
        assert!(analyze(&pf2, Some(Trim { start: 0.0, end: 0.4 })).unwrap().peak < 1e-4);
        assert!(analyze(&pf2, Some(Trim { start: 0.5, end: 1.0 })).unwrap().peak > 0.7);
    }

    #[test]
    fn gain_rules() {
        let quiet = Levels { peak: 0.1, rms: 0.05 };
        let ceiling = db_to_lin(PEAK_CEILING_DB);
        assert!((gain_for(quiet, NormMode::Peak) * 0.1 - ceiling).abs() < 1e-9, "peak mode lands on -1 dBFS");
        // RMS mode would want 0.126/0.05 = 2.5x, but the peak ceiling (8.9x) allows it.
        assert!((gain_for(quiet, NormMode::Rms) - db_to_lin(RMS_TARGET_DB) / 0.05).abs() < 1e-9);
        // A spiky file is held back by the peak ceiling.
        let spiky = Levels { peak: 0.9, rms: 0.01 };
        assert!((gain_for(spiky, NormMode::Rms) * 0.9 - ceiling).abs() < 1e-9);
        assert_eq!(gain_for(Levels { peak: 0.0, rms: 0.0 }, NormMode::Peak), 1.0, "silence is left alone");
        assert!(gain_for(Levels { peak: 1e-6, rms: 1e-6 }, NormMode::Peak) <= db_to_lin(MAX_GAIN_DB) + 1e-9, "boost is capped");
    }

    #[test]
    fn render_trims_normalizes_and_matches_its_size_estimate() {
        let pf = PcmFile::open(&write("quiet.wav", &wav16(2, 8000, &sine(0.2, 32000)))).unwrap();
        let out = tmp("rendered.wav");
        let trim = Trim { start: 0.5, end: 1.5 };
        render(&pf, &out, RenderOpts { trim: Some(trim), normalize: Some(NormMode::Peak) }, |_| true).unwrap();

        let r = PcmFile::open(&out).unwrap();
        assert_eq!((r.channels, r.bits, r.rate), (2, 16, 8000), "format is preserved");
        assert_eq!(r.frames(), 8000, "one second trimmed out of two");
        assert_eq!(std::fs::metadata(&out).unwrap().len(), rendered_size(&pf, Some(trim)).unwrap());
        let level = analyze(&r, None).unwrap();
        assert!((level.peak - db_to_lin(PEAK_CEILING_DB)).abs() < 0.01, "normalized to -1 dBFS: {level:?}");
    }

    #[test]
    fn render_without_normalize_keeps_the_audio_unchanged() {
        let samples = sine(0.3, 4000);
        let pf = PcmFile::open(&write("plain.wav", &wav16(1, 8000, &samples))).unwrap();
        let out = tmp("plain-out.wav");
        render(&pf, &out, RenderOpts { trim: Some(Trim { start: 0.0, end: 0.25 }), normalize: None }, |_| true).unwrap();
        let r = PcmFile::open(&out).unwrap();
        assert_eq!(r.frames(), 2000);
        let (a, b) = (all_samples(&pf), all_samples(&r));
        assert!(a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1.0 / 32768.0), "samples are bit-identical after a trim");
    }

    #[test]
    fn render_converts_aiff_and_raw_to_wav() {
        let a = PcmFile::open(&write("conv.aif", &aiff16(1, 8000, &sine(0.4, 800), None))).unwrap();
        let out = tmp("conv.wav");
        render(&a, &out, RenderOpts { trim: None, normalize: Some(NormMode::Peak) }, |_| true).unwrap();
        assert_eq!(PcmFile::open(&out).unwrap().container, Container::Wav);

        let raw: Vec<u8> = sine(0.4, 800).iter().flat_map(|s| s.to_le_bytes()).collect();
        let r = PcmFile::open(&write("conv.raw", &raw)).unwrap();
        let out2 = tmp("conv2.wav");
        render(&r, &out2, RenderOpts { trim: Some(Trim { start: 0.0, end: 0.01 }), normalize: None }, |_| true).unwrap();
        let w = PcmFile::open(&out2).unwrap();
        assert_eq!((w.rate, w.channels, w.bits), (44100, 1, 16));
        assert_eq!(w.frames(), 441);
    }

    #[test]
    fn render_cancel_removes_the_partial_file() {
        let pf = PcmFile::open(&write("big.wav", &wav16(1, 8000, &vec![5i16; 200_000]))).unwrap();
        let out = tmp("cancelled.wav");
        let err = render(&pf, &out, RenderOpts { trim: Some(Trim { start: 0.0, end: 20.0 }), normalize: None }, |n| n < 70_000).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
        assert!(!out.exists());
    }

    #[test]
    fn bad_trims_are_rejected() {
        let pf = PcmFile::open(&write("t.wav", &wav16(1, 8000, &sine(0.3, 8000)))).unwrap();
        assert!(frame_range(&pf, Some(Trim { start: 0.5, end: 0.5 })).is_err());
        assert!(frame_range(&pf, Some(Trim { start: 0.9, end: 0.1 })).is_err());
        assert!(frame_range(&pf, Some(Trim { start: -1.0, end: 1.0 })).is_err());
        assert!(frame_range(&pf, Some(Trim { start: 5.0, end: 9.0 })).is_err(), "entirely past the end");
        // An end past the file is clamped.
        assert_eq!(frame_range(&pf, Some(Trim { start: 0.5, end: 9.0 })).unwrap(), (4000, 8000));
    }
}
