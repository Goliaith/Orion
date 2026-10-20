//! `orion voice enroll | test | status | forget`: teach Orion your voice,
//! check it, and undo it.

use std::io::{BufRead, Write};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::speaker::{Method, Verdict, Voiceprint};
use super::{Recorder, Verify, VoiceConfig};

/// About three seconds each, the way people talk to Orion.
const PHRASES: [&str; 6] = [
    "Orion, what is using the GPU right now, and how hot is it?",
    "Hey Orion, restart the model server and tell me when it's ready.",
    "Orion, how much space is left on my disk this morning?",
    "Okay Orion, read me the last few errors from the system log.",
    "Orion, install the package I need for the project we talked about.",
    "Orion, stop that, and remind me what we were doing yesterday.",
];

fn path() -> Result<std::path::PathBuf> {
    Voiceprint::default_path().context("no state directory for the voiceprint (is HOME set?)")
}

/// Record one utterance until the speaker pauses, with a meter.
fn record(recorder: &Recorder) -> Result<Vec<i16>> {
    let rec = recorder.start()?;
    let mut out = std::io::stdout();
    while !rec.should_stop() {
        let n = (rec.level() * 20.0) as usize;
        let _ = write!(out, "\r  ● {:<20} {:>2}s ", "▮".repeat(n), rec.elapsed().as_secs());
        let _ = out.flush();
        thread::sleep(Duration::from_millis(50));
    }
    let _ = write!(out, "\r{:40}\r", "");
    let wav = rec.finish()?;
    Ok(wav[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
}

fn wait_for_enter() -> Result<()> {
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("enrollment needs a terminal to talk you through it");
    }
    Ok(())
}

pub fn enroll(config: &VoiceConfig) -> Result<()> {
    let recorder = Recorder::detect(config)?;
    let model = config.speaker_model().map_err(anyhow::Error::msg)?;
    if model.is_none() && config.verify_method != super::VerifyMethod::Builtin {
        println!("(The neural speaker model isn't installed, so this enrolls with the built-in method, which tells");
        println!(" microphones apart better than people. `orion voice model install` first is better.)\n");
    }
    println!("Orion will learn your voice from {} short phrases, so that only you can wake it.", PHRASES.len());
    println!("Speak as you normally would to Orion, from where you usually sit. Each recording ends when you pause.\n");
    let mut clips = Vec::new();
    for (i, phrase) in PHRASES.iter().enumerate() {
        loop {
            print!("{}/{}  Press Enter, then say:  \"{phrase}\" ", i + 1, PHRASES.len());
            std::io::stdout().flush()?;
            wait_for_enter()?;
            match record(&recorder) {
                Ok(clip) if clip.len() as u64 >= recorder.rate() as u64 => {
                    println!("  ✓ {:.1}s", clip.len() as f32 / recorder.rate() as f32);
                    clips.push(clip);
                    break;
                }
                Ok(_) => println!("  That was very short; once more, please."),
                Err(e) => println!("  {e:#}; once more, please."),
            }
        }
    }
    println!("\nLearning your voice…");
    let vp = Voiceprint::enroll(&clips, recorder.rate(), model.as_ref())?;
    let odd = |i: usize, how: String| {
        println!("  Phrase {} sounded different from the rest ({how}): noise, or a different distance from the microphone?", i + 1)
    };
    match &vp.neural {
        Some(n) => {
            let worst = n.consistency.iter().copied().fold(1.0f32, f32::min);
            println!("Checked each phrase against the others (neural model): lowest similarity {worst:.2}; the limit is {:.2}.", config.verify_similarity);
            for (i, c) in n.consistency.iter().enumerate().filter(|(_, c)| **c < config.verify_similarity) {
                odd(i, format!("{c:.2}"));
            }
        }
        None => {
            let worst = vp.consistency.iter().copied().fold(0.0f32, f32::max);
            println!("Checked each phrase against the others: worst {worst:.2} (a phrase in your voice scores about 1; the limit is {:.2}).", config.verify_threshold);
            for (i, c) in vp.consistency.iter().enumerate().filter(|(_, c)| **c > config.verify_threshold) {
                odd(i, format!("{c:.2}"));
            }
        }
    }
    let path = path()?;
    vp.save(&path)?;
    println!("Saved your voiceprint to {} (readable only by you).", path.display());
    match config.verify_speaker {
        Verify::Off => println!("Speaker verification is off in the policy (voice.verify_speaker = \"off\"); set it to \"auto\" to use it."),
        _ => println!("From now on, only your voice wakes Orion. Check it with `orion voice test`."),
    }
    Ok(())
}

pub fn test(config: &VoiceConfig) -> Result<()> {
    let judge = config.judge().map_err(anyhow::Error::msg)?.context("no voice enrolled yet: run `orion voice enroll`")?;
    let recorder = Recorder::detect(config)?;
    print!("Press Enter, then say anything to Orion. ");
    std::io::stdout().flush()?;
    wait_for_enter()?;
    let clip = record(&recorder)?;
    let setting = match judge.method() {
        Method::Neural => "lower voice.verify_similarity",
        Method::Codebook => "raise voice.verify_threshold",
    };
    match judge.verify(&clip, recorder.rate()) {
        Verdict::Match(s) => println!("That's you: {s}."),
        Verdict::Other(s) => println!(
            "That doesn't sound like the enrolled voice: {s}. If it was you, enroll again where you usually talk to Orion, or {setting}."
        ),
        Verdict::TooShort => println!("Too little speech to tell; say a little more."),
    }
    Ok(())
}

pub fn status(config: &VoiceConfig) -> Result<()> {
    let path = path()?;
    match Voiceprint::load(&path) {
        Ok(vp) => {
            let mode = match config.verify_speaker {
                Verify::Off => "off in the policy",
                _ => "only your voice wakes Orion",
            };
            println!("enrolled: {} phrases, {} ({mode})", vp.phrases, path.display());
            let judge = config.judge().ok().flatten().map(|j| j.method());
            match (judge, &vp.neural) {
                (Some(Method::Neural), _) => println!("method: neural model, similarity at least {:.2}", config.verify_similarity),
                (_, Some(_)) => println!("method: built-in (the voice has a neural embedding, but the model isn't installed or is turned off)"),
                _ => println!("method: built-in, distortion at most {:.2} (`orion voice model install`, then enroll again, for the neural model)", config.verify_threshold),
            }
        }
        Err(_) if !path.exists() => println!("no voice enrolled: anyone can wake Orion (`orion voice enroll`)"),
        Err(e) => println!("the voiceprint can't be read: {e:#}"),
    }
    Ok(())
}

pub fn forget() -> Result<()> {
    let path = path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => println!("Forgot your voice: anyone can wake Orion again."),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("No voice was enrolled."),
        Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
    }
    Ok(())
}

pub fn model_install(from: Option<&std::path::Path>) -> Result<()> {
    match from {
        Some(p) => println!("Converting the speaker model from {}…", p.display()),
        None => println!("Downloading the speaker model ({}) from PyPI…", super::neural::MODEL_NAME),
    }
    let path = super::neural::install(from)?;
    println!("Installed: {}.", path.display());
    let enrolled = Voiceprint::default_path().and_then(|p| Voiceprint::load(&p).ok());
    match enrolled {
        Some(vp) if vp.neural.is_none() => {
            println!("Your voice was enrolled before the model was installed; run `orion voice enroll` again to use it.")
        }
        Some(_) => println!("Speaker verification now uses it."),
        None => println!("Enroll your voice with `orion voice enroll` to use it."),
    }
    Ok(())
}

pub fn model_status() -> Result<()> {
    let path = super::neural::Ge2e::default_path().context("no data directory (is HOME set?)")?;
    match super::neural::Ge2e::load(&path) {
        Ok(_) => println!("installed: {} ({})", super::neural::MODEL_NAME, path.display()),
        Err(_) if !path.exists() => println!("not installed: verification uses the built-in method (`orion voice model install`)"),
        Err(e) => println!("can't be loaded: {e:#}"),
    }
    Ok(())
}

pub fn model_remove() -> Result<()> {
    let path = super::neural::Ge2e::default_path().context("no data directory (is HOME set?)")?;
    match std::fs::remove_file(&path) {
        Ok(()) => println!("Removed the speaker model; verification falls back to the built-in method."),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("The speaker model isn't installed."),
        Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
    }
    Ok(())
}
