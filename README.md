# Orion

Orion lets an AI look after your Linux computer. You ask in plain words, by typing or by voice, and the AI does the work: it checks what's slowing the machine down, installs apps, applies updates, changes the volume, fixes settings in `/etc`, and more.

The AI never touches the computer directly. It can only use Orion's tools, each one is checked against your rules, anything risky asks you first, and every action is written to a log. Most changes can be undone.

```
you: what's eating my memory?
  ⚙ proc_list {"limit":5,"sort":"memory"}   ✓ 3ms
orion: Firefox is using 3.1 GB of your 16 GB. Want me to show which tabs?

you: install Discord
  ⚙ flatpak_install {"app":"com.discordapp.Discord"}
  ? Approve flatpak_install?  [y]es / [n]o / [a]lways › y
orion: Discord is installed. Open it?
```

## What it can do

- **Chat with you** in the terminal, in a full-screen view, or in a web page (`orion shell`).
- **Listen and talk.** Say "Orion, what's using the GPU?" and hear the answer. Only enrolled voices can wake it.
- **Use the AI you have.** Claude, Google's Gemini (the free tier works), or a model on your own machine (Ollama, llama.cpp, vLLM, LM Studio). Private messages stay on your machine. When a cloud model is rate limited, Orion switches to the next-best one you have a key for instead of waiting.
- **Run the machine.** Files, programs, services, packages, Flatpak apps, updates, Wi-Fi, the firewall, disks, GPUs, sound, screens, Bluetooth, power and the clock. See [all tools](docs/tools.md).
- **Watch for trouble.** Failed services, full disks, security updates, hot parts and failing drives, with a desktop notification.
- **Remember.** Facts about each person, and a journal of what was done and why.
- **Tune itself.** When a setting keeps getting in the way (rate limits, slow speech to text, an approval you always give), it suggests the fix and makes it once you say yes.
- **Undo.** Package changes and root work are snapshotted, so they can be rolled back.
- **Build software, when you say so.** `orion mode dev` gives the AI a persistent shell with compilers and the network in `~/orion-dev`, for a few hours, with every command logged and undoable. See [Developer mode](docs/safety.md#developer-mode).

## Get started

These steps are for Fedora KDE with Gemini's free tier. Other distributions and models work too: see [Installing Orion](docs/install.md).

**1. Install the GitHub CLI and sign in.** The repository is private, so the installer needs it.

```sh
sudo dnf install -y gh
gh auth login
```

**2. Install Orion.** This one command installs the build tools, builds Orion, sets up its root helper and its background service, and then starts `orion setup`.

```sh
gh api -H 'Accept: application/vnd.github.raw' repos/Goliaith/Orion/contents/packaging/bootstrap.sh | sh
```

Log out and back in afterwards, so your new `orion` group applies.

**3. Connect Gemini.** `orion setup` asks which model to use. Pick Gemini, then paste a key from [aistudio.google.com/apikey](https://aistudio.google.com/apikey). It checks the key and picks a model for you.

**4. Choose a trust level.** Setup then asks how much Orion may do without asking. "Everyday" is a good start: volume, brightness, opening apps and Flatpak installs just happen, and everything else asks. See [Trust levels](docs/safety.md#trust-levels).

**5. Check everything.** `orion doctor` checks each part and offers to fix what's wrong.

```sh
orion doctor
```

**6. Start talking.**

```sh
orion chat
```

Try "how is my machine doing?", "are there security updates?" or "install Spotify".

**Optional next steps:**

- Set up voice: `orion voice setup` ([Voice](docs/voice.md)).
- Let Orion make changes as root, with your approval each time: [Root work](docs/safety.md#root-work).
- Turn on snapshots so changes can be undone: [Snapshots and rollback](docs/safety.md#snapshots-and-rollback).

To update Orion later, run `sh ~/Orion/packaging/bootstrap.sh` again.

## Guides

| Guide | What's in it |
|---|---|
| [Installing Orion](docs/install.md) | Every install method, connecting a model, the optional parts, root access, running as a service |
| [Setup, doctor and self-tuning](docs/setup.md) | `orion setup`, `orion doctor`, and how Orion tunes its own settings |
| [Chatting with Orion](docs/chat.md) | The chat, the full-screen view, the web shell, commands, hotkeys, the fast path, routing between models |
| [Voice](docs/voice.md) | `orion voice setup`, push to talk, the wake word, enrolling voices, hands-free mode, voice tuning |
| [Memory and the journal](docs/memory.md) | What Orion remembers about each person, and its HelixCore journal |
| [Safety and control](docs/safety.md) | Trust levels, approvals, the policy file, the sandbox, root work, snapshots and rollback |
| [The machine](docs/machine.md) | What the tools do: updates, packages, Flatpak, services, files, network, disks, GPUs, the desktop |
| [All tools](docs/tools.md) | Every tool, its risk, and whether it asks first |
| [Operating Orion](docs/operating.md) | Day-to-day use, ready-made policies, the audit log, troubleshooting |
| [For developers](docs/developing.md) | How the code is laid out, adding a tool, making a release, the roadmap |

## How Orion keeps you safe

- **You approve changes.** Out of the box the AI can look at anything, and most changes ask you first. Only harmless ones (volume, brightness, locking the screen) run without asking. Your [trust level](docs/safety.md#trust-levels) decides what else may: Everyday adds Flatpak installs and opening apps.
- **Some things always ask.** Running a program as root, editing a system file, or running a program outside the sandbox asks every time, at every trust level.
- **Hard limits come first.** Orion refuses, before you are asked, to write outside its folders, stop protected services, remove core packages, unmount system disks or cut off SSH.
- **Programs run in a sandbox.** Programs the AI starts can't see your keys, can't reach the network, and can't change files outside its workspace.
- **Private stays private.** Messages about passwords, money or health, and files such as `~/.ssh`, never go to a hosted model.
- **Everything is logged.** `orion audit` shows every call, and `orion audit --verify` checks that no log entry was edited or removed.

More in [Safety and control](docs/safety.md).

## Commands

```sh
orion chat                 # talk to Orion (full screen in a terminal)
orion shell --open         # the same chat in your browser
orion listen               # hands-free: "Orion, …", answered aloud
orion setup                # connect a model and choose a trust level
orion doctor               # check everything and offer fixes
orion voice setup          # set up voice in one guided pass
orion status               # is the background service running, and what needs attention
orion tools                # every tool and what your policy decides for it
orion call <tool> '<json>' # call one tool yourself, as the AI would
orion audit -n 20          # the last 20 actions
orion memory               # what Orion remembers about you
orion conversations        # saved chats
orion approvals            # approve requests from other AI agents
orion serve                # the tools over MCP, for other AI agents
```

## For other AI agents

Orion speaks the [Model Context Protocol](https://modelcontextprotocol.io), so any MCP client can use its tools with the same rules and log. For Claude Code:

```sh
claude mcp add orion -- ~/.cargo/bin/orion serve
```

See [Connect other AI agents](docs/install.md#8-connect-other-ai-agents-mcp).

## License

Copyright (c) 2026 MrSilhouette. All rights reserved: Orion is proprietary, and no license is granted to use, copy, modify or distribute it without written permission. See [LICENSE](LICENSE). The third-party libraries it uses keep their own licenses.
