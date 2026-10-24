//! `orion voice enroll | test | status | forget`: teach Orion the voices
//! of the people who may wake it, check them, and undo it. `orion voice
//! noise` measures the room and suggests a speech threshold.

use std::io::{BufRead, Write};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::capitalize as cap;
use super::record::{rms, Sensitivity, CHUNK_MS};
use super::speaker::{valid_name, Heard, Method, Voiceprint};
use super::{Recorder, ToolAccess, Verify, VoiceConfig};

/// About three seconds each, the way people talk to Orion.
const PHRASES: [&str; 6] = [
    "Orion, what is using the GPU right now, and how hot is it?",
    "Hey Orion, restart the model server and tell me when it's ready.",
    "Orion, how much space is left on my disk this morning?",
    "Okay Orion, read me the last few errors from the system log.",
    "Orion, install the package I need for the project we talked about.",
    "Orion, stop that, and remind me what we were doing yesterday.",
];

/// Record one utterance until the speaker pauses, with a meter.
pub(super) fn record(recorder: &Recorder) -> Result<Vec<i16>> {
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
    Ok(wav[44..].as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect())
}

fn wait_for_enter() -> Result<()> {
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("enrollment needs a terminal to talk you through it");
    }
    Ok(())
}

/// Enroll someone: `name`, or the login name for the first person.
pub fn enroll(config: &VoiceConfig, name: Option<&str>) -> Result<()> {
    let people = Voiceprint::all()?;
    let name = match name {
        Some(n) => valid_name(n)?,
        None if people.is_empty() => super::speaker::default_name(),
        None => bail!(
            "{} enrolled already; name the person: `orion voice enroll --name <name>` (again with an existing name replaces their voice)",
            people.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ")
        ),
    };
    let recorder = Recorder::detect(config)?;
    let model = config.speaker_model().map_err(anyhow::Error::msg)?;
    if model.is_none() && config.verify_method != super::VerifyMethod::Builtin {
        println!("(The neural speaker model isn't installed, so this enrolls with the built-in method, which tells");
        println!(" microphones apart better than people. `orion voice model install` first is better.)\n");
    }
    let again = people.iter().any(|v| v.name == name);
    println!(
        "Orion will learn {}'s voice from {} short phrases{}.",
        cap(&name),
        PHRASES.len(),
        if again { " (replacing the voice enrolled under that name)" } else { "" }
    );
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
    println!("\nLearning {}'s voice…", cap(&name));
    let mut vp = Voiceprint::enroll(&clips, recorder.rate(), model.as_ref())?;
    vp.name = name.clone();
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
    // Too close to someone already enrolled: Orion would mix them up.
    if let Some(judge) = config.judge().ok().flatten() {
        let others = judge.people.iter().filter(|v| v.name != name).count();
        if others > 0 {
            for (i, clip) in clips.iter().enumerate() {
                if let Heard::Person(id) = judge.identify(clip, recorder.rate()) {
                    if id.name != name {
                        println!("  Phrase {} also sounds like {} ({}): Orion may mix the two of you up.", i + 1, cap(&id.name), id.score);
                    }
                }
            }
        }
    }
    let path = Voiceprint::path_for(&name).context("no state directory for voiceprints (is HOME set?)")?;
    vp.save(&path)?;
    println!("Saved {}'s voiceprint to {} (readable only by you).", cap(&name), path.display());
    match config.verify_speaker {
        Verify::Off => println!("Speaker verification is off in the policy (voice.verify_speaker = \"off\"); set it to \"auto\" to use it."),
        _ => {
            let all: Vec<String> = Voiceprint::all()?.iter().map(|v| cap(&v.name)).collect();
            println!("From now on, only {} can wake Orion. Check it with `orion voice test`.", all.join(", "));
            if !config.people.contains_key(&name) {
                println!("{} may do everything by voice; limit that under [voice.people.{name}] in the policy.", cap(&name));
            }
        }
    }
    Ok(())
}

pub fn test(config: &VoiceConfig) -> Result<()> {
    let judge = config.judge().map_err(anyhow::Error::msg)?.context("nobody is enrolled yet: run `orion voice enroll`")?;
    let recorder = Recorder::detect(config)?;
    print!("Press Enter, then say anything to Orion. ");
    std::io::stdout().flush()?;
    wait_for_enter()?;
    let clip = record(&recorder)?;
    match judge.identify(&clip, recorder.rate()) {
        Heard::Person(id) => println!("That's {}: {}.", cap(&id.name), id.score),
        Heard::Nobody(closest) => {
            print!("That doesn't sound like anyone enrolled");
            match closest {
                Some(id) => {
                    let setting = match id.score.method {
                        Method::Neural => "lower voice.verify_similarity",
                        Method::Codebook => "raise voice.verify_threshold",
                    };
                    println!(
                        " (closest: {}, {}). If it was {}, enroll again with `orion voice enroll --name {}`, or {setting}.",
                        cap(&id.name),
                        id.score,
                        cap(&id.name),
                        id.name
                    )
                }
                None => println!("."),
            }
        }
        Heard::TooShort => println!("Too little speech to tell; say a little more."),
    }
    Ok(())
}

/// Record for a fixed time, with a meter, and return each chunk's level.
pub(super) fn levels_for(recorder: &Recorder, secs: u64) -> Result<Vec<f32>> {
    let rec = recorder.start()?;
    let mut out = std::io::stdout();
    while rec.elapsed() < Duration::from_secs(secs) {
        let n = (rec.level() * 20.0) as usize;
        let _ = write!(out, "\r  ● {:<20} {:>2}s ", "▮".repeat(n), secs.saturating_sub(rec.elapsed().as_secs()));
        let _ = out.flush();
        thread::sleep(Duration::from_millis(50));
    }
    let _ = write!(out, "\r{:40}\r", "");
    let chunk = (recorder.rate() as u64 * CHUNK_MS / 1000) as usize;
    Ok(rec.finish_raw()?.chunks_exact(chunk).map(rms).collect())
}

pub(super) fn percentile(levels: &[f32], p: f32) -> f32 {
    let mut v = levels.to_vec();
    v.sort_by(f32::total_cmp);
    v.get(((v.len().saturating_sub(1)) as f32 * p).round() as usize).copied().unwrap_or(0.0)
}

/// What `orion voice noise` found.
#[derive(Debug, PartialEq)]
pub(crate) enum Advice {
    /// The current settings already tell the two apart.
    Fine,
    /// Set these to put the threshold between the noise and the voice.
    Set { margin: Option<f32>, level: Option<f32> },
    /// The noise is as loud as the voice: no threshold separates them.
    TooLoud,
}

/// Put the threshold above the room's loudest noise and below most of the
/// voice, moving it as little as possible.
pub(crate) fn advise(quiet: &[f32], speech: &[f32], now: Sensitivity) -> Advice {
    let floor = percentile(quiet, 0.1);
    let noise = percentile(quiet, 0.95) * 1.2;
    let voice = percentile(speech, 0.5) * 0.6;
    if noise >= voice {
        return Advice::TooLoud;
    }
    let threshold = now.threshold(Some(floor));
    if (noise..=voice).contains(&threshold) {
        return Advice::Fine;
    }
    let target = threshold.clamp(noise, voice);
    // The margin follows the room as it gets louder or quieter, so prefer
    // it, unless the room is nearly silent (a digital microphone, say).
    // In half steps, within what separates the two.
    let (lo, hi) = ((noise / floor.max(1.0) * 2.0).ceil() / 2.0, (voice / floor.max(1.0) * 2.0).floor() / 2.0);
    if floor >= 10.0 && lo.max(1.0) <= hi {
        let margin = ((target / floor * 2.0).round() / 2.0).clamp(lo.max(1.0), hi);
        let level = (now.level > voice).then_some((target / 10.0).round() * 10.0);
        Advice::Set { margin: Some(margin), level }
    } else {
        Advice::Set { margin: None, level: Some((target / 10.0).round() * 10.0) }
    }
}

/// The room and the voice, as `orion voice noise` hears them: each chunk's
/// level, five seconds of each.
pub(super) struct Room {
    quiet: Vec<f32>,
    speech: Vec<f32>,
}

impl Room {
    pub(super) fn measure(recorder: &Recorder) -> Result<Self> {
        print!("Press Enter, then stay quiet for 5 seconds, with the room as it usually is (fan, TV, typing). ");
        std::io::stdout().flush()?;
        wait_for_enter()?;
        let quiet = levels_for(recorder, 5)?;
        print!("Now press Enter and talk to Orion for 5 seconds, from where you usually sit. ");
        std::io::stdout().flush()?;
        wait_for_enter()?;
        let speech = levels_for(recorder, 5)?;
        Ok(Self { quiet, speech })
    }

    /// Say what was heard, and what would tell the two apart.
    pub(super) fn report(&self, now: Sensitivity) -> Advice {
        let floor = percentile(&self.quiet, 0.1);
        let threshold = now.threshold(Some(floor));
        let crossed = self.quiet.iter().filter(|&&l| l > threshold).count() * 100 / self.quiet.len().max(1);
        println!(
            "Room: {:.0} usually, up to {:.0}. Your voice: about {:.0}. Speech threshold now: {threshold:.0} (noise_margin {}, speech_level {}).",
            floor,
            percentile(&self.quiet, 0.95),
            percentile(&self.speech, 0.5),
            now.margin,
            now.level
        );
        if crossed > 0 {
            println!("{crossed}% of the quiet part counted as speech, which keeps Orion listening after you stop.");
        }
        let advice = advise(&self.quiet, &self.speech, now);
        match advice {
            Advice::Fine => println!("That's fine: the room stays under the threshold and your voice clears it."),
            Advice::TooLoud => println!(
                "The room is about as loud as your voice, so no threshold tells them apart. Move closer to the microphone, \
                 or quiet the noise. A wake-word detector (voice.wake_command) copes better with a noisy room."
            ),
            Advice::Set { .. } => {}
        }
        advice
    }
}

/// `orion voice noise`: measure the room and the voice, and suggest a
/// threshold that keeps the room's noise from counting as speech.
pub fn noise(config: &VoiceConfig) -> Result<()> {
    let recorder = Recorder::detect(config)?;
    if let Advice::Set { margin, level } = Room::measure(&recorder)?.report(recorder.sensitivity()) {
        println!("Put this under [voice] in your settings:");
        if let Some(m) = margin {
            println!("  noise_margin = {m:.1}");
        }
        if let Some(l) = level {
            println!("  speech_level = {l:.0}");
        }
    }
    Ok(())
}

pub fn status(config: &VoiceConfig) -> Result<()> {
    let people = Voiceprint::all()?;
    if people.is_empty() {
        println!("nobody enrolled: anyone can wake Orion (`orion voice enroll`)");
        return Ok(());
    }
    if config.verify_speaker == Verify::Off {
        println!("speaker verification is off in the policy (voice.verify_speaker = \"off\")");
    }
    let judge = config.judge().ok().flatten();
    println!("{} enrolled; only they can wake Orion:", people.len());
    for v in &people {
        let method = match judge.as_ref().map(|j| j.method_for(v)) {
            Some(Method::Neural) => format!("neural model, similarity at least {:.2}", config.verify_similarity),
            _ if v.neural.is_some() => "built-in (has a neural embedding, but the model isn't installed or is off)".into(),
            _ => format!("built-in, distortion at most {:.2}", config.verify_threshold),
        };
        let p = config.person(&v.name);
        let tools = match p.tools {
            ToolAccess::All => "all tools",
            ToolAccess::Read => "read-only tools",
            ToolAccess::None => "no tools",
        };
        let approve = if !config.wake_approvals { "no spoken approvals (voice.wake_approvals)" } else if p.approve { "may approve" } else { "may not approve" };
        println!("  {:<12} {method}; {tools}, {approve}", v.name);
    }
    if judge.as_ref().is_some_and(|j| j.method().is_none()) || people.iter().any(|v| v.neural.is_none()) {
        println!("(With the neural model installed, enroll everyone again to use it: `orion voice enroll --name <name>`.)");
    }
    Ok(())
}

/// Forget someone's voice, or everyone's.
pub fn forget(name: Option<&str>, all: bool) -> Result<()> {
    let people = Voiceprint::all()?;
    let doomed: Vec<&Voiceprint> = match (name, all) {
        (_, true) => people.iter().collect(),
        (Some(n), _) => {
            let v = people.iter().find(|v| v.name == n).with_context(|| format!("nobody named {n} is enrolled"))?;
            vec![v]
        }
        (None, _) if people.len() <= 1 => people.iter().collect(),
        (None, _) => bail!(
            "several people are enrolled ({}); name one, or pass --all",
            people.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ")
        ),
    };
    if doomed.is_empty() {
        println!("Nobody was enrolled.");
        return Ok(());
    }
    for v in &doomed {
        let path = Voiceprint::path_for(&v.name).context("no state directory")?;
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        println!("Forgot {}'s voice.", cap(&v.name));
    }
    if doomed.len() == people.len() {
        println!("Nobody is enrolled now: anyone can wake Orion again.");
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
    let people = Voiceprint::all().unwrap_or_default();
    let before: Vec<&str> = people.iter().filter(|v| v.neural.is_none()).map(|v| v.name.as_str()).collect();
    match (people.is_empty(), before.is_empty()) {
        (true, _) => println!("Enroll your voice with `orion voice enroll` to use it."),
        (false, true) => println!("Speaker verification now uses it."),
        (false, false) => println!(
            "{} enrolled before the model was installed; enroll again (`orion voice enroll --name <name>`) to use it.",
            before.join(", ")
        ),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(level: f32, n: usize) -> Vec<f32> {
        vec![level; n]
    }

    #[test]
    fn a_quiet_room_needs_nothing() {
        assert_eq!(advise(&flat(50.0, 50), &flat(3000.0, 50), Sensitivity::default()), Advice::Fine);
    }

    #[test]
    fn a_fan_that_crosses_the_threshold_raises_the_margin() {
        // A floor of 200 with bursts to 900: 3x (600) lets the bursts through.
        let mut quiet = flat(200.0, 40);
        quiet.extend(flat(900.0, 10));
        match advise(&quiet, &flat(5000.0, 50), Sensitivity::default()) {
            Advice::Set { margin: Some(m), level: None } => {
                assert!(200.0 * m > 900.0 * 1.2, "{m} still lets the fan through");
                assert!(200.0 * m < 5000.0 * 0.6, "{m} would miss the voice");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_quiet_voice_lowers_a_high_threshold() {
        let now = Sensitivity { margin: 3.0, level: 1500.0 };
        match advise(&flat(100.0, 50), &flat(1000.0, 50), now) {
            Advice::Set { level: Some(l), .. } => assert!((120.0..=600.0).contains(&l), "{l}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_silent_microphone_sets_the_level() {
        let now = Sensitivity { margin: 3.0, level: 5.0 };
        let mut quiet = flat(1.0, 45);
        quiet.extend(flat(400.0, 5));
        assert!(matches!(advise(&quiet, &flat(4000.0, 50), now), Advice::Set { margin: None, level: Some(l) } if l >= 480.0));
    }

    #[test]
    fn noise_as_loud_as_the_voice_cannot_be_fixed() {
        assert_eq!(advise(&flat(2000.0, 50), &flat(2500.0, 50), Sensitivity::default()), Advice::TooLoud);
    }
}
