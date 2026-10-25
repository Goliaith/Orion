# Voice

Talk to Orion and hear it answer. Everything runs on your machine unless you point it elsewhere.

- [What you need](#what-you-need)
- [Set it up: `orion voice setup`](#set-it-up-orion-voice-setup)
- [Push to talk](#push-to-talk)
- [Spoken replies](#spoken-replies)
- [The wake word](#the-wake-word)
- [Enrolling voices](#enrolling-voices)
- [Hands-free mode: `orion listen`](#hands-free-mode-orion-listen)
- [Voice tuning](#voice-tuning)
- [Settings](#settings)

## What you need

Three parts. `/voice` in the chat shows which ones Orion found.

| Part | What to install | Notes |
|---|---|---|
| Microphone recorder | `pulseaudio-utils` (for `parecord`; Fedora has it), or `alsa-utils`, `sox` or `ffmpeg` | Orion uses the first it finds |
| Speech to text | a Whisper server on `http://127.0.0.1:8000/v1`, such as [speaches](https://github.com/speaches-ai/speaches), LocalAI or whisper.cpp's server; or whisper.cpp's `whisper-cli` as a command | set `transcribe_url` or `transcribe_command` under `[voice]` |
| Text to speech | `espeak-ng` (`sudo dnf install espeak-ng`), or [piper](https://github.com/rhasspy/piper), or a speech server such as Kokoro-FastAPI | set `speak_command` or `speak_url` to choose |

With whisper.cpp and no server, for example:

```toml
[voice]
transcribe_command = "whisper-cli -m ~/models/ggml-base.en.bin -nt -np -f {wav}"
```

## Set it up: `orion voice setup`

With speech to text ready, run:

```sh
orion voice setup
```

It walks you through five steps, about five minutes in all, and saves what each step finds:

1. **The microphone.** Count to five. It checks the recording isn't silent or very quiet. If it is, it tells you where to fix it (in KDE: System Settings → Sound → Input Devices).
2. **The room's noise.** Five seconds of quiet, then five of talking. The levels it picks are saved in `~/.config/orion/voice-tuning.toml`.
3. **The wake word.** It installs the wake-word detector as `~/.local/bin/orion-wake`, and Vosk with `pip install --user vosk` once you say yes (and pip, if it's missing). Then you say "Orion" until it hears you, and it writes `wake_command` into `orion.toml` (keeping the old file as `orion.toml.bak`).
4. **Your voice.** It offers the neural speaker model, enrolls your voice with six short phrases, then sends one real request ("Orion, what time is it?") to your model and speaks the answer.
5. **Speed.** It times speech to text. If the pause is noticeable on your processor, it names a smaller Whisper model, and says whether a GPU here would be faster.

To start at a later step: `orion voice setup --from noise` (or `wake`, `enroll`, `speed`).

Each step is also a command of its own:

```sh
orion voice noise             # step 2
orion voice model install     # the neural speaker model, about 16 MB, once
orion voice enroll            # step 4: your voice
orion voice enroll --name ana # someone else's
orion voice test              # who does Orion think is speaking?
orion voice status            # who is enrolled, and what each may do
orion voice forget ana        # forget a voice (--all for everyone)
```

## Push to talk

1. Press **Ctrl-T** in the chat (or the mic button in the shell).
2. Talk. A level meter shows Orion is listening.
3. Pause, and the recording ends. (Or press Ctrl-T again; Esc cancels.)
4. Your words appear in the message box. Check them and press Enter.

```
┌ ● listening ▁▁▁▆▆▆▆▆▆▅▅▅ 1s · pause or Ctrl-T to finish · Esc cancels ─┐
│›                                                                      │
└───────────────────────────────────────────────────────────────────────┘
```

Set `send_after_transcribe = true` under `[voice]` to send right away, without the check.

## Spoken replies

- `/voice speak on` makes Orion read its replies aloud, sentence by sentence as they arrive.
- Code is left on screen, and Markdown symbols aren't read out.
- Ctrl-C, or starting your next message, stops it.
- `/voice test` says a test sentence.

## The wake word

Turn it on with **Alt-W**, `/voice wake on`, or `wake = true` under `[voice]`. It works in the full-screen chat, the shell and `orion listen`.

- **"Orion, what's using the GPU?"** is sent at once, as if you typed it.
- **A bare "Orion"** plays a chime, and the next thing you say is the request.
- **"Hey Orion"** and **"OK Orion"** work too. The name only counts at the start, so "I read about Orion" doesn't wake it.
- **While the AI is working,** a new request waits its turn, and **"Orion, stop"** interrupts.
- **Spoken "no"** declines an approval. **Spoken "yes"** approves only if you set `wake_approvals = true`, because a TV or video nearby could say it. It never approves `root_run`, `sys_edit` or `proc_run` outside the sandbox: those need a screen.

### The wake-word detector

`orion voice setup` installs one for you: [`contrib/orion-wake`](../contrib/orion-wake). It listens all the time on its own, using little power, and nothing is turned into text until it hears the word. It has two offline engines:

- **Vosk** (the default) hears "Orion" with no training. Its small English model (about 40 MB) downloads on first use.
- **[openWakeWord](https://github.com/dscripka/openWakeWord)** copes better with a noisy room. It uses a trained model, such as the ready-made `hey_jarvis`, or your own. Install it with `pip install --user openwakeword onnxruntime`.

To set it up by hand:

```sh
install -Dm 755 contrib/orion-wake ~/.local/bin/orion-wake
pip install --user vosk
orion-wake --verbose              # say "Orion": it prints what it hears, then "wake"
```

```toml
[voice]
wake_command = "~/.local/bin/orion-wake"
# wake_command = "~/.local/bin/orion-wake --engine openwakeword --model hey_jarvis"
```

With openWakeWord, say the model's phrase ("Hey Jarvis" for `hey_jarvis`).

### Without a detector

Orion can also listen by cutting what it hears into short pieces and transcribing the first 3 seconds of each, looking for the name. It allows for the ways a transcriber misspells "Orion" ("O'Ryan", "Orien"), and `wake_aliases` adds your own. This costs more power on a CPU than the detector.

- **It stays local.** It refuses to start if your transcriber is on another machine, since everything said near the microphone would go there. `wake_allow_remote = true` allows it.
- **Your own phrase:** `wake_phrase = "hey computer"`. A short, distinctive name works best.

### Good to know

- The microphone closes as soon as wake listening stops, even if Orion is killed.
- Use PipeWire or PulseAudio (`parecord`), so the detector and Orion can share the microphone. Fedora KDE already does.
- Run only one listener: wake listening in a chat or shell, or `orion listen`, not both.

## Enrolling voices

Enroll your voice, and those of the people you live with, and only you can wake Orion. Others, and the TV, are ignored.

```sh
orion voice model install       # the neural speaker model (recommended), once
orion voice enroll              # you: six short phrases, about a minute
orion voice enroll --name ana   # and anyone else
orion voice test                # "That's Ana: similarity 0.90 (needs at least 0.80)"
```

- **Other voices are never transcribed.** Each piece of speech is checked first, and one in an unknown voice is dropped.
- **Orion knows who is asking.** The AI sees "Ana (by voice): what's using the GPU?", and the audit log names Ana on every call her request makes.
- **Each person has their own** conversations and [memory](memory.md).
- **Enroll where you usually talk,** with the same microphone, and enroll again if that changes.
- **It keeps other voices out, but it isn't proof of identity.** A recording of you gets through. That's why spoken approval stays off unless you turn it on.

### What each person may do

Set it under `[voice.people.<name>]` in the policy:

```toml
[voice.people.kid]
tools = "read"      # "all" (default), "read" (only look), or "none" (just talk)
approve = false     # may not answer approvals by voice
```

A tool someone may not use is refused before anyone is asked. Someone who may not approve hears "Sorry, Kid, you can't approve that," and the question stays open for someone who can.

### The two methods

Orion uses the better one it has (`verify_method = "auto"`).

- **The neural speaker model** (recommended). The GE2E speaker encoder with [Resemblyzer](https://github.com/resemble-ai/Resemblyzer)'s weights, run inside Orion in plain Rust. A voice passes at a similarity of 0.80 or more (`verify_similarity`; higher is stricter). `orion voice model install` downloads it from PyPI and checks its checksum. Enroll again after installing it. It runs in about 0.1 s for 3 seconds of speech, and tests hold it to the Python reference (cosine similarity above 0.999). On a real two-person recording, the enrolled speaker's own speech scored 0.84 and 0.90, and one of the other speaker's eight turns passed 0.80: a small, noisy test, not a benchmark.
- **The built-in method** (no download). Good at telling microphones and rooms apart, and keeping out the TV, but not at telling two people in the same room apart. Its limit is `verify_threshold` (1.2; lower is stricter).

Voiceprints are in `~/.local/state/orion/voices/`, one file per person, readable only by you, and they can't be turned back into speech.

## Hands-free mode: `orion listen`

Orion with no screen at all: it listens for the wake word, does what you ask, and speaks the answer.

```sh
orion listen                                          # until you stop it
systemctl --user enable --now orion-voice.service     # or always, as a service
journalctl --user -u orion-voice -f                   # what it heard and said
```

```
Orion is listening: say "orion"
you: how long has this machine been up?
⚙ proc_run {"command":"uptime"}
asking: May I run uptime?
you: yes. (approved)
orion: The machine says: up 35 min.
```

- **It works like the chat:** the same routing, tools, rules, log and saved conversations. A new conversation starts after 10 minutes of quiet (`listen_new_after_mins`).
- **Approvals are asked aloud** if `wake_approvals = true`: "May I restart ollama? Say Orion yes, or Orion no." Silence for 30 seconds counts as no. With it off (the default), anything that needs approval is declined, and Orion says so. Pre-approve what it may do freely with your [trust level](safety.md#trust-levels).
- **It names what's missing** at start (microphone, speech to text or speech), and retries the microphone every 10 seconds if it fails.

## Voice tuning

Orion notices when listening isn't going well, and offers to fix it:

- requests that keep running to the time limit, because background noise keeps it listening;
- the wake word heard as something else ("Oreo").

You hear about it as a finding ("Background noise keeps Orion listening"), or when you ask "how is my voice setup doing?". It says what it saw and which setting would help. Once you approve, `voice_tune` makes the change.

- **Changes go to `~/.config/orion/voice-tuning.toml`,** applied over `[voice]`. That file holds only voice settings (noise margin, speech level, pause, time limit, wake-word spellings), so the AI can't change the rest of your policy this way.
- **To undo,** ask Orion to reset them, or delete the file.
- **What it keeps:** the levels and endings of the last 20 requests, and near-miss spellings of the wake word, in `~/.local/state/orion/voice.json`. Never audio or anything else that was said.

To measure the room yourself, run `orion voice noise`: it listens to the room for 5 seconds, then to you for 5, and suggests `noise_margin` and `speech_level`.

Other settings Orion tunes (rate limits, slow speech to text) are in [self-tuning](setup.md#self-tuning).

## Settings

All under `[voice]` in `~/.config/orion/orion.toml`. [`orion.example.toml`](../orion.example.toml) lists every one.

| Setting | What it does |
|---|---|
| `record_command` | your own recorder (16 kHz mono 16-bit PCM on stdout) |
| `transcribe_url`, `transcribe_model`, `transcribe_command` | speech to text |
| `speak_command`, `speak_url` | text to speech |
| `noise_margin`, `speech_level` | how loud speech must be, compared with the room |
| `send_after_transcribe` | send push-to-talk without the check |
| `wake`, `wake_command`, `wake_phrase`, `wake_aliases`, `wake_allow_remote` | the wake word |
| `wake_approvals` | allow a spoken "yes" |
| `verify_speaker`, `verify_method`, `verify_similarity`, `verify_threshold` | enrolled voices |
| `listen_new_after_mins` | when `orion listen` starts a new conversation |

Nothing leaves the machine unless you point `transcribe_url` or `speak_url` at another computer.
