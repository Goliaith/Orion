# Setup, doctor and self-tuning

Three things keep Orion's settings right without you editing files by hand:

- [`orion setup`](#orion-setup) connects a model and chooses a trust level.
- [`orion doctor`](#orion-doctor) checks every part and offers fixes.
- [Self-tuning](#self-tuning): Orion notices settings that keep getting in the way and suggests changes.

## orion setup

Run it once after installing, and again whenever you want to change model or trust level.

```sh
orion setup
```

What it does, step by step:

1. **Asks which model to use:** Claude, Gemini (a Google AI Studio key; the free tier works), or a server on this machine (Ollama, llama.cpp, vLLM, LM Studio).
2. **Takes the key** with the typing hidden. Stray spaces, line breaks and invisible characters from the paste are removed.
3. **Checks the key** with a live call, picks a model the key really offers, and asks it to say "ok".
4. **Asks your [trust level](safety.md#trust-levels):** how much Orion may do without asking.
5. **Writes the files:**
   - `~/.config/orion/orion.toml`, the policy. Only the model and trust settings change, your comments stay, and the old file is kept as `orion.toml.bak`.
   - `~/.config/orion/env`, the key, readable only by you. Every `orion` command reads it at start, so the key works right away, with no new shell.
6. **Offers to restart** a running Orion service, so it sees the new key.

With Claude, it also offers a local model server it finds, so private and simple messages can stay on this machine.

### Gemini's free tier

Pick Gemini when setup asks, and paste a key from [aistudio.google.com/apikey](https://aistudio.google.com/apikey). Setup lists the models your key offers, keeps only the ones that can chat and use tools, and picks one.

Good to know:

- **The free tier allows only a few requests a minute,** and each tool Orion runs is one more request. When Google says to slow down, Orion waits and counts it down: `waiting on Gemini's rate limit (17s, retry 1 of 3)`.
- **Orion switches instead of waiting.** While your model is limited, the request goes to another Gemini model (each has its own allowance), or to Claude or OpenAI if you have a key. See [switching cloud models](chat.md#switching-cloud-models-when-one-is-rate-limited).
- **If waits keep happening,** Orion suggests a lighter model and less thinking ([self-tuning](#self-tuning)).
- **The [fast path](chat.md#the-fast-path) helps:** "volume up" or "open firefox" needs no request at all.
- **Private things stay on your machine.** Gemini isn't on your machine, so Orion never sends it private notes, private journal entries or private files, and asks before sending a message that looks private.

### Shortcuts

```sh
orion setup --trust everyday                              # change only the trust level: ask, everyday or hands-off
orion setup --provider gemini                             # skip the "which model" question
orion setup --provider gemini --model gemini-3.8-flash    # and choose the model
orion setup --provider local --base-url http://127.0.0.1:8080/v1
GEMINI_API_KEY=… orion setup --provider gemini --yes      # take every default, with the key from the environment
```

## orion doctor

When something doesn't work, start here.

```sh
orion doctor             # check, and ask before each fix
orion doctor --yes       # apply every fix without asking
orion doctor --no-fix    # only report
```

It checks, in order:

1. **The policy:** which file Orion reads (and a misspelled one nearby), whether it loads (curly quotes, unknown keys), and which [trust level](safety.md#trust-levels) it's on.
2. **Keys:** the env file's permissions, `export` lines that don't belong, stray characters, and an older key your shell exports that wins over the file.
3. **Models:** each configured model, with a live call. A rejected key or a retired or misspelled model name shows here.
4. **Voice:** the microphone, the transcriber, and the wake-word detector.
5. **The sandbox:** whether bubblewrap works.
6. **Root access:** the privileged helper's group and socket, including a group you joined but haven't logged in again for.
7. **The daemon:** whether the background service has your current keys.

For each problem it says what to do, and offers to do it. It keeps a `.bak` of any file it changes, and exits with 1 while something is still broken.

### The trust level in doctor

`orion doctor` says which trust level the policy is on. If you changed the policy by hand, it names the nearest level and how yours differs. It also names any `[tools]` entry that lets a tool run unasked beyond the level.

## Self-tuning

Orion watches for settings that keep getting in the way. When it sees one, it tells you what it saw and which change would fix it. **Nothing changes until you approve it.**

You see a suggestion in three places:

- as a finding in `orion status`, in the chat and in the shell, like "Gemini keeps rate limiting requests";
- when you ask, for example "is anything slowing you down?";
- in `orion call settings_check`.

### What it notices

| It notices | It suggests |
|---|---|
| The chat model was rate limited 3 times in a day | a lighter model (Pro to Flash, Flash to Flash-Lite), and `reasoning_effort = "low"` |
| Speech took at least half as long to turn into text as to say (and 2 seconds or more), 3 times in a row | the next smaller Whisper model (`medium` to `small`), and the GPU when the machine has one |
| You approved a tool 5 times in 30 days and never said no | pre-approving it (`allow`) |

### How a change is made

1. You say yes in the chat.
2. The AI calls `settings_tune` with the suggestion's name. The values come from what Orion measured, not from the AI.
3. `settings_tune` always asks you, even if you pre-approved it.
4. The change is written to `~/.config/orion/tuning.toml`, which applies on top of `orion.toml`. The daemon picks it up within seconds.

### What it can't touch

`tuning.toml` can hold only:

- `[chat.local] model` and `reasoning_effort`,
- `[voice] transcribe_model`,
- `[tools]` entries set to `allow`, for `write` tools only.

It can't pre-approve a tool with a list of its own (`proc_run`'s commands, `pkg_install`'s packages, `svc_control`'s units), can't override a `[tools]` entry in your `orion.toml`, and does nothing if `[defaults] write` is anything but `ask`. A file that breaks these rules is ignored.

### Undo

Ask Orion to undo the setting, or delete `~/.config/orion/tuning.toml`.

### What it keeps

When requests were rate limited and how long transcriptions took, in `~/.local/state/orion/tuning.json`, and the approvals already in the audit log. Never what was said.

To stop these suggestions, add them to `[watch] ignore`, for example `"tune:approve:*"`.

### Voice tuning

Listening has its own tuning (background noise, the wake word heard as something else). It works the same way, with `voice_check` and `voice_tune`, and writes to `~/.config/orion/voice-tuning.toml`. See [Voice tuning](voice.md#voice-tuning).
