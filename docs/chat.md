# Chatting with Orion

- [Start a chat](#start-a-chat)
- [What you see](#what-you-see)
- [Approvals](#approvals)
- [Slash commands](#slash-commands)
- [Typing `orion` commands in the chat](#typing-orion-commands-in-the-chat)
- [The fast path](#the-fast-path)
- [When a model makes Orion wait](#when-a-model-makes-orion-wait)
- [Switching cloud models when one is rate limited](#switching-cloud-models-when-one-is-rate-limited)
- [Hotkeys](#hotkeys)
- [The full-screen view](#the-full-screen-view)
- [The graphical shell](#the-graphical-shell)
- [Saved conversations](#saved-conversations)
- [Choosing a model: automatic routing](#choosing-a-model-automatic-routing)
- [Local models](#local-models)

## Start a chat

First connect a model with [`orion setup`](setup.md#orion-setup). Then:

```sh
orion chat
```

Type what you want in plain words: "what's using my disk?", "are there security updates?", "install Discord", "mount the USB drive". The AI picks the tools and shows each one as it runs.

Useful options:

```sh
orion chat --resume        # continue the last conversation
orion chat --plain         # line by line instead of full screen
orion chat --local         # only the model on this machine
orion chat --claude        # only Claude
orion chat --auto          # route each message (the default)
```

## What you see

```
orion auto › what's eating my memory? write a short report

I'll check the hardware and the biggest processes.
  ⚙ system_info
    ✓ 0ms
  ⚙ proc_list {"limit":5,"sort":"memory"}
    ✓ 3ms
  ⚙ fs_write {"content":"…","path":"~/orion-workspace/memory.txt"}
  ? Approve fs_write?
    { "content": "…", "path": "~/orion-workspace/memory.txt" }
  [y]es / [n]o / [a]lways for this session › y
    ✓ 1ms
Firefox is using 3.1 GB of your 16 GB. I wrote the details to ~/orion-workspace/memory.txt.
```

- **Replies stream** as they are written. The model's reasoning and progress notes show dimmed.
- **Every tool call is shown**, with its arguments and whether it worked.
- **Anything that changes the machine** shows its full arguments and asks first.

## Approvals

When a call needs your OK:

- **`y`** runs this one call.
- **`n`** declines it, and the AI is told.
- **`a`** approves this tool for the rest of the session. (For `root_run`, `sys_edit` and `proc_run` outside the sandbox, `a` approves only this one call.)
- **Ctrl-C** counts as no.

Orion runs its hard checks first, so you're never asked about a call that would be refused anyway. To stop being asked about something routine, raise your [trust level](safety.md#trust-levels) or pre-approve it in the [policy](safety.md#the-policy-file).

## Slash commands

| Command | What it does |
|---|---|
| `/help` | list the commands |
| `/status` | the daemon, the sandbox, and what needs attention |
| `/tools` | what the AI can do, and what needs your approval |
| `@claude …`, `@local …` | send one message to that model |
| `/use auto\|claude\|local` | route automatically, or stick to one model |
| `/route <text>` | show where a message would go, without sending it |
| `/model [claude\|local] [id]`, `/models` | show or switch models; list the local server's models |
| `/effort <level>` | Claude's effort: low, medium, high, xhigh, max |
| `/usage` | tokens used this session |
| `/new`, `/history`, `/resume <n\|id>`, `/title <text>` | start, list, continue or rename conversations |
| `/memory`, `/memory forget <id>`, `/memory clear` | what Orion remembers about you ([memory](memory.md)) |
| `/journal`, `/journal <topic>`, `/journal search <words>` | the AI's journal ([memory](memory.md#the-journal)) |
| `/voice`, `/voice speak on\|off`, `/voice test`, `/voice wake on\|off` | voice ([voice](voice.md)) |
| `/keys` | the hotkeys |
| `/exit` | leave |

## Typing `orion` commands in the chat

If you type an `orion` command into the chat by habit, Orion handles it instead of sending it to the AI.

**These run the matching slash command:**

| You type | Orion runs |
|---|---|
| `orion` or `orion help` | `/help` |
| `orion status` | `/status` |
| `orion tools` | `/tools` |
| `orion memory` | `/memory` |
| `orion conversations` | `/history` |

**These get a short note on where they belong**, because they need a terminal: `orion doctor`, `orion setup`, `orion voice …`, `orion audit`, `orion call …`, `orion serve`, `orion daemon`, `orion approvals`, and `orion memory forget` or `clear` (the note says which slash command to type, so forgetting stays something you do on purpose).

**Messages that just start with the name still go to the AI.** "Orion, what time is it?", "orion status of my disks please" and "orion help me find my photos" are messages, not commands.

## The fast path

Simple requests don't need a model. Orion answers them by calling the tool directly, which is faster and saves requests (handy on Gemini's free tier).

| You say | Orion does |
|---|---|
| "volume up", "louder", "set volume to 40", "mute", "unmute" | changes the volume |
| "brightness 60%", "brighter", "dim the screen" | changes the brightness |
| "status", "system status" | shows a one-line summary of the machine |
| "open firefox", "launch spotify", "open ~/Documents/report.pdf", "open https://…" | opens the app, file or link |

- Small words around them are fine: "please turn the volume up", "Orion, mute".
- **The usual rules apply.** The call goes through the same policy, approvals and audit log as a model's call would.
- **The models are told.** Both models hear about it the next time they answer, so the conversation still makes sense.
- **Anything else goes to a model as usual,** such as a different phrasing or an app that isn't installed.
- **To skip it,** start with `@claude` or `@local`.
- **To check,** `/route <text>` shows whether a message takes the fast path.
- **To turn it off,** set `fast_path = false` under `[chat]`.

## When a model makes Orion wait

When a model asks Orion to slow down (a rate limit), the chat says so and counts down:

```
waiting on Gemini's rate limit (17s, retry 1 of 3) · 15 requests in the last minute
```

Where the API reports how many requests you have left (Claude and most OpenAI-style servers do; Gemini doesn't), the notice says that instead, and Orion warns once a minute when you are nearly out. The full-screen view counts down in its status line, the shell next to the notice, and `orion listen` says it aloud when the wait is long.

With a hosted model such as Gemini, Orion usually doesn't wait at all: it [switches to another cloud model](#switching-cloud-models-when-one-is-rate-limited) and waits only when every one of them is limited. If waits keep happening, Orion suggests a lighter model. See [self-tuning](setup.md#self-tuning).

## Switching cloud models when one is rate limited

When the hosted model you set up (Gemini, say) is rate limited, Orion sends the request to the next-best cloud model it has a key for, without waiting. Once your model's limit has passed, it goes back to it.

```
gemini-2.5-flash (Gemini) is rate limited for 17s; switching to gemini-flash-latest (Gemini).
gemini-2.5-flash (Gemini) is free again; back to it.
```

- **Only providers you have a key for take part.** With just a Gemini key, Orion switches between Gemini models: Flash, Flash-Lite and Pro each have their own free-tier allowance. With `ANTHROPIC_API_KEY` it can also use Claude Haiku and Sonnet, and with `OPENAI_API_KEY` an OpenAI model.
- **Speed and quality come first.** Your own model is always tried first. The others are ranked by speed and quality together. `prefer = "speed"` or `"quality"` under `[chat.cloud]` ranks by one of them first.
- **It can switch mid-answer.** Each request goes to the best model that is free, so a long job with many tool calls keeps going. The conversation carries over, tool calls and results included.
- **The usual rules apply.** Tool calls still go through the policy, approvals and audit log, whichever model makes them. Private messages, notes and files are never sent to any of these models, because none of them is on your machine.
- **When every model is limited,** Orion waits for the one that is free soonest and counts it down, as before.
- **A model that can't be used** (a key that's turned down, a model that doesn't exist) is left out for the rest of the session, with a note.
- **To check the order,** run `orion doctor`. The chat's model line shows which model will answer next.
- **To narrow it down,** list the providers allowed to take over: `providers = ["gemini"]`. To turn it off, set `enabled = false`.
- **To add a model,** such as one on Groq or OpenRouter, or to rescore a built-in one, add it under `models`. Speed and quality go from 1 to 5:

```toml
[chat.cloud]
models = [
  { provider = "groq", model = "llama-3.3-70b-versatile", speed = 5, quality = 3 },
]
```

Known providers (`gemini`, `claude`, `openai`, `groq`, `openrouter`, `mistral`, `deepseek`) need only the model's name. Any other provider takes a `base_url` and an `api_key_env`. Every one must speak the OpenAI-style chat API. Claude is reached through Anthropic's OpenAI-compatible endpoint here. The Claude you set up under `[chat.claude]` is separate, and still waits out its own limits.

## Hotkeys

| Key | What it does |
|---|---|
| **Ctrl-C** | Stop the AI mid-turn: a reply, a long think, a running program or an approval prompt. Press it twice at the prompt to quit. |
| **Shift-Tab** | Switch routing between auto, claude and local, keeping what you typed |
| **Ctrl-T** | Talk: listens until you pause, then puts your words in the message box ([voice](voice.md)) |
| **Alt-W** | Wake-word listening on or off |
| **Ctrl-O** | Show or hide reasoning |
| **Ctrl-B** | The machine panel (full screen) |
| **Alt-Enter**, **Ctrl-J** | New line |
| **PgUp**, **PgDn**, **End** | Scroll; jump back to the latest |
| **Ctrl-R**, **↑ ↓** | Search and browse earlier messages |
| **Ctrl-L** / **Ctrl-D** | Clear the screen / quit |

What Ctrl-C does underneath:

- A reply that was streaming is kept and marked as interrupted, so the model knows where it stopped.
- The network call is dropped, which stops the model on the server too.
- Programs the AI started are killed, with all their children.
- An open approval counts as no.
- An interrupt is never treated as a failure, so it never switches to the other model.
- A second Ctrl-C before the first is handled quits at once, in case something hangs.

## The full-screen view

In a terminal, `orion chat` opens full screen:

```
┌ Orion · why is my machine slow? save a note ──────────────────────────┐┌ Machine ───────────────────┐
│you     why is my machine slow? save a note                            ││ laptop                     │
│        → Claude (claude-opus-5-5) · needs root-cause reasoning        ││ Fedora Linux 44 (KDE)      │
│        ⚙ system_info   ✓ 0ms                                          ││ CPU  ██░░░░░░░░  18% ·8c   │
│        ⚙ proc_list {"limit":5,"sort":"memory"}  ✓ 4ms                 ││ RAM  ██████░░░░ 9.1/16G    │
│        ┌─ Approve fs_write?                                           ││ Routing                    │
│        │ { "path": "~/orion-workspace/notes.txt", … }                 ││ auto                       │
│        └─ approved                                                    ││ Session                    │
│claude  Mostly idle; Firefox holds most memory. I saved a note.        ││ claude in 6.2k · out 150   │
└───────────────────────────────────────────────────────────────────────┘└────────────────────────────┘
┌ message · Enter sends · Alt-Enter new line ─────────────────────────────────────────────────────────┐
│›                                                                                                    │
└─────────────────────────────────────────────────────────────────────────────────────────────────────┘
 AUTO   Shift-Tab routing · Ctrl-O reasoning · Ctrl-B panel · PgUp/PgDn scroll · /keys · Ctrl-D quit
```

- **The conversation** shows your messages, which model answered and why, reasoning (dimmed), each tool call with a live status, and replies. Code is highlighted.
- **Approvals** appear as cards. Answer with `y`, `n` or `a`.
- **The machine panel** (Ctrl-B) shows live CPU, memory, each NVIDIA GPU, the connected models, routing, token use, and what needs attention.
- **You can keep typing while the AI works.** Your next message sends when it's done.
- **The mouse wheel scrolls.** Hold Shift to select text in most terminals.

`orion chat --plain`, or `interface = "plain"` under `[chat]`, keeps the line-by-line chat. It's used automatically when the input or output isn't a terminal.

## The graphical shell

The same chat as a web page on this machine:

```sh
orion shell --open     # starts it and opens your browser
```

![The Orion shell: conversations on the left, the chat with routing, tool calls and an approval card in the middle, the machine panel on the right](shell.png)

It has everything the terminal chat has, plus:

- **A sidebar of saved conversations.** Click one to continue it.
- **An Auto / Claude / Local switch** in the header (Alt+M), and each reply labeled with the model that wrote it.
- **Approval cards** with Approve, Always allow this session, and Decline.
- **A machine panel:** CPU, memory, GPUs, models and token use.
- **Voice:** the mic button (Alt+T) and a button to read replies aloud.
- **Every tab shows the same conversation,** live.
- **Keys:** Enter sends, Shift+Enter makes a new line, Esc stops the AI, Alt+R shows reasoning.

**Run it in the background:** `systemctl --user enable --now orion-shell.service`. Its address is in `~/.local/state/orion/shell-url`.

**Make it the whole screen** on a machine just for Orion: `cage -- contrib/orion-kiosk` starts the shell and a full-screen browser on it ([install guide](install.md#the-graphical-shell)).

**It's locked down:**

- It listens on `127.0.0.1` only. From another machine, use an SSH tunnel.
- Each run gets a random token. The address carries it, and on first load it's swapped for a private cookie.
- Requests must name this server, which blocks DNS-rebinding attacks.
- Anything that changes something needs a special header that other web pages can't send.
- The page loads nothing from the internet, and model output is escaped before it's shown.

## Saved conversations

Every conversation is saved after each message, so closing the terminal or a crash loses nothing.

```sh
orion chat --resume               # continue the most recent
orion chat --resume 20261020-15   # or one by id (or the start of it)
orion conversations               # list them
orion conversations show <id>     # print one
orion conversations delete <id>   # delete one
```

In the chat: `/history` lists them, `/resume <n|id>` switches, `/new` starts fresh, `/title <text>` renames.

- **Each model's history is saved exactly as it was,** so a resumed conversation picks up with full context, not a summary.
- **The routing mode and models come back too.** If a model is gone, the conversation carries on with what's there.
- **Where:** `~/.local/state/orion/conversations/`, one file per conversation, readable only by you. Set `save_conversations = false` under `[chat]` to stop saving.
- **Each person has their own.** A spoken request from an enrolled voice goes to that person's conversation ([memory](memory.md)).

## Choosing a model: automatic routing

With two models (Claude and a local model, say), `auto` mode picks one for each message and says which and why:

```
orion auto › how full is my disk?
→ the local model (qwen3:8b) · short request

orion auto › plan a cleanup of my home directory
→ Claude (claude-opus-5-5) · complex: "plan"

orion auto › my bank password is hunter2, remember it
→ the local model (qwen3:8b) · private: mentions "password"
```

How it decides, in order:

1. **Simple requests take the [fast path](#the-fast-path)** and skip the models.
2. **Private stays local.** A message with a private word (password, bank, medical, …), something that looks like a key, or a private path (`~/.ssh`, …) goes to the model on this machine. With no local model, Orion asks before sending it anywhere else. No model can overrule this.
3. **Short follow-ups** ("yes", "go ahead") stay with the model that answered last.
4. **The local model judges the rest** by difficulty. If there's no local model, or it fails, keyword rules decide: code, words like "debug", "plan" or "explain", several questions, and long messages go to Claude.
5. **If the chosen model is down, the other answers.** Private messages never fall back to a hosted model.

**One conversation, two models.** Before a model answers, it's caught up on what the other one did. Private turns are never shown to Claude.

**Claude never sees private files.** When Claude is answering, a tool call that touches a private path is refused, whatever the policy says.

Controls:

- `@claude …` or `@local …` sends one message to that model.
- `/use auto|claude|local` or Shift-Tab switches mode.
- `/route <text>` shows where a message would go.
- The word lists, private paths and other rules are under `[chat.router]` in [`orion.example.toml`](../orion.example.toml).

**With Gemini,** routing uses the keyword rules, because asking Gemini to judge each message would spend a request.

## Local models

Orion can use a model running on your own computer. Nothing leaves the machine and no key is needed. Any server with the OpenAI-style chat API works:

| Runtime | Start it | `[chat.local] base_url` |
|---|---|---|
| [Ollama](https://ollama.com) | `ollama pull qwen3:8b` | `http://127.0.0.1:11434/v1` (default) |
| llama.cpp | `llama-server -m model.gguf --jinja` | `http://127.0.0.1:8080/v1` |
| vLLM | `vllm serve Qwen/Qwen3-8B --enable-auto-tool-choice --tool-call-parser hermes` | `http://127.0.0.1:8000/v1` |
| LM Studio | start the local server in the app | `http://127.0.0.1:1234/v1` |

```sh
orion chat --local                    # the first model the server lists
orion chat --local --model qwen3:8b   # a specific one
```

Tips:

- **Pick a model trained for tool calling:** Qwen3, Llama 3.1+, Mistral Small 3 or gpt-oss. Models under about 7B often use tools badly.
- **Give it a big enough context.** For Ollama, set `OLLAMA_CONTEXT_LENGTH=32768`.
- **A small judge makes routing faster:** `[chat.router] classifier_model = "qwen3:1.7b"`.
- **The same rules apply.** A local model gets the same tools, approvals and audit log as any other.

Gemini uses the same settings, pointed at Google. `orion setup` writes them for you; [the install guide](install.md#gemini-free-tier) shows them by hand.
