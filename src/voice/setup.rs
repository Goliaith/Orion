//! `orion voice setup`: everything voice needs, in one guided pass and in
//! the order each part depends on the last: the microphone, the room's
//! noise, the wake word, your voice and a real request, and whether speech
//! to text keeps up on this machine. Each step does what its own command
//! does (`orion voice noise`, `orion voice enroll`, contrib/orion-wake) and
//! saves what it learns, so nothing needs editing by hand.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::capitalize as cap;
use super::enroll::{self, percentile, Advice, Room};
use super::speaker::{self, Heard, Voiceprint};
use super::tuning::Tuning;
use super::{Recorder, Speaker, Transcriber, VerifyMethod, VoiceConfig};
use crate::chat::agent::{self, Answer, FromAgent, ToAgent};
use crate::policy::Policy;
use crate::setup::{edit_config, set, tilde, Prompt};
use crate::Orion;

/// The steps, in order; `--from` starts at one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Step {
    Mic,
    Noise,
    Wake,
    Enroll,
    Speed,
}

impl Step {
    const ALL: [Step; 5] = [Step::Mic, Step::Noise, Step::Wake, Step::Enroll, Step::Speed];

    fn title(self) -> &'static str {
        match self {
            Step::Mic => "The microphone",
            Step::Noise => "The room's noise",
            Step::Wake => "The wake word",
            Step::Enroll => "Your voice, and a first request",
            Step::Speed => "Speech to text on this machine",
        }
    }
}

/// Where the detector goes, and how `wake_command` names it.
const WAKE_COMMAND: &str = "~/.local/bin/orion-wake";
/// Shipped inside Orion, so no checkout is needed to install it.
const ORION_WAKE: &str = include_str!("../../contrib/orion-wake");
/// Each try at saying the wake word.
const WAKE_TRY: Duration = Duration::from_secs(8);
/// Loading Vosk, and downloading its model the first time.
const WAKE_START: Duration = Duration::from_secs(180);

pub fn run(policy: Policy, source: Option<&Path>, explicit: Option<&Path>, from: Step) -> Result<()> {
    let mut ask = Prompt::open(false).context("orion voice setup talks you through it, so run it in a terminal")?;
    let path = explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("ORION_CONFIG").filter(|v| !v.is_empty()).map(PathBuf::from))
        .or_else(Policy::default_config_path)
        .context("no home directory to keep the policy in")?;
    if !policy.voice.enabled {
        anyhow::bail!("voice is turned off in the policy (voice.enabled = false)");
    }
    let mut policy = policy;
    println!("Orion voice setup: five steps, about five minutes. Ctrl-C stops; `orion voice setup --from <step>` picks up again.");
    let mut heard = None;
    for (i, step) in Step::ALL.iter().enumerate().filter(|(_, s)| **s >= from) {
        println!("\n{}/{}  {}\n", i + 1, Step::ALL.len(), step.title());
        match step {
            Step::Mic => mic(&mut ask, &policy.voice)?,
            Step::Noise => noise(&mut policy.voice)?,
            Step::Wake => wake(&mut ask, &mut policy.voice, &path)?,
            Step::Enroll => heard = voice_and_request(&mut ask, &policy, source)?,
            Step::Speed => speed(&mut ask, &policy.voice, heard.take())?,
        }
    }
    println!("\nDone. `orion chat` or `orion shell` listens for \"Orion\" (Alt-W turns it on and off); `orion listen` does it with no screen.");
    println!("`orion doctor` checks it all again later.");
    Ok(())
}

// ---------------------------------------------------------------------------
// 1. The microphone.

/// What a few seconds of counting out loud sounded like.
#[derive(Debug, PartialEq)]
pub(crate) enum Mic {
    /// Nothing at all: muted, unplugged, or the wrong input.
    Silent,
    /// Something, but the voice barely clears the default speech level.
    Quiet(f32),
    Good(f32),
}

pub(crate) fn hear_mic(levels: &[f32]) -> Mic {
    let loudest = percentile(levels, 0.9);
    if levels.iter().copied().fold(0.0, f32::max) < 8.0 {
        Mic::Silent
    } else if loudest < 600.0 {
        Mic::Quiet(loudest)
    } else {
        Mic::Good(loudest)
    }
}

fn mic(ask: &mut Prompt, v: &VoiceConfig) -> Result<()> {
    let recorder = Recorder::detect(v).context("can't record: install pulseaudio-utils (parecord), or set voice.record_command")?;
    let input = Command::new("pactl")
        .arg("get-default-source")
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    match &input {
        Some(name) => println!("Recording from {name}, with {}.", recorder.describe()),
        None => println!("Recording with {}.", recorder.describe()),
    }
    let fix = "In KDE: System Settings → Sound → Input Devices: pick the microphone, unmute it and turn it up (or pavucontrol). \
               Then run `orion voice setup` again.";
    ask.line("Press Enter, then count to five out loud. ")?;
    let levels = enroll::levels_for(&recorder, 4).with_context(|| format!("the microphone gave nothing. {fix}"))?;
    match hear_mic(&levels) {
        Mic::Silent => anyhow::bail!("the microphone records only silence. {fix}"),
        Mic::Quiet(l) => println!(
            "✓ It hears you, but quietly (about {l:.0}; a voice is usually 1000 or more). Turning the input up helps; \
             the next step sets Orion's threshold for what it has."
        ),
        Mic::Good(l) => println!("✓ It hears you (about {l:.0})."),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 2. The room.

/// Measure as `orion voice noise` does, and save what it suggests to the
/// voice tuning, which applies over `[voice]`.
fn noise(v: &mut VoiceConfig) -> Result<()> {
    let recorder = Recorder::detect(v)?;
    let Advice::Set { margin, level } = Room::measure(&recorder)?.report(recorder.sensitivity()) else { return Ok(()) };
    let mut tuning = Tuning::load();
    tuning.noise_margin = margin.or(tuning.noise_margin);
    tuning.speech_level = level.or(tuning.speech_level);
    tuning.check()?;
    let path = tuning.save()?;
    tuning.apply(v);
    let saved: Vec<String> = [margin.map(|m| format!("noise_margin = {m:.1}")), level.map(|l| format!("speech_level = {l:.0}"))].into_iter().flatten().collect();
    println!("✓ Saved {} to {}, which applies over [voice].", saved.join(" and "), tilde(&path));
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. The wake word.

fn wake(ask: &mut Prompt, v: &mut VoiceConfig, policy_path: &Path) -> Result<()> {
    let ours = v.wake_command.split_whitespace().next() == Some(WAKE_COMMAND) && !v.wake_command.contains("--engine");
    if !v.wake_command.is_empty() && !ours {
        println!("wake_command is already set to your own detector ({}); leaving it as it is.", v.wake_command);
        return Ok(());
    }
    let bin = crate::policy::expand_home(Path::new(WAKE_COMMAND));
    install_detector(&bin)?;
    if !has_python_module("vosk") && !install_vosk(ask)? {
        println!("Skipping the wake word for now; without it Orion wakes by transcribing what it hears, which is slower.");
        return Ok(());
    }
    if !hear_wake_word(ask, &bin, &v.record_command)? {
        println!("Left wake_command unset, so Orion wakes by transcribing what it hears. `orion voice setup --from wake` tries again.");
        return Ok(());
    }
    // The detector records for itself, the same way Orion does.
    let command = match v.record_command.as_str() {
        "" => WAKE_COMMAND.to_string(),
        r => format!("{WAKE_COMMAND} --record {}", super::shell_quote(r)),
    };
    let mut edits = Vec::new();
    if v.wake_command != command {
        edits.push(set("voice.wake_command", command.as_str()));
    }
    let wake_on = !v.wake && ask.yes("Listen for \"Orion\" from the start whenever the chat or shell is open?", true)?;
    if wake_on {
        edits.push(set("voice.wake", true));
    }
    if edits.is_empty() {
        println!("✓ wake_command already uses it.");
        return Ok(());
    }
    let backup = edit_config(policy_path, &edits)?;
    v.wake_command = command.clone();
    v.wake |= wake_on;
    println!(
        "✓ Saved wake_command = {}{} in {}{}.",
        toml_edit::Value::from(command.as_str()),
        if wake_on { " and wake = true" } else { "" },
        tilde(policy_path),
        backup.map(|b| format!(" (the old one is {})", tilde(&b))).unwrap_or_default()
    );
    Ok(())
}

/// Put contrib/orion-wake where `wake_command` finds it.
fn install_detector(bin: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::read_to_string(bin).is_ok_and(|t| t == ORION_WAKE) {
        println!("orion-wake is installed ({WAKE_COMMAND}).");
        return Ok(());
    }
    if let Some(dir) = bin.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(bin, ORION_WAKE).with_context(|| format!("writing {}", bin.display()))?;
    std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755))?;
    println!("Installed the wake-word detector, orion-wake, as {WAKE_COMMAND}.");
    Ok(())
}

fn has_python_module(module: &str) -> bool {
    Command::new("python3")
        .args(["-c", &format!("import {module}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Install Vosk for this user, and pip first if it's missing. Whether
/// Vosk is there now.
fn install_vosk(ask: &mut Prompt) -> Result<bool> {
    if Command::new("python3").arg("--version").stdout(Stdio::null()).status().is_err() {
        println!("orion-wake needs Python 3, which isn't installed (on Fedora: sudo dnf install python3).");
        return Ok(false);
    }
    println!("orion-wake hears \"Orion\" with Vosk, an offline speech recognizer, which isn't installed yet.");
    if !has_python_module("pip") {
        let Some(argv) = crate::doctor::install_command(&[("pacman", "python-pip"), ("", "python3-pip")]) else {
            println!("Installing it needs pip; install python3-pip with your package manager.");
            return Ok(false);
        };
        if !ask.yes(&format!("Installing it needs pip. Install pip ({})?", argv.join(" ")), true)? || !run_shown(&argv) {
            return Ok(false);
        }
    }
    let argv: Vec<String> = ["python3", "-m", "pip", "install", "--user", "vosk"].map(String::from).into();
    if !ask.yes(&format!("Install Vosk for you ({})?", argv.join(" ")), true)? {
        return Ok(false);
    }
    if !run_shown(&argv) || !has_python_module("vosk") {
        println!("pip couldn't install Vosk; its message is above.");
        return Ok(false);
    }
    println!("✓ Vosk is installed.");
    Ok(true)
}

/// Run a command where its output shows; whether it worked.
fn run_shown(argv: &[String]) -> bool {
    Command::new(&argv[0]).args(&argv[1..]).status().is_ok_and(|s| s.success())
}

/// What the detector said.
#[derive(Debug, PartialEq)]
pub(crate) enum Detector {
    /// The microphone is open and it is listening.
    Ready,
    Heard(String),
    Wake,
    /// Anything else on stderr, such as an error or a download's progress.
    Said(String),
    Exited,
}

pub(crate) fn detector_line(line: &str, stdout: bool) -> Detector {
    match (stdout, line.trim()) {
        (true, "wake") => Detector::Wake,
        (false, "listening") => Detector::Ready,
        (false, l) if l.starts_with("heard: ") => Detector::Heard(l["heard: ".len()..].to_string()),
        (_, l) => Detector::Said(l.to_string()),
    }
}

/// Start the detector with `--verbose`; its lines arrive on the channel.
fn start_detector(bin: &Path, record: &str) -> Result<(std::process::Child, Receiver<Detector>)> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(bin);
    if !record.is_empty() {
        cmd.args(["--record", record]);
    }
    let mut child = cmd
        .arg("--verbose")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting {}", bin.display()))?;
    let (tx, rx) = mpsc::channel();
    let forward = |stream: Box<dyn std::io::Read + Send>, stdout: bool, tx: Sender<Detector>| {
        thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if tx.send(detector_line(&line, stdout)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Detector::Exited);
        });
    };
    forward(Box::new(child.stdout.take().context("no output from the detector")?), true, tx.clone());
    forward(Box::new(child.stderr.take().context("no output from the detector")?), false, tx);
    Ok((child, rx))
}

/// Have the person say the wake word until the detector hears it. Whether
/// it did.
fn hear_wake_word(ask: &mut Prompt, bin: &Path, record: &str) -> Result<bool> {
    println!("Starting orion-wake (the first time, Vosk downloads its small English model, about 40 MB)…");
    let (mut child, rx) = start_detector(bin, record)?;
    let stop = |child: &mut std::process::Child| {
        super::kill_group(child.id(), libc::SIGTERM);
        let _ = child.wait();
    };
    let started = Instant::now();
    loop {
        match rx.recv_timeout(WAKE_START.saturating_sub(started.elapsed())) {
            Ok(Detector::Ready) => break,
            Ok(Detector::Said(l)) if !l.is_empty() => println!("  {l}"),
            Ok(Detector::Exited) => {
                let _ = child.wait();
                println!("orion-wake stopped before it could listen; its message is above.");
                return Ok(false);
            }
            Ok(_) => {}
            Err(_) => {
                stop(&mut child);
                println!("orion-wake didn't start listening within {}s.", WAKE_START.as_secs());
                return Ok(false);
            }
        }
    }
    let mut tries = 0;
    let heard = loop {
        while rx.try_recv().is_ok() {}
        ask.line("Press Enter, then say \"Orion\". ")?;
        tries += 1;
        let deadline = Instant::now() + WAKE_TRY;
        let mut words = Vec::new();
        let outcome = loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Detector::Wake) => break Some(true),
                // Vosk repeats a partial word as it firms up.
                Ok(Detector::Heard(t)) if words.last() != Some(&t) => {
                    println!("  heard: {t}");
                    words.push(t);
                }
                Ok(Detector::Said(l)) if !l.is_empty() => println!("  {l}"),
                Ok(Detector::Exited) | Err(RecvTimeoutError::Disconnected) => break Some(false),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => break None,
            }
        };
        match outcome {
            Some(true) => {
                println!("✓ It heard \"Orion\"{}.", if tries > 1 { format!(" (try {tries})") } else { String::new() });
                break true;
            }
            Some(false) => {
                println!("orion-wake stopped; its message is above.");
                break false;
            }
            None => {
                println!("  Not heard{}.", if words.is_empty() { " (nothing reached it: is the microphone the same one as in step 1?)" } else { "" });
                if tries >= 2 {
                    println!("  Say it as on its own, a little louder or closer, with a short pause after it.");
                }
                if !ask.yes("Try again?", true)? {
                    break false;
                }
            }
        }
    };
    stop(&mut child);
    Ok(heard)
}

// ---------------------------------------------------------------------------
// 4. Your voice, and a first request.

/// A request that was transcribed, kept to time speech to text with.
pub(crate) struct Transcribed {
    wav: Vec<u8>,
    speech: Duration,
    took: Duration,
}

fn voice_and_request(ask: &mut Prompt, policy: &Policy, source: Option<&Path>) -> Result<Option<Transcribed>> {
    let v = &policy.voice;
    if v.verify_method != VerifyMethod::Builtin && super::neural::Ge2e::default_path().is_some_and(|p| !p.exists()) {
        if ask.yes("Download the neural speaker model first (about 16 MB from PyPI)? It tells people apart far better.", true)? {
            if let Err(e) = enroll::model_install(None) {
                println!("Couldn't install it ({e:#}); enrolling with the built-in method.");
            }
        }
        println!();
    }
    let people = Voiceprint::all()?;
    let me = speaker::default_name();
    let again = match people.as_slice() {
        [] => Some(None),
        [one] if ask.yes(&format!("{} is enrolled already. Enroll {} again?", cap(&one.name), cap(&one.name)), false)? => Some(Some(one.name.clone())),
        [_] => None,
        many if many.iter().any(|p| p.name == me) && ask.yes(&format!("{} people are enrolled. Enroll {} again?", many.len(), cap(&me)), false)? => {
            Some(Some(me.clone()))
        }
        _ => None,
    };
    if let Some(name) = again {
        enroll::enroll(v, name.as_deref())?;
    }
    println!("\nNow a real request, the way you'll use Orion.");
    request(ask, policy, source)
}

fn request(ask: &mut Prompt, policy: &Policy, source: Option<&Path>) -> Result<Option<Transcribed>> {
    let v = &policy.voice;
    let recorder = Recorder::detect(v)?;
    let transcriber = match Transcriber::from_config(v) {
        Ok(t) => t,
        Err(e) => {
            println!("Speech to text isn't set up: {e:#}. `orion doctor` helps.");
            return Ok(None);
        }
    };
    ask.line("Press Enter, then ask: \"Orion, what time is it?\" (or anything you like). ")?;
    let samples = enroll::record(&recorder)?;
    let wav = super::wav(&samples, recorder.rate());
    let speech = Duration::from_secs_f32(samples.len() as f32 / recorder.rate() as f32);
    let start = Instant::now();
    let text = match transcriber.transcribe(&wav) {
        Ok(t) => t,
        Err(e) => {
            println!("Speech to text failed ({}): {e:#}. `orion doctor` checks it.", transcriber.describe());
            return Ok(None);
        }
    };
    let heard = Transcribed { wav, speech, took: start.elapsed() };
    println!("Heard: \"{}\" ({:.1}s to transcribe {:.1}s of speech).", text.trim(), heard.took.as_secs_f32(), speech.as_secs_f32());
    let wanted = super::wake::match_wake(&text, &v.wake_phrase, &v.wake_aliases).unwrap_or_else(|| text.trim().to_string());
    if wanted.is_empty() {
        println!("That was only the wake word, so there's nothing to ask.");
        return Ok(Some(heard));
    }
    let by = match v.judge() {
        Ok(Some(judge)) => match judge.identify(&samples, recorder.rate()) {
            Heard::Person(id) => {
                println!("That was {}: {}.", cap(&id.name), id.score);
                Some(v.who(&id.name))
            }
            Heard::Nobody(_) => {
                println!("That didn't sound like anyone enrolled, so the wake word would have ignored it; `orion voice test` helps find out why.");
                None
            }
            Heard::TooShort => None,
        },
        _ => None,
    };
    ask_model(ask, policy, source, wanted, by);
    Ok(Some(heard))
}

/// Send one request through the conversation, as `orion listen` would,
/// print and speak the answer.
fn ask_model(ask: &mut Prompt, policy: &Policy, source: Option<&Path>, text: String, by: Option<super::Person>) {
    let orion = Orion::new(policy.clone()).through_daemon(source, "orion voice setup");
    let (session, saver, _) = match crate::chat::terminal::prepare(&orion, None, None, None) {
        Ok(s) => s,
        Err(e) => {
            println!("Couldn't reach a model: {e:#}. `orion setup` connects one.");
            return;
        }
    };
    let (to_agent, agent_rx) = mpsc::channel();
    let (agent_tx, from_agent) = mpsc::channel();
    let mut reply = String::new();
    thread::scope(|scope| {
        scope.spawn(|| agent::run(&orion, session, saver, agent_rx, agent_tx));
        // The conversation thread reports in once before any work.
        while !matches!(from_agent.recv(), Ok(FromAgent::Done) | Err(_)) {}
        let _ = to_agent.send(match by {
            Some(by) => ToAgent::Spoken { text, by },
            None => ToAgent::Turn(text),
        });
        for ev in from_agent.iter() {
            match ev {
                FromAgent::Text(t) => reply.push_str(&t),
                FromAgent::Route { to, model, .. } => println!("(asking {to}, {model})"),
                FromAgent::ToolStart { name, .. } => println!("(using {name})"),
                FromAgent::Notice(n) | FromAgent::Wait { text: n, .. } => println!("({n})"),
                FromAgent::Error(e) => println!("The model failed: {e}"),
                FromAgent::Approve { tool, preview, reply } => {
                    let yes = ask.yes(&format!("Allow {tool}? {preview}\n"), false).unwrap_or(false);
                    let _ = reply.send(if yes { Answer::Yes } else { Answer::No });
                }
                FromAgent::Confirm { question, reply } => {
                    let _ = reply.send(ask.yes(&question, false).unwrap_or(false));
                }
                FromAgent::Done | FromAgent::Quit => break,
                _ => {}
            }
        }
        // Dropping the sender ends the conversation thread.
        drop(to_agent);
    });
    let reply = reply.trim();
    if reply.is_empty() {
        return;
    }
    println!("Orion: {reply}");
    match Speaker::from_config(&policy.voice) {
        Ok(s) => {
            s.say(reply);
            thread::sleep(Duration::from_millis(300));
            let start = Instant::now();
            while s.is_speaking() && start.elapsed() < Duration::from_secs(60) {
                thread::sleep(Duration::from_millis(100));
            }
        }
        Err(e) => println!("(Replies can't be spoken yet: {e:#})"),
    }
    println!("✓ Orion heard you and answered.");
}

// ---------------------------------------------------------------------------
// 5. Speech to text.

/// How long the wait after speaking feels.
#[derive(Debug, PartialEq)]
pub(crate) enum Pace {
    Quick,
    /// Usable, but the pause shows.
    Noticeable,
    Slow,
}

pub(crate) fn pace(took: Duration) -> Pace {
    match took.as_secs_f32() {
        t if t <= 1.5 => Pace::Quick,
        t if t <= 3.0 => Pace::Noticeable,
        _ => Pace::Slow,
    }
}

/// Whisper sizes, largest first, as they appear in model names.
const SIZES: [&str; 9] = ["large-v3-turbo", "large-v3", "large-v2", "large-v1", "large", "medium", "small", "base", "tiny"];

/// The next smaller Whisper model than `model` (a faster-whisper name, or
/// a whisper.cpp ggml file), English-only when the language is English.
pub(crate) fn smaller_model(model: &str, english: bool) -> Option<String> {
    let lower = model.to_ascii_lowercase();
    if lower.contains("distil") {
        return None;
    }
    let (at, size) = SIZES.iter().find_map(|s| lower.find(s).map(|at| (at, *s)))?;
    let to = match size {
        "small" => "base",
        "base" => "tiny",
        "tiny" => return None,
        // A large or medium model is far too slow on a processor.
        _ => "small",
    };
    let rest = &model[at + size.len()..];
    let (had_en, rest) = match rest.strip_prefix(".en") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let en = if english || had_en { ".en" } else { "" };
    Some(format!("{}{to}{en}{rest}", &model[..at]))
}

fn speed(ask: &mut Prompt, v: &VoiceConfig, heard: Option<Transcribed>) -> Result<()> {
    let transcriber = match Transcriber::from_config(v) {
        Ok(t) => t,
        Err(e) => {
            println!("Speech to text isn't set up: {e:#}.");
            return Ok(());
        }
    };
    let mut heard = match heard {
        Some(h) => h,
        None => {
            let recorder = Recorder::detect(v)?;
            ask.line("Press Enter, then say: \"Orion, how much space is left on my disk?\" ")?;
            let samples = enroll::record(&recorder)?;
            let wav = super::wav(&samples, recorder.rate());
            let start = Instant::now();
            transcriber.transcribe(&wav).with_context(|| format!("speech to text failed ({})", transcriber.describe()))?;
            Transcribed { wav, speech: Duration::from_secs_f32(samples.len() as f32 / recorder.rate() as f32), took: start.elapsed() }
        }
    };
    // A server loads its model on the first request, which isn't the wait
    // you'll have later.
    if pace(heard.took) != Pace::Quick {
        let start = Instant::now();
        if transcriber.transcribe(&heard.wav).is_ok() {
            heard.took = heard.took.min(start.elapsed());
        }
    }
    let secs = heard.took.as_secs_f32();
    let what = transcriber.describe();
    println!("{what} takes {secs:.1}s for {:.1}s of speech: that's the pause between finishing a request and Orion starting on it.", heard.speech.as_secs_f32());
    match pace(heard.took) {
        Pace::Quick => {
            println!("✓ Fast enough on this processor.");
            return Ok(());
        }
        Pace::Noticeable => println!("It works, but you'll notice the pause."),
        Pace::Slow => println!("That's slow enough to get in the way."),
    }
    let english = v.language.starts_with("en");
    match &transcriber {
        Transcriber::Http { .. } => match smaller_model(&v.transcribe_model, english) {
            Some(m) => println!(
                "A smaller model is faster, a little less accurate. Under [voice]:\n  transcribe_model = \"{m}\"\n(your server may need to download it first)."
            ),
            None => println!("{} is already about as small as Whisper goes.", v.transcribe_model),
        },
        Transcriber::Command(cmd) => match cmd.split_whitespace().find_map(|w| smaller_model(w, english).map(|m| (w, m))) {
            Some((old, new)) => println!(
                "A smaller model is faster, a little less accurate: download {} and use it in voice.transcribe_command instead of {old}.",
                Path::new(&new).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or(new.clone())
            ),
            None => println!("A smaller Whisper model (base.en or tiny.en) in voice.transcribe_command is faster."),
        },
    }
    if v.language.is_empty() {
        println!("If you only speak English to Orion, language = \"en\" under [voice] saves detecting it, and allows the faster .en models.");
    }
    let gpus = crate::tools::gpu::inventory();
    use crate::tools::gpu::{Kind, Vendor};
    if let Some(g) = gpus.iter().find(|g| g.vendor == Vendor::Nvidia) {
        println!("This machine has {}: speech to text on it is many times faster. Run speaches' CUDA image instead of the CPU one, \
                  with the GPU passed through (podman: --device nvidia.com/gpu=all), at the same transcribe_url.", g.name);
    } else if let Some(g) = gpus.iter().find(|g| g.kind == Kind::Discrete || g.vendor == Vendor::Amd) {
        println!("This machine has {}: whisper.cpp built with Vulkan runs on it (voice.transcribe_command = \"whisper-cli -m <model> -nt -np -f {{wav}}\").", g.name);
    } else {
        println!("There's no GPU here that would help, so a smaller model is the way.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_run_in_order_from_any_of_them() {
        let from = |s: Step| Step::ALL.iter().filter(|x| **x >= s).count();
        assert_eq!(from(Step::Mic), 5);
        assert_eq!(from(Step::Wake), 3);
        assert_eq!(from(Step::Speed), 1);
    }

    #[test]
    fn hears_whether_the_microphone_works() {
        assert_eq!(hear_mic(&[0.0, 1.0, 2.0]), Mic::Silent);
        assert!(matches!(hear_mic(&[20.0; 40]), Mic::Quiet(_)));
        let mut counting = vec![30.0; 20];
        counting.extend([2500.0; 20]);
        assert!(matches!(hear_mic(&counting), Mic::Good(l) if l >= 2000.0));
    }

    #[test]
    fn reads_the_detector() {
        assert_eq!(detector_line("wake", true), Detector::Wake);
        assert_eq!(detector_line("listening", false), Detector::Ready);
        assert_eq!(detector_line("heard: or ryan", false), Detector::Heard("or ryan".into()));
        assert_eq!(detector_line("wake", false), Detector::Said("wake".into()));
        assert_eq!(detector_line("orion-wake: Vosk isn't installed: pip install --user vosk", false), Detector::Said("orion-wake: Vosk isn't installed: pip install --user vosk".into()));
    }

    #[test]
    fn the_detector_signals_when_it_listens() {
        // detector_line waits for this line; the script must print it.
        assert!(ORION_WAKE.contains("log(\"listening\")"));
    }

    #[test]
    fn judges_the_pause() {
        assert_eq!(pace(Duration::from_millis(900)), Pace::Quick);
        assert_eq!(pace(Duration::from_millis(2500)), Pace::Noticeable);
        assert_eq!(pace(Duration::from_secs(6)), Pace::Slow);
    }

    #[test]
    fn suggests_a_smaller_model() {
        assert_eq!(smaller_model("Systran/faster-whisper-small", false).as_deref(), Some("Systran/faster-whisper-base"));
        assert_eq!(smaller_model("Systran/faster-whisper-small", true).as_deref(), Some("Systran/faster-whisper-base.en"));
        assert_eq!(smaller_model("Systran/faster-whisper-small.en", true).as_deref(), Some("Systran/faster-whisper-base.en"));
        assert_eq!(smaller_model("Systran/faster-whisper-small.en", false).as_deref(), Some("Systran/faster-whisper-base.en"));
        assert_eq!(smaller_model("Systran/faster-whisper-large-v3", true).as_deref(), Some("Systran/faster-whisper-small.en"));
        assert_eq!(smaller_model("Systran/faster-whisper-medium", false).as_deref(), Some("Systran/faster-whisper-small"));
        assert_eq!(smaller_model("~/models/ggml-base.en.bin", true).as_deref(), Some("~/models/ggml-tiny.en.bin"));
        assert_eq!(smaller_model("Systran/faster-whisper-tiny", true), None);
        assert_eq!(smaller_model("Systran/faster-distil-whisper-large-v3", true), None);
        assert_eq!(smaller_model("-nt", true), None);
    }
}
