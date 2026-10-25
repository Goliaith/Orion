# Memory and the journal

Orion remembers two kinds of things:

- **[Notes](#notes)** are the few facts it should always know about a person: "prefers metric units", "the thesis code is in ~/thesis".
- **[The journal](#the-journal)** is the long tail: what was done and how, what worked, where things are, and decisions with their reasons. It uses [HelixCore](https://github.com/Goliaith/helixcore)'s format.

Both are kept per person, on this machine.

## Whose memory it is

- **Typed messages** belong to whoever is logged in.
- **Spoken requests** from an [enrolled voice](voice.md#enrolling-voices) belong to that person. Their latest conversation continues if it was active in the last 10 minutes; otherwise a new one starts. When the speaker changes, Orion switches and says so ("Ana's conversation").
- **Nobody sees anyone else's** notes or journal.

## Notes

The AI saves a note when it learns a lasting fact, with its `memory_save` tool. Each new conversation starts with that person's notes.

```
bea: remember that I prefer metric units.
⚙ memory_save {"text":"Prefers metric units"}
orion: Got it, Bea: I'll remember that.
carl: what do you know about me?
orion: Carl, I don't know anything about you yet.
```

### See and remove notes

In the chat:

```
/memory              what is remembered about you
/memory forget 3     forget note 3
/memory clear        forget everything about you
```

In a terminal:

```sh
orion memory                      # yours
orion memory --person ana         # someone else's
orion memory forget 3
orion memory clear
```

Or just ask: "forget that I prefer metric units."

### Rules

- **Secrets are never kept.** A note that looks like a password, key or token is refused.
- **Private notes stay on this machine.** A note about health, money, private paths and the like is only shown to a model running on this machine. Claude, or Gemini, is only told that some notes are withheld.
- **Small:** at most 60 notes of 300 characters each per person.
- **Where:** `~/.local/state/orion/memory/`, one file per person, readable only by you. Programs the AI runs can't see it.
- **Turn it off:** `memory = false` under `[chat]`.

## The journal

The journal holds what doesn't fit in a few notes: how a problem was fixed, where a project lives, why a choice was made. It is kept by topic and found again when a message is about it.

### How the AI uses it

The AI has three tools:

| Tool | What it does |
|---|---|
| `journal_write` | keeps an entry under a topic |
| `journal_decide` | records a decision and why (the latest one per category wins) |
| `journal_recall` | searches by keywords |

### Recall costs no extra request

Before each message goes to a model, Orion searches that person's journal on this machine and adds the best matches to the message, under "From Orion's journal". Up to 3 entries are added, each sharing at least two words with your message.

The AI doesn't need to make a tool call to remember, which matters on Gemini's free tier, where every tool call is one more request.

### How search works

It is HelixCore's search: keyword overlap counts for three quarters and how recent the entry is for one quarter, fading to nothing after 90 days. It is plain keyword matching, fast on any CPU, with no embeddings.

Topics also link up. Each decision links topics that share words, and `/journal` shows the strongest links.

### See it

In the chat:

```
/journal                     the topics
/journal <topic>             one topic
/journal search <words>      search them all
```

### Rules

- **Each person's own.** A person's topics start with their name (`ana_thesis-code`), and nobody else's are searched, listed or linked.
- **Secrets are refused, and private entries stay on this machine,** as with notes.
- **Logged.** Journal tool calls are in the audit log.
- **Where:** `~/.local/state/orion/helix/.grok/state/`, readable only by you and hidden from programs the AI runs. HelixCore's own Python package can read and search it.

### Settings

Under `[chat.journal]`:

```toml
[chat.journal]
enabled = true        # false turns the journal off
auto_recall = true    # false stops adding matches to each message
# home = "..."        # keep it somewhere else (the sandbox hides it there too)
```
