//! A neural speaker encoder: GE2E (Wan et al., 2018), with the pretrained
//! weights published as Resemblyzer (Apache-2.0). Speech becomes a
//! 256-number voice embedding; two utterances by the same person point the
//! same way, whatever was said and whatever the microphone.
//!
//! Everything runs here, in plain Rust: a 40-band mel spectrogram (as
//! librosa computes it, which is what the model was trained on), three
//! LSTM layers, a linear layer. The weights are downloaded once by
//! `orion voice model install` and converted to a small file of our own.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};

const RATE: u32 = 16_000;
const NFFT: usize = 400; // 25 ms
const HOP: usize = 160; // 10 ms
const MELS: usize = 40;
/// Frames per partial utterance (1.6 s); an utterance is the average of its
/// partials.
const PARTIAL: usize = 160;
/// Less speech than this (0.5 s) is too little to judge.
const MIN_SAMPLES: usize = RATE as usize / 2;

/// Where the weights come from: a pinned release on PyPI.
pub const WHEEL_URL: &str = "https://files.pythonhosted.org/packages/d9/b5/d897d5de5b2123a1ad5d32e8e007d83f76582f5c32c60992577263bd7c7e/Resemblyzer-0.1.4-py3-none-any.whl";
pub const WHEEL_SHA256: &str = "8f12eb2f1a9982d32e8db7856de754709b59c93a77bcf0ff536584b619a9dd1f";
pub const MODEL_NAME: &str = "ge2e-resemblyzer-0.1.4";
const FILE_MAGIC: &[u8; 8] = b"ORGE2E01";

struct Layer {
    /// (4·hidden) × input, gates in PyTorch's order: input, forget, cell, output.
    w_ih: Vec<f32>,
    w_hh: Vec<f32>,
    /// Both biases, summed.
    bias: Vec<f32>,
    input: usize,
}

pub struct Ge2e {
    layers: Vec<Layer>,
    hidden: usize,
    linear_w: Vec<f32>,
    linear_b: Vec<f32>,
    embedding: usize,
    mel: Vec<(usize, Vec<f32>)>,
    window: Vec<f32>,
    cos: Vec<f32>,
    sin: Vec<f32>,
}

/// Named weights, as read from a file.
pub type Weights = std::collections::BTreeMap<String, (Vec<usize>, Vec<f32>)>;

impl Ge2e {
    pub fn default_path() -> Option<PathBuf> {
        let data = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
        Some(data.join("orion/models/ge2e.bin"))
    }

    pub fn from_weights(w: &Weights) -> Result<Self> {
        let get = |k: &str| w.get(k).with_context(|| format!("the model has no {k}"));
        let (hh_shape, _) = get("lstm.weight_hh_l0")?;
        ensure!(hh_shape.len() == 2 && hh_shape[0] == 4 * hh_shape[1], "unexpected LSTM shape");
        let hidden = hh_shape[1];
        let mut layers = Vec::new();
        for l in 0.. {
            let Some((ih_shape, w_ih)) = w.get(&format!("lstm.weight_ih_l{l}")) else { break };
            let (_, w_hh) = get(&format!("lstm.weight_hh_l{l}"))?;
            let (_, b_ih) = get(&format!("lstm.bias_ih_l{l}"))?;
            let (_, b_hh) = get(&format!("lstm.bias_hh_l{l}"))?;
            ensure!(ih_shape[0] == 4 * hidden && w_hh.len() == 4 * hidden * hidden, "layer {l} has the wrong size");
            ensure!(ih_shape[1] == if l == 0 { MELS } else { hidden }, "layer {l} takes the wrong input");
            layers.push(Layer {
                w_ih: w_ih.clone(),
                w_hh: w_hh.clone(),
                bias: b_ih.iter().zip(b_hh).map(|(a, b)| a + b).collect(),
                input: ih_shape[1],
            });
        }
        ensure!(!layers.is_empty(), "the model has no LSTM layers");
        let (lin_shape, linear_w) = get("linear.weight")?;
        let (_, linear_b) = get("linear.bias")?;
        ensure!(lin_shape.len() == 2 && lin_shape[1] == hidden, "unexpected linear layer");
        let tau = std::f64::consts::TAU;
        let bins = NFFT / 2 + 1;
        let mut cos = Vec::with_capacity(bins * NFFT);
        let mut sin = Vec::with_capacity(bins * NFFT);
        for k in 0..bins {
            for n in 0..NFFT {
                let a = tau * (k * n % NFFT) as f64 / NFFT as f64;
                cos.push(a.cos() as f32);
                sin.push(a.sin() as f32);
            }
        }
        Ok(Self {
            layers,
            hidden,
            linear_w: linear_w.clone(),
            linear_b: linear_b.clone(),
            embedding: lin_shape[0],
            mel: slaney_mel(),
            // Periodic Hann, as librosa uses.
            window: (0..NFFT).map(|i| (0.5 - 0.5 * (tau * i as f64 / NFFT as f64).cos()) as f32).collect(),
            cos,
            sin,
        })
    }

    /// The model in Orion's own file format.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_weights(&read_weights(&bytes)?)
    }

    /// The power mel spectrogram, frames × 40, centered with reflection
    /// padding.
    pub fn mel(&self, x: &[f32]) -> Vec<[f32; MELS]> {
        let pad = NFFT / 2;
        if x.len() <= pad {
            return Vec::new();
        }
        let at = |i: isize| -> f32 {
            // Reflect without repeating the edge sample.
            let n = x.len() as isize;
            let j = if i < 0 { -i } else if i >= n { 2 * (n - 1) - i } else { i };
            x[j.clamp(0, n - 1) as usize]
        };
        let frames = 1 + x.len() / HOP;
        let bins = NFFT / 2 + 1;
        let mut out = Vec::with_capacity(frames);
        let mut buf = [0.0f32; NFFT];
        let mut power = vec![0.0f32; bins];
        for f in 0..frames {
            let start = (f * HOP) as isize - pad as isize;
            for (n, b) in buf.iter_mut().enumerate() {
                *b = at(start + n as isize) * self.window[n];
            }
            for (k, p) in power.iter_mut().enumerate() {
                let (c, s) = (&self.cos[k * NFFT..(k + 1) * NFFT], &self.sin[k * NFFT..(k + 1) * NFFT]);
                let (mut re, mut im) = (0.0f32, 0.0f32);
                for n in 0..NFFT {
                    re += buf[n] * c[n];
                    im += buf[n] * s[n];
                }
                *p = re * re + im * im;
            }
            let mut m = [0.0f32; MELS];
            for (mi, (first, w)) in self.mel.iter().enumerate() {
                m[mi] = w.iter().enumerate().map(|(i, wt)| wt * power[first + i]).sum();
            }
            out.push(m);
        }
        out
    }

    /// The embedding of one partial: the last hidden state, through the
    /// linear layer, ReLU, and L2-normalized.
    fn forward(&self, mels: &[[f32; MELS]]) -> Vec<f32> {
        let h4 = 4 * self.hidden;
        let mut input: Vec<Vec<f32>> = mels.iter().map(|m| m.to_vec()).collect();
        let mut gates = vec![0.0f32; h4];
        for layer in &self.layers {
            let mut h = vec![0.0f32; self.hidden];
            let mut c = vec![0.0f32; self.hidden];
            let mut outputs = Vec::with_capacity(input.len());
            for x in &input {
                gates.copy_from_slice(&layer.bias);
                matvec_add(&layer.w_ih, x, &mut gates, layer.input);
                matvec_add(&layer.w_hh, &h, &mut gates, self.hidden);
                let n = self.hidden;
                for j in 0..n {
                    let i = sigmoid(gates[j]);
                    let f = sigmoid(gates[n + j]);
                    let g = gates[2 * n + j].tanh();
                    let o = sigmoid(gates[3 * n + j]);
                    c[j] = f * c[j] + i * g;
                    h[j] = o * c[j].tanh();
                }
                outputs.push(h.clone());
            }
            input = outputs;
        }
        let last = input.last().cloned().unwrap_or_else(|| vec![0.0; self.hidden]);
        let mut e = self.linear_b.clone();
        matvec_add(&self.linear_w, &last, &mut e, self.hidden);
        e.iter_mut().for_each(|v| *v = v.max(0.0));
        normalize(&mut e);
        e
    }

    /// The voice embedding of an utterance (16-bit mono at `rate`), or
    /// `None` if it has too little sound to judge.
    pub fn embed(&self, samples: &[i16], rate: u32) -> Option<Vec<f32>> {
        let mut x = super::speaker::resample(samples, rate);
        trim_silence(&mut x);
        if x.len() < MIN_SAMPLES {
            return None;
        }
        // Volume up to -30 dBFS, never down (as in training).
        let rms = (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        let gain = if rms > 0.0 { (10f32.powf(-30.0 / 20.0) * 32767.0 / rms).max(1.0) } else { 1.0 };
        let mut wav: Vec<f32> = x.iter().map(|v| v * gain / 32767.0).collect();
        // Partials 1.6 s long, 1.3 per second; the last kept if 75% covered.
        let n = wav.len();
        let n_frames = (n + 1).div_ceil(HOP);
        let step = ((RATE as f32 / 1.3) / HOP as f32).round() as usize;
        let mut slices: Vec<usize> = (0..(n_frames + step + 1).saturating_sub(PARTIAL).max(1)).step_by(step).collect();
        if let Some(&last) = slices.last() {
            let coverage = (n as f32 - (last * HOP) as f32) / (PARTIAL * HOP) as f32;
            if coverage < 0.75 && slices.len() > 1 {
                slices.pop();
            }
        }
        let need = (slices.last().copied().unwrap_or(0) + PARTIAL) * HOP;
        if need >= wav.len() {
            wav.resize(need, 0.0);
        }
        let mel = self.mel(&wav);
        let parts: Vec<Vec<f32>> = std::thread::scope(|s| {
            let handles: Vec<_> = slices
                .iter()
                .map(|&start| {
                    let end = (start + PARTIAL).min(mel.len());
                    let m = &mel[start..end];
                    s.spawn(move || self.forward(m))
                })
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        let mut e = vec![0.0f32; self.embedding];
        for p in &parts {
            e.iter_mut().zip(p).for_each(|(a, b)| *a += b);
        }
        normalize(&mut e);
        Some(e)
    }
}

/// Cut quiet lead-in and tail (the encoder was trained on voice-trimmed
/// audio): everything before the first and after the last 10 ms that is
/// within 30 dB of the loudest.
fn trim_silence(x: &mut Vec<f32>) {
    let frame = HOP;
    let energies: Vec<f32> = x.chunks(frame).map(|c| c.iter().map(|v| v * v).sum::<f32>() / c.len() as f32).collect();
    let peak = energies.iter().copied().fold(0.0f32, f32::max);
    if peak <= 0.0 {
        x.clear();
        return;
    }
    let loud = |e: &f32| *e > peak * 1e-3;
    let (Some(first), Some(last)) = (energies.iter().position(loud), energies.iter().rposition(loud)) else { return };
    let end = ((last + 1) * frame).min(x.len());
    x.truncate(end);
    x.drain(..first * frame);
}

fn sigmoid(v: f32) -> f32 {
    1.0 / (1.0 + (-v).exp())
}

fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

/// `out += m · x`, with `m` row-major and `cols` wide.
fn matvec_add(m: &[f32], x: &[f32], out: &mut [f32], cols: usize) {
    for (o, row) in out.iter_mut().zip(m.chunks_exact(cols)) {
        let mut acc = [0.0f32; 8];
        let (rc, xc) = (row.chunks_exact(8), x.chunks_exact(8));
        let (rr, xr) = (rc.remainder(), xc.remainder());
        for (r, v) in rc.zip(xc) {
            for k in 0..8 {
                acc[k] += r[k] * v[k];
            }
        }
        *o += acc.iter().sum::<f32>() + rr.iter().zip(xr).map(|(a, b)| a * b).sum::<f32>();
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb).max(1e-9)
}

/// librosa's mel filters (Slaney scale and area normalization) for 16 kHz,
/// a 400-point FFT and 40 bands: (first bin, weights).
fn slaney_mel() -> Vec<(usize, Vec<f32>)> {
    let f_sp = 200.0 / 3.0;
    let log_step = (6.4f64).ln() / 27.0;
    let to_mel = |f: f64| if f >= 1000.0 { 15.0 + (f / 1000.0).ln() / log_step } else { f / f_sp };
    let to_hz = |m: f64| if m >= 15.0 { 1000.0 * (log_step * (m - 15.0)).exp() } else { m * f_sp };
    let (lo, hi) = (to_mel(0.0), to_mel(RATE as f64 / 2.0));
    let pts: Vec<f64> = (0..MELS + 2).map(|i| to_hz(lo + (hi - lo) * i as f64 / (MELS + 1) as f64)).collect();
    let bins = NFFT / 2 + 1;
    let freq = |k: usize| k as f64 * RATE as f64 / NFFT as f64;
    (0..MELS)
        .map(|i| {
            let enorm = 2.0 / (pts[i + 2] - pts[i]);
            let w: Vec<f32> = (0..bins)
                .map(|k| {
                    let lower = (freq(k) - pts[i]) / (pts[i + 1] - pts[i]);
                    let upper = (pts[i + 2] - freq(k)) / (pts[i + 2] - pts[i + 1]);
                    (lower.min(upper).max(0.0) * enorm) as f32
                })
                .collect();
            let first = w.iter().position(|v| *v > 0.0).unwrap_or(0);
            let last = w.iter().rposition(|v| *v > 0.0).unwrap_or(first);
            (first, w[first..=last].to_vec())
        })
        .collect()
}

/// Orion's weight file: magic, then per tensor its name, shape and floats.
pub fn write_weights(w: &Weights) -> Vec<u8> {
    let mut out = FILE_MAGIC.to_vec();
    out.extend((w.len() as u32).to_le_bytes());
    for (name, (shape, data)) in w {
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(name.as_bytes());
        out.push(shape.len() as u8);
        for d in shape {
            out.extend((*d as u32).to_le_bytes());
        }
        for v in data {
            out.extend(v.to_le_bytes());
        }
    }
    out
}

pub fn read_weights(bytes: &[u8]) -> Result<Weights> {
    let mut r = bytes;
    let mut take = |n: usize| -> Result<&[u8]> {
        ensure!(r.len() >= n, "the model file is cut short");
        let (a, b) = r.split_at(n);
        r = b;
        Ok(a)
    };
    ensure!(take(8)? == FILE_MAGIC, "not an Orion speaker model; run `orion voice model install` again");
    let count = u32::from_le_bytes(take(4)?.try_into()?);
    let mut w = Weights::new();
    for _ in 0..count {
        let len = u16::from_le_bytes(take(2)?.try_into()?) as usize;
        let name = String::from_utf8(take(len)?.to_vec())?;
        let ndim = take(1)?[0] as usize;
        let shape: Vec<usize> = (0..ndim).map(|_| take(4).map(|b| u32::from_le_bytes(b.try_into().unwrap_or_default()) as usize)).collect::<Result<_>>()?;
        let n: usize = shape.iter().product();
        let data: Vec<f32> = take(n * 4)?.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        w.insert(name, (shape, data));
    }
    Ok(w)
}

/// The encoder's weights out of the Resemblyzer wheel (or its
/// `pretrained.pt`), checked and converted.
pub fn convert(bytes: &[u8]) -> Result<Weights> {
    let pt = if bytes.starts_with(b"PK") {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("reading the wheel")?;
        let mut f = zip.by_name("resemblyzer/pretrained.pt").context("the wheel has no resemblyzer/pretrained.pt")?;
        let mut pt = Vec::new();
        f.read_to_end(&mut pt)?;
        pt
    } else {
        bytes.to_vec()
    };
    let tensors = super::torch::load(&pt, "model_state")?;
    let w: Weights = tensors
        .into_iter()
        .filter(|(k, _)| k.starts_with("lstm.") || k.starts_with("linear."))
        .map(|(k, t)| (k, (t.shape, t.data)))
        .collect();
    Ge2e::from_weights(&w).context("these aren't the speaker encoder's weights")?;
    Ok(w)
}

/// Download the pinned wheel, verify it, and save the converted model.
pub fn install(from: Option<&Path>) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let bytes = match from {
        Some(p) => std::fs::read(p).with_context(|| format!("reading {}", p.display()))?,
        None => {
            let agent = ureq::AgentBuilder::new()
                .try_proxy_from_env(!crate::chat::http::bypass_proxy(WHEEL_URL))
                .timeout_connect(std::time::Duration::from_secs(30))
                .build();
            let resp = agent.get(WHEEL_URL).call().context("downloading the speaker model from PyPI")?;
            let mut b = Vec::new();
            resp.into_reader().take(64 << 20).read_to_end(&mut b)?;
            let digest: String = Sha256::digest(&b).iter().map(|x| format!("{x:02x}")).collect();
            if digest != WHEEL_SHA256 {
                bail!("the download doesn't match the pinned checksum (got {digest}); not installing it");
            }
            b
        }
    };
    let weights = convert(&bytes)?;
    let path = Ge2e::default_path().context("no data directory (is HOME set?)")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, write_weights(&weights))?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("testdata/ge2e.json")).unwrap()
    }

    fn floats(v: &Value) -> Vec<f32> {
        v.as_array().unwrap().iter().map(|x| x.as_f64().unwrap() as f32).collect()
    }

    /// The fixture signal, as the Python reference made it.
    fn signal(n: usize) -> Vec<f32> {
        (0..n)
            .map(|t| {
                let t = t as f64;
                (0.3 * (std::f64::consts::TAU * 440.0 * t / 16000.0).sin()
                    + 0.2 * (std::f64::consts::TAU * 1234.5 * t / 16000.0 + 0.7).sin()
                    + 0.05 * (t * t * 1e-4).sin()) as f32
            })
            .collect()
    }

    fn tiny() -> Ge2e {
        let f = fixture();
        let w: Weights = f["tiny_weights"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                let shape = v["shape"].as_array().unwrap().iter().map(|d| d.as_u64().unwrap() as usize).collect();
                (k.clone(), (shape, floats(&v["data"])))
            })
            .collect();
        Ge2e::from_weights(&w).unwrap()
    }

    #[test]
    fn mel_matches_librosa() {
        let f = fixture();
        let g = tiny();
        let mel = g.mel(&signal(16000));
        assert_eq!(mel.len(), f["mel_n_frames"].as_u64().unwrap() as usize);
        for (i, frame) in [0, 2, 5, 50].into_iter().zip(f["mel_frames_0_2_5_50"].as_array().unwrap()) {
            let want = floats(frame);
            let peak = want.iter().copied().fold(0.0f32, f32::max);
            for (a, b) in mel[i].iter().zip(&want) {
                assert!((a - b).abs() <= 1e-3 * peak, "frame {i}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn lstm_matches_the_reference() {
        let f = fixture();
        let input: Vec<[f32; MELS]> = f["tiny_input"].as_array().unwrap().iter().map(|r| floats(r).try_into().unwrap()).collect();
        let got = tiny().forward(&input);
        for (a, b) in got.iter().zip(floats(&f["tiny_embedding"])) {
            assert!((a - b).abs() < 1e-4, "{got:?}");
        }
    }

    #[test]
    fn weights_round_trip() {
        let mut w = Weights::new();
        w.insert("a".into(), (vec![2, 2], vec![1.0, 2.0, 3.0, 4.0]));
        let back = read_weights(&write_weights(&w)).unwrap();
        assert_eq!(back["a"], (vec![2, 2], vec![1.0, 2.0, 3.0, 4.0]));
        assert!(read_weights(b"ORGE2E01\x01\0\0\0").is_err());
    }

    /// With the real model installed: the full pipeline against the Python
    /// reference's embedding of the same signal.
    #[test]
    fn full_model_matches_the_reference() {
        let Some(path) = std::env::var_os("ORION_GE2E").map(PathBuf::from).or_else(Ge2e::default_path).filter(|p| p.exists()) else {
            return;
        };
        let g = Ge2e::load(&path).unwrap();
        let x: Vec<i16> = signal(48000).iter().map(|v| (v * 32767.0) as i16).collect();
        let got = g.embed(&x, 16000).unwrap();
        let want = floats(&fixture()["full_embedding_3s"]);
        let sim = cosine(&got, &want);
        assert!(sim > 0.999, "similarity to the reference {sim}");
    }
}
