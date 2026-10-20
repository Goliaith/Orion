//! Speaker verification: only the enrolled voice can wake Orion.
//!
//! Two methods. With the neural speaker model installed (`orion voice model
//! install`, see `neural.rs`), each utterance becomes a voice embedding,
//! compared with the average of the enrollment phrases' embeddings. That
//! tells people apart, even on the same microphone.
//!
//! Without it, a built-in method that needs no model. Each 10 ms frame of speech
//! becomes 19 mel-cepstral coefficients plus how they change. The frames of
//! a few enrollment phrases are summarized as a codebook of 32 typical
//! frames (k-means). A new utterance is scored by how far its frames lie
//! from the nearest codebook entries, relative to how far the enrollment
//! phrases themselves lie from a codebook built without them. That ratio
//! sits around 1 for the enrolled speaker and well above it for other
//! voices, whatever was said, and it needs no recordings of anyone else.
//!
//! It mostly tells microphones and rooms apart: it keeps the television
//! and other devices out, but people in the same room can sound alike to
//! it. Neither method is proof of identity: a recording of the speaker
//! gets through.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::neural::{cosine, Ge2e};

const RATE: u32 = 16_000;
const WIN: usize = 400; // 25 ms
const HOP: usize = 160; // 10 ms
const NFFT: usize = 512;
const MELS: usize = 26;
const CEPS: usize = 19; // c1..c19; c0 (loudness) says nothing about who
const DIMS: usize = CEPS * 2;
const CODEBOOK: usize = 32;
/// An utterance needs this many speech frames (0.4 s) to be judged.
const MIN_FRAMES: usize = 40;

/// What enrollment learned about a voice.
#[derive(Serialize, Deserialize, Clone)]
pub struct Voiceprint {
    pub version: u32,
    /// Whose voice ("" in voiceprints from before there could be several).
    #[serde(default)]
    pub name: String,
    pub codebook: Vec<Vec<f32>>,
    /// Typical distortion of the speaker's own phrases (leave one out).
    pub baseline: f32,
    /// Each enrollment phrase's ratio against the others, for display.
    pub consistency: Vec<f32>,
    pub phrases: usize,
    /// When, in milliseconds since the epoch.
    pub enrolled_ms: u64,
    /// The voice embedding, when the neural model was installed at
    /// enrollment.
    #[serde(default)]
    pub neural: Option<NeuralPrint>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct NeuralPrint {
    /// Which model made it: embeddings from different models don't compare.
    pub model: String,
    /// The normalized average of the phrases' embeddings.
    pub embedding: Vec<f32>,
    /// Each phrase's similarity to the average of the others, for display.
    pub consistency: Vec<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Method {
    Neural,
    Codebook,
}

/// A score, with what it is measured against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    pub method: Method,
    pub value: f32,
    pub limit: f32,
}

impl std::fmt::Display for Score {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.method {
            Method::Neural => write!(f, "similarity {:.2} (needs at least {:.2})", self.value, self.limit),
            Method::Codebook => write!(f, "distortion {:.2} (needs at most {:.2})", self.value, self.limit),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// The enrolled voice.
    Match(Score),
    /// Someone else.
    Other(Score),
    /// Too little speech to tell.
    TooShort,
}

impl Verdict {
    pub fn accepted(self) -> bool {
        matches!(self, Verdict::Match(_))
    }
}

impl Voiceprint {
    /// Learn a voice from enrollment phrases (16-bit mono at `rate`), with
    /// the neural model too if there is one.
    pub fn enroll(clips: &[Vec<i16>], rate: u32, model: Option<&Ge2e>) -> Result<Self> {
        if clips.len() < 3 {
            bail!("enrollment needs at least three phrases");
        }
        let feats: Vec<Vec<[f32; DIMS]>> = clips.iter().map(|c| features(c, rate)).collect();
        if let Some(i) = feats.iter().position(|f| f.len() < MIN_FRAMES) {
            bail!("phrase {} has too little speech to learn from", i + 1);
        }
        let all: Vec<[f32; DIMS]> = feats.iter().flatten().copied().collect();
        let codebook = kmeans(&all, CODEBOOK);
        // Each phrase against a codebook made without it.
        let loo: Vec<f32> = (0..feats.len())
            .map(|i| {
                let rest: Vec<[f32; DIMS]> = feats.iter().enumerate().filter(|(j, _)| *j != i).flat_map(|(_, f)| f.iter().copied()).collect();
                distortion(&kmeans(&rest, CODEBOOK), &feats[i])
            })
            .collect();
        let baseline = loo.iter().sum::<f32>() / loo.len() as f32;
        let neural = match model {
            Some(m) => Some(NeuralPrint::enroll(m, clips, rate)?),
            None => None,
        };
        Ok(Self {
            version: 1,
            name: String::new(),
            codebook: codebook.iter().map(|c| c.to_vec()).collect(),
            baseline,
            consistency: loo.iter().map(|d| d / baseline).collect(),
            phrases: clips.len(),
            enrolled_ms: crate::chat::store::now_ms(),
            neural,
        })
    }

    /// The distortion ratio of an utterance: about 1 for the enrolled voice.
    pub fn score(&self, samples: &[i16], rate: u32) -> Option<f32> {
        let f = features(samples, rate);
        if f.len() < MIN_FRAMES {
            return None;
        }
        let book: Vec<[f32; DIMS]> = self
            .codebook
            .iter()
            .map(|c| {
                let mut a = [0.0; DIMS];
                a.iter_mut().zip(c).for_each(|(x, y)| *x = *y);
                a
            })
            .collect();
        Some(distortion(&book, &f) / self.baseline.max(1e-6))
    }

    /// The built-in method's verdict.
    pub fn verify(&self, samples: &[i16], rate: u32, threshold: f32) -> Verdict {
        match self.score(samples, rate) {
            None => Verdict::TooShort,
            Some(value) => {
                let score = Score { method: Method::Codebook, value, limit: threshold };
                if value <= threshold { Verdict::Match(score) } else { Verdict::Other(score) }
            }
        }
    }

    /// Where the single voiceprint of earlier versions lived.
    pub fn legacy_path() -> Option<PathBuf> {
        crate::policy::Policy::state_dir().map(|d| d.join("voiceprint.json"))
    }

    /// One file per person.
    pub fn dir() -> Option<PathBuf> {
        crate::policy::Policy::state_dir().map(|d| d.join("voices"))
    }

    pub fn path_for(name: &str) -> Option<PathBuf> {
        Self::dir().map(|d| d.join(format!("{name}.json")))
    }

    /// Everyone enrolled, by name. A voiceprint from before there could be
    /// several is moved in under the login name first.
    pub fn all() -> Result<Vec<Voiceprint>> {
        let state = crate::policy::Policy::state_dir().context("no state directory for voiceprints (is HOME set?)")?;
        Self::all_in(&state)
    }

    /// [`all`](Self::all), for a given state directory.
    pub fn all_in(state: &Path) -> Result<Vec<Voiceprint>> {
        let dir = state.join("voices");
        if let Some(legacy) = Some(state.join("voiceprint.json")).filter(|p| p.exists()) {
            let mut v = Self::load(&legacy)?;
            v.name = default_name();
            v.save(&dir.join(format!("{}.json", v.name)))?;
            std::fs::remove_file(&legacy)?;
        }
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().is_some_and(|x| x == "json") {
                    let mut v = Self::load(&path)?;
                    if v.name.is_empty() {
                        v.name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                    }
                    out.push(v);
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let v: Self = serde_json::from_str(&text).with_context(|| format!("{} is not a voiceprint", path.display()))?;
        if v.version != 1 || v.codebook.is_empty() || v.codebook.iter().any(|c| c.len() != DIMS) {
            bail!("{} is from another version of Orion; enroll again", path.display());
        }
        Ok(v)
    }

    /// Saved readable only by its owner, like the conversations.
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(serde_json::to_string(self)?.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

impl NeuralPrint {
    fn enroll(model: &Ge2e, clips: &[Vec<i16>], rate: u32) -> Result<Self> {
        let embeds: Vec<Vec<f32>> = clips
            .iter()
            .enumerate()
            .map(|(i, c)| model.embed(c, rate).with_context(|| format!("phrase {} has too little speech to learn from", i + 1)))
            .collect::<Result<_>>()?;
        let mean = |items: &mut dyn Iterator<Item = &Vec<f32>>| {
            let mut m = vec![0.0f32; embeds[0].len()];
            for e in items {
                m.iter_mut().zip(e).for_each(|(a, b)| *a += b);
            }
            m
        };
        let consistency = (0..embeds.len())
            .map(|i| cosine(&embeds[i], &mean(&mut embeds.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, e)| e))))
            .collect();
        let mut embedding = mean(&mut embeds.iter());
        let n = embedding.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
        embedding.iter_mut().for_each(|v| *v /= n);
        Ok(Self { model: super::neural::MODEL_NAME.into(), embedding, consistency })
    }
}

/// A login-friendly default name for the first person enrolled.
pub fn default_name() -> String {
    std::env::var("USER").ok().and_then(|u| valid_name(&u).ok()).unwrap_or_else(|| "me".into())
}

/// Names are file names too: letters, digits, `-` and `_`, up to 32.
pub fn valid_name(name: &str) -> Result<String> {
    let name = name.trim();
    let ok = !name.is_empty() && name.len() <= 32 && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_');
    if !ok {
        bail!("{name:?} can't be a name: use letters, digits, - and _ (up to 32)");
    }
    Ok(name.to_string())
}

/// Who an utterance sounds like.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub name: String,
    pub score: Score,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Heard {
    /// An enrolled person.
    Person(Identity),
    /// Nobody enrolled; the closest, for display.
    Nobody(Option<Identity>),
    /// Too little speech to tell.
    TooShort,
}

/// Decides who of the enrolled people an utterance is, by the best method
/// each voiceprint allows.
#[derive(Clone)]
pub struct Judge {
    pub people: Vec<Voiceprint>,
    model: Option<Arc<Ge2e>>,
    ratio_limit: f32,
    similarity_limit: f32,
}

impl Judge {
    /// The neural method for voiceprints with an embedding from this model,
    /// when the model is given; the built-in one for the rest.
    pub fn new(people: Vec<Voiceprint>, model: Option<Ge2e>, ratio_limit: f32, similarity_limit: f32) -> Self {
        Self { people, model: model.map(Arc::new), ratio_limit, similarity_limit }
    }

    fn neural_for<'a>(&'a self, v: &'a Voiceprint) -> Option<(&'a Ge2e, &'a NeuralPrint)> {
        let print = v.neural.as_ref().filter(|n| n.model == super::neural::MODEL_NAME)?;
        Some((self.model.as_deref()?, print))
    }

    /// The method used for this person.
    pub fn method_for(&self, v: &Voiceprint) -> Method {
        if self.neural_for(v).is_some() { Method::Neural } else { Method::Codebook }
    }

    /// The method used, if it's the same for everyone.
    pub fn method(&self) -> Option<Method> {
        let mut methods = self.people.iter().map(|v| self.method_for(v));
        let first = methods.next()?;
        methods.all(|m| m == first).then_some(first)
    }

    /// The best-matching enrolled person, if anyone is close enough. Scores
    /// from the two methods are ranked by how far past their own limit
    /// they are.
    pub fn identify(&self, samples: &[i16], rate: u32) -> Heard {
        // One embedding serves everyone on the neural method.
        let embedding = self.people.iter().any(|v| self.neural_for(v).is_some()).then(|| self.model.as_ref().and_then(|m| m.embed(samples, rate))).flatten();
        let mut best: Option<(f32, Identity)> = None;
        let mut too_short = true;
        for v in &self.people {
            let score = match self.neural_for(v) {
                Some((_, print)) => match &embedding {
                    Some(e) => Score { method: Method::Neural, value: cosine(e, &print.embedding), limit: self.similarity_limit },
                    None => continue,
                },
                None => match v.score(samples, rate) {
                    Some(value) => Score { method: Method::Codebook, value, limit: self.ratio_limit },
                    None => continue,
                },
            };
            too_short = false;
            let margin = match score.method {
                Method::Neural => (score.value - score.limit) / (1.0 - score.limit).max(1e-3),
                Method::Codebook => (score.limit - score.value) / score.limit.max(1e-3),
            };
            if best.as_ref().is_none_or(|(m, _)| margin > *m) {
                best = Some((margin, Identity { name: v.name.clone(), score }));
            }
        }
        match best {
            _ if too_short => Heard::TooShort,
            Some((m, id)) if m >= 0.0 => Heard::Person(id),
            other => Heard::Nobody(other.map(|(_, id)| id)),
        }
    }
}

/// Speech frames of an utterance as liftered cepstra and their deltas.
pub(crate) fn features(samples: &[i16], rate: u32) -> Vec<[f32; DIMS]> {
    let x = resample(samples, rate);
    if x.len() < WIN {
        return Vec::new();
    }
    let n = 1 + (x.len() - WIN) / HOP;
    let window: Vec<f32> = (0..WIN).map(|i| 0.54 - 0.46 * (std::f32::consts::TAU * i as f32 / (WIN - 1) as f32).cos()).collect();
    let mel = mel_bank();
    let lifter: Vec<f32> = (1..=CEPS).map(|k| (1.0 + 11.0 * (std::f32::consts::PI * k as f32 / 22.0).sin()) / 10.0).collect();
    let mut energy = Vec::with_capacity(n);
    let mut ceps: Vec<[f32; CEPS]> = Vec::with_capacity(n);
    for f in 0..n {
        let s = &x[f * HOP..f * HOP + WIN];
        let mut re = [0.0f32; NFFT];
        let mut im = [0.0f32; NFFT];
        for i in 0..WIN {
            let pre = if i == 0 { s[0] } else { s[i] - 0.97 * s[i - 1] };
            re[i] = pre * window[i];
        }
        energy.push(re.iter().map(|v| v * v).sum::<f32>());
        fft(&mut re, &mut im);
        let power: Vec<f32> = (0..=NFFT / 2).map(|k| re[k] * re[k] + im[k] * im[k]).collect();
        let logmel: Vec<f32> = mel.iter().map(|(start, w)| {
            let e: f32 = w.iter().enumerate().map(|(i, wt)| wt * power[start + i]).sum();
            (e + 1e-6).ln()
        }).collect();
        let mut c = [0.0f32; CEPS];
        for (k, ck) in c.iter_mut().enumerate() {
            let k1 = k + 1;
            *ck = logmel
                .iter()
                .enumerate()
                .map(|(m, v)| v * (std::f32::consts::PI * k1 as f32 * (m as f32 + 0.5) / MELS as f32).cos())
                .sum::<f32>()
                * lifter[k];
        }
        ceps.push(c);
    }
    // Speech frames: within 17 dB of the loud ones.
    let mut sorted = energy.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let p90 = sorted[(sorted.len() * 9 / 10).min(sorted.len() - 1)];
    let keep: Vec<usize> = (0..n).filter(|&i| energy[i] > p90 * 0.02 && energy[i] > 0.0).collect();
    let speech: Vec<[f32; CEPS]> = keep.iter().map(|&i| ceps[i]).collect();
    let m = speech.len();
    (0..m)
        .map(|i| {
            let mut v = [0.0f32; DIMS];
            v[..CEPS].copy_from_slice(&speech[i]);
            if i >= 2 && i + 2 < m {
                for k in 0..CEPS {
                    v[CEPS + k] = (speech[i + 2][k] - speech[i - 2][k]) / 8.0;
                }
            }
            v
        })
        .collect()
}

/// To 16 kHz floats, linearly.
pub(crate) fn resample(samples: &[i16], rate: u32) -> Vec<f32> {
    if rate == RATE || samples.is_empty() {
        return samples.iter().map(|&s| s as f32).collect();
    }
    let n = (samples.len() as u64 * RATE as u64 / rate as u64) as usize;
    let step = (samples.len() - 1) as f64 / n.max(2).saturating_sub(1) as f64;
    (0..n)
        .map(|i| {
            let p = i as f64 * step;
            let (j, t) = (p.floor() as usize, (p - p.floor()) as f32);
            let a = samples[j] as f32;
            let b = samples[(j + 1).min(samples.len() - 1)] as f32;
            a + (b - a) * t
        })
        .collect()
}

/// Triangular mel filters from 60 Hz to 7.6 kHz: (first bin, weights).
fn mel_bank() -> Vec<(usize, Vec<f32>)> {
    let mel = |f: f32| 2595.0 * (1.0 + f / 700.0).log10();
    let hz = |m: f32| 700.0 * (10f32.powf(m / 2595.0) - 1.0);
    let (lo, hi) = (mel(60.0), mel(7600.0));
    let bins: Vec<usize> = (0..MELS + 2)
        .map(|i| ((NFFT + 1) as f32 * hz(lo + (hi - lo) * i as f32 / (MELS + 1) as f32) / RATE as f32).floor() as usize)
        .collect();
    (1..=MELS)
        .map(|i| {
            let (a, b, c) = (bins[i - 1], bins[i], bins[i + 1]);
            let w = (a..c)
                .map(|k| if k < b { (k - a) as f32 / (b - a).max(1) as f32 } else { (c - k) as f32 / (c - b).max(1) as f32 })
                .collect();
            (a, w)
        })
        .collect()
}

/// In-place radix-2 FFT.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -std::f32::consts::TAU / len as f32;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (wr, wi) = ((ang * k as f32).cos(), (ang * k as f32).sin());
                let (a, b) = (start + k, start + k + len / 2);
                let (xr, xi) = (re[b] * wr - im[b] * wi, re[b] * wi + im[b] * wr);
                re[b] = re[a] - xr;
                im[b] = im[a] - xi;
                re[a] += xr;
                im[a] += xi;
            }
        }
        len <<= 1;
    }
}

fn dist2(a: &[f32; DIMS], b: &[f32; DIMS]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// Mean distance from each frame to its nearest codebook entry.
fn distortion(book: &[[f32; DIMS]], frames: &[[f32; DIMS]]) -> f32 {
    let total: f32 = frames.iter().map(|f| book.iter().map(|c| dist2(c, f)).fold(f32::MAX, f32::min).sqrt()).sum();
    total / frames.len().max(1) as f32
}

/// k-means with a deterministic spread-out start (k-means++ on a fixed
/// sequence), so the same phrases always give the same voiceprint.
fn kmeans(data: &[[f32; DIMS]], k: usize) -> Vec<[f32; DIMS]> {
    let k = k.min(data.len());
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut centers = vec![data[(next() * data.len() as f64) as usize % data.len()]];
    let mut nearest: Vec<f32> = data.iter().map(|d| dist2(d, &centers[0])).collect();
    while centers.len() < k {
        let total: f64 = nearest.iter().map(|&d| d as f64).sum();
        let mut pick = next() * total;
        let mut idx = data.len() - 1;
        for (i, &d) in nearest.iter().enumerate() {
            pick -= d as f64;
            if pick <= 0.0 {
                idx = i;
                break;
            }
        }
        centers.push(data[idx]);
        let c = centers[centers.len() - 1];
        nearest.iter_mut().zip(data).for_each(|(n, d)| *n = n.min(dist2(d, &c)));
    }
    refine(centers, data, 20)
}

/// Lloyd's iterations from the given centers.
fn refine(mut centers: Vec<[f32; DIMS]>, data: &[[f32; DIMS]], rounds: usize) -> Vec<[f32; DIMS]> {
    let mut assign = vec![usize::MAX; data.len()];
    for _ in 0..rounds {
        let mut changed = false;
        for (i, d) in data.iter().enumerate() {
            let best = (0..centers.len()).min_by(|&a, &b| dist2(d, &centers[a]).total_cmp(&dist2(d, &centers[b]))).unwrap_or(0);
            changed |= best != assign[i];
            assign[i] = best;
        }
        let mut sums = vec![[0.0f32; DIMS]; centers.len()];
        let mut counts = vec![0usize; centers.len()];
        for (d, &a) in data.iter().zip(&assign) {
            counts[a] += 1;
            sums[a].iter_mut().zip(d).for_each(|(s, v)| *s += v);
        }
        for (c, (s, n)) in centers.iter_mut().zip(sums.iter().zip(&counts)) {
            if *n > 0 {
                c.iter_mut().zip(s).for_each(|(c, s)| *c = s / *n as f32);
            }
        }
        if !changed {
            break;
        }
    }
    centers
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn fft_matches_a_plain_dft() {
        let x: Vec<f32> = (0..16).map(|i| ((i * 7 % 5) as f32 - 2.0) * 0.3).collect();
        let (mut re, mut im) = (x.clone(), vec![0.0; 16]);
        fft(&mut re, &mut im);
        for k in 0..16 {
            let (mut r, mut i) = (0.0f32, 0.0f32);
            for (n, v) in x.iter().enumerate() {
                let a = -std::f32::consts::TAU * (k * n) as f32 / 16.0;
                r += v * a.cos();
                i += v * a.sin();
            }
            assert!((re[k] - r).abs() < 1e-4 && (im[k] - i).abs() < 1e-4, "bin {k}");
        }
    }

    /// A tiny formant synthesizer: a pulse train at `f0` through resonators
    /// at the speaker's formants, gliding through a sequence of vowels. A
    /// speaker is a vocal tract (formant scale and bandwidth) and a pitch;
    /// `text` picks the vowels.
    pub fn voice(f0: f32, tract: f32, text: u64, secs: f32) -> Vec<i16> {
        const VOWELS: [[f32; 3]; 6] =
            [[730., 1090., 2440.], [270., 2290., 3010.], [300., 870., 2240.], [530., 1840., 2480.], [640., 1190., 2390.], [390., 1990., 2550.]];
        let n = (RATE as f32 * secs) as usize;
        let mut seq = text;
        let mut next = || {
            seq = seq.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seq >> 33) as usize
        };
        let syllables: Vec<usize> = (0..(secs * 4.0) as usize + 2).map(|_| next() % VOWELS.len()).collect();
        let mut out: Vec<f32> = Vec::with_capacity(n);
        let mut state = [[0.0f32; 2]; 3];
        let mut phase = 0.0f32;
        let mut noise = text.wrapping_add(17);
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let s = t * 4.0;
            let (a, b) = (syllables[s as usize], syllables[s as usize + 1]);
            let mix = s.fract();
            let pitch = f0 * (1.0 + 0.08 * (t * 3.1).sin());
            phase += pitch / RATE as f32;
            let mut x = if phase >= 1.0 {
                phase -= 1.0;
                3000.0
            } else {
                0.0
            };
            noise = noise.wrapping_mul(6364136223846793005).wrapping_add(1);
            x += ((noise >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 40.0;
            for (f, st) in state.iter_mut().enumerate() {
                let freq = (VOWELS[a][f] * (1.0 - mix) + VOWELS[b][f] * mix) * tract;
                let bw = 60.0 + 30.0 * f as f32 * tract;
                let r = (-std::f32::consts::PI * bw / RATE as f32).exp();
                let c = 2.0 * r * (std::f32::consts::TAU * freq / RATE as f32).cos();
                let y = x * (1.0 - r) + c * st[0] - r * r * st[1];
                st[1] = st[0];
                st[0] = y;
                x = y;
            }
            // Syllables with short gaps between them, like speech.
            let gate = if mix < 0.85 { 1.0 } else { 0.05 };
            out.push(x * gate);
        }
        // To a normal speaking level: peaks around -8 dBFS.
        let peak = out.iter().fold(1e-9f32, |m, v| m.max(v.abs()));
        out.iter().map(|v| (v / peak * 13_000.0) as i16).collect()
    }

    #[test]
    fn knows_a_voice_from_others() {
        let me = |text, secs| voice(120.0, 1.0, text, secs);
        // Six phrases of about three seconds, as `orion voice enroll` asks for.
        let enroll: Vec<Vec<i16>> = (1..=6).map(|t| me(t, 3.0)).collect();
        let vp = Voiceprint::enroll(&enroll, RATE, None).unwrap();
        assert_eq!(vp.consistency.len(), 6);
        // New phrases from the same voice pass, short ones included.
        for text in 20..26 {
            for secs in [0.6, 1.0, 2.0] {
                // (Under 0.4 s of speech is too short to judge, not a no.)
                let v = vp.verify(&me(text, secs), RATE, 1.2);
                assert!(v.accepted() || (secs < 1.0 && v == Verdict::TooShort), "text {text}, {secs}s: {v:?}");
            }
        }
        // Clearly different voices don't, whatever they say.
        for (f0, tract) in [(210.0, 1.18), (95.0, 0.88), (160.0, 1.3)] {
            for text in 20..24 {
                let v = vp.verify(&voice(f0, tract, text, 1.5), RATE, 1.2);
                assert!(matches!(v, Verdict::Other(_)), "f0 {f0} tract {tract} text {text}: {v:?}");
            }
        }
        // Close voices too: a vocal tract only 6% or 12% different.
        for tract in [1.06, 1.12] {
            for text in 20..26 {
                let v = vp.verify(&voice(120.0, tract, text, 1.5), RATE, 1.2);
                assert!(matches!(v, Verdict::Other(_)), "tract {tract} text {text}: {v:?}");
            }
        }
        assert_eq!(vp.verify(&me(30, 0.2), RATE, 1.2), Verdict::TooShort);
    }

    #[test]
    fn tells_enrolled_people_apart() {
        let person = |name: &str, f0, tract| {
            let clips: Vec<Vec<i16>> = (1..=6).map(|t| voice(f0, tract, t, 3.0)).collect();
            let mut v = Voiceprint::enroll(&clips, RATE, None).unwrap();
            v.name = name.into();
            v
        };
        let judge = Judge::new(vec![person("ana", 120.0, 1.0), person("ben", 210.0, 1.25)], None, 1.2, 0.8);
        assert_eq!(judge.method(), Some(Method::Codebook));
        let who = |f0, tract, text| match judge.identify(&voice(f0, tract, text, 1.5), RATE) {
            Heard::Person(id) => id.name,
            Heard::Nobody(_) => "nobody".into(),
            Heard::TooShort => "too short".into(),
        };
        for text in 20..24 {
            assert_eq!(who(120.0, 1.0, text), "ana");
            assert_eq!(who(210.0, 1.25, text), "ben");
            assert_eq!(who(95.0, 0.88, text), "nobody");
        }
        // The closest is named even when nobody matches.
        let Heard::Nobody(Some(closest)) = judge.identify(&voice(130.0, 0.9, 21, 1.5), RATE) else { panic!() };
        assert_eq!(closest.name, "ana");
        assert!(valid_name("ana-2").is_ok() && valid_name("../x").is_err() && valid_name("").is_err());
    }

    #[test]
    fn moves_the_old_single_voiceprint_in() {
        let state = tempfile::tempdir().unwrap();
        let clips: Vec<Vec<i16>> = (1..=3).map(|t| voice(120.0, 1.0, t, 1.5)).collect();
        let mut old = Voiceprint::enroll(&clips, RATE, None).unwrap();
        old.save(&state.path().join("voiceprint.json")).unwrap();
        let people = Voiceprint::all_in(state.path()).unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].name, default_name());
        assert!(!state.path().join("voiceprint.json").exists());
        assert!(state.path().join(format!("voices/{}.json", default_name())).exists());
        // Another person alongside, listed by name.
        old.name = "aaron".into();
        old.save(&state.path().join("voices/aaron.json")).unwrap();
        let names: Vec<String> = Voiceprint::all_in(state.path()).unwrap().into_iter().map(|v| v.name).collect();
        assert_eq!(names[0], "aaron");
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn saves_privately_and_loads() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/voiceprint.json");
        let enroll: Vec<Vec<i16>> = (1..=3).map(|t| voice(120.0, 1.0, t, 1.5)).collect();
        let vp = Voiceprint::enroll(&enroll, RATE, None).unwrap();
        vp.save(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let back = Voiceprint::load(&path).unwrap();
        assert_eq!(back.codebook, vp.codebook);
        assert!(Voiceprint::enroll(&enroll[..2], RATE, None).is_err());
    }

    /// Real (synthesized) voices, when espeak-ng is installed: enroll one,
    /// and check its other phrases pass and other voices don't.
    #[test]
    fn with_espeak_voices() {
        if super::super::which("espeak-ng").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let say = |voice: &str, pitch: u32, text: &str| -> Vec<i16> {
            let wav = dir.path().join("x.wav");
            let ok = std::process::Command::new("espeak-ng")
                .args(["-v", voice, "-p", &pitch.to_string(), "-w"])
                .arg(&wav)
                .arg(text)
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok);
            let bytes = std::fs::read(&wav).unwrap();
            let pcm: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
            pcm
        };
        let phrases = [
            "Orion, what is using the GPU right now?",
            "Orion, restart the ollama service please.",
            "Hey Orion, how full is my disk?",
            "Orion, load the qwen model and keep it warm.",
            "Orion, what time is it in Tokyo?",
        ];
        let enroll: Vec<Vec<i16>> = phrases.iter().map(|p| say("en-us+m3", 45, p)).collect();
        let vp = Voiceprint::enroll(&enroll, 22_050, None).unwrap();
        for p in ["Orion, install htop for me.", "Orion, stop.", "Orion, which services failed today?"] {
            assert!(vp.verify(&say("en-us+m3", 45, p), 22_050, 1.2).accepted(), "{p}");
        }
        for (v, pitch) in [("en-us+f2", 60), ("en-gb+m1", 35), ("en-gb-scotland+m5", 50), ("en+f1", 55)] {
            let verdict = vp.verify(&say(v, pitch, "Orion, which services failed today?"), 22_050, 1.2);
            assert!(!verdict.accepted(), "{v}: {verdict:?}");
        }

        // With the neural model installed, the judge uses it. (Synthesized
        // voices are hard for it, so only clearly different ones here.)
        let Some(model) = Ge2e::default_path().filter(|p| p.exists()).and_then(|p| Ge2e::load(&p).ok()) else { return };
        let vp = Voiceprint::enroll(&enroll, 22_050, Some(&model)).unwrap();
        assert_eq!(vp.neural.as_ref().unwrap().consistency.len(), 5);
        let judge = Judge::new(vec![vp], Some(model), 1.2, 0.8);
        assert_eq!(judge.method(), Some(Method::Neural));
        let mine = judge.identify(&say("en-us+m3", 45, "Orion, install htop for me."), 22_050);
        assert!(matches!(mine, Heard::Person(Identity { score: Score { method: Method::Neural, .. }, .. })), "{mine:?}");
        let other = judge.identify(&say("en-us+f2", 60, "Orion, install htop for me."), 22_050);
        assert!(matches!(other, Heard::Nobody(_)), "{other:?}");
    }
}
