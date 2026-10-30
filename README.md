# devin-acp-telegram

Drive [Devin CLI](https://devin.ai) from Telegram. Start sessions, follow
them live, approve commands with a tap, interrupt, switch modes: all from
your phone, while the agent keeps working on your machine.

```
You:    app: the login test is flaky, find out why
Bot:    🟢 Working · Ran cargo test
        ▶️ Reproduce the failure
        ▫️ Fix the race
        ⚙️ mode accept-edits · context 41k/262k
Bot:    🔐 Allow? Ran git push
        $ git push origin fix/login-race
        [✅ Allow] [♾️ Always allow `git push`] [⛔ Reject]
You:    (tap ✅)
Bot:    Fixed: the session cookie was read before the mock server
        was ready. Pushed fix/login-race.
```

It talks to Devin through the **[Agent Client Protocol](https://agentclientprotocol.com)**
(`devin acp`), the same protocol Devin's own terminal UI uses, and to Telegram through a
**bot you create**. Everything runs on your machine: no server, no inbound port, no
database.

## Features

- **One forum topic per session** (optional): link a Telegram group with Topics and every
  Devin session gets its own thread, named after the session. Without a group, everything
  works in the private chat with the bot.
- **Live status**: a single message edited in place shows the running tool, Devin's plan, the
  mode and context usage. Interim text is sent silently, and only the final answer notifies you.
- **Permission buttons**: Devin's own options (allow once, always allow, reject…). The agent
  waits for your tap, like in the terminal.
- **Mid-turn messages**: a message sent while Devin is working is folded into the running
  turn (Devin supports this natively). Nothing is queued until the end.
- **/stop, /mode, Devin's commands**: interrupt a turn, switch between `code`, `smart`, `ask`,
  `plan` and `bypass`. Devin's own slash commands (`/compact`, `/plan`…) pass through.
- **All your projects, one bridge**: aliases (`/new api: run the migrations`) pick the working
  directory per session.
- **Resume**: `/resume` lists your recent Devin sessions, including ones started in the
  terminal, as buttons. Sessions also survive a restart of the bridge.
- **Keeps the Mac awake** (`caffeinate`) while it runs, toggled with `/caffeine on|off`.
- **Allowlist**: only the Telegram user ids you list can drive the agent.
- English and French messages.

## How it works

```
Telegram  ⇄  devin-acp-telegram (your machine)  ⇄  devin acp (stdio, JSON-RPC)
             long-polling, outbound only            sessions, tools, permissions
```

The bridge polls the Bot API (`getUpdates`) and drives one `devin acp` process. Telegram keeps
undelivered messages for 24 h, so messages you send while the machine sleeps are handled
when it wakes up.

## Setup

**Requirements:** Devin CLI installed and logged in (`devin auth login`), and Rust 1.89+ to
build. macOS is the main target; Linux works too, without caffeine.

1. **Create a bot**: talk to [@BotFather](https://t.me/BotFather), send `/newbot`, and copy
   the token.
2. **Find your user id**: talk to [@userinfobot](https://t.me/userinfobot).
3. **Install**:
   ```bash
   cargo install --git https://github.com/hquezser/devin-acp-telegram
   ```
4. **Configure** `~/.config/devin-acp-telegram/config.env` (and run `chmod 600` on it):
   ```env
   TELEGRAM_BOT_TOKEN=123456789:AA…
   TELEGRAM_ALLOWED_USERS=11111111
   # Optional
   DEVIN_PROJECTS=app=~/code/app,api=~/code/api
   ```
5. **Run**, then open your bot in Telegram and send `/start`:
   ```bash
   devin-acp-telegram ~/code/app
   ```

### One topic per session (recommended)

1. Create a Telegram group and enable **Topics** in its settings.
2. Add your bot and make it an **admin** with the *Manage topics* right.
3. Send `/link` in the group.

New sessions now open in their own topic. Write in *General* to start one, and inside a topic
to talk to its session. `/unlink` goes back to the private chat.

## Commands

| | |
|---|---|
| *any text* | instruction for the current session (a new one if none) |
| *reply to a session message* | instruction for that session |
| `/new [project:] text` | new session, optionally in a project (`/new api: …`, `/new ~/x: …`) |
| `/resume [project]` | pick a recent Devin session to continue |
| `/stop` | interrupt the current turn |
| `/mode code\|smart\|ask\|plan\|bypass` | switch the session mode |
| `/sessions` | bridge and session status |
| `/projects` | project aliases |
| `/close` | end the session (closes its topic) |
| `/caffeine on\|off` | keep the Mac awake, or not |
| `/link`, `/unlink` | use, or stop using, a forum group |
| `/compact`, `/plan`… | any other command goes to Devin |

In a group, commands apply to the topic they are sent in.

## Configuration

Environment variables, or the same keys in `~/.config/devin-acp-telegram/config.env`
(`DEVIN_TG_CONFIG` to use another file). Real environment variables take precedence.

| Variable | Default | |
|---|---|---|
| `TELEGRAM_BOT_TOKEN` | required | token from @BotFather |
| `TELEGRAM_ALLOWED_USERS` | required | comma-separated Telegram user ids |
| `DEVIN_PROJECTS` | none | `alias=dir,…` for `/new alias: …` |
| `DEVIN_CWD` | current directory | default working directory (the CLI argument wins) |
| `DEVIN_MODE` | Devin's default | mode of new sessions |
| `DEVIN_CAFFEINE` | `1` | `0` to let the machine sleep |
| `DEVIN_TG_LANG` | `en` | `fr` for French messages |
| `DEVIN_BIN` | `devin` | agent executable (run as `<bin> acp`) |
| `DEVIN_ACP_ARGS` | none | extra arguments, e.g. `--model opus` |
| `DEVIN_TG_STATE_DIR` | `~/.local/state/devin-acp-telegram` | state file and lock |
| `RUST_LOG` | `info` | log level |

## Running in the background (macOS)

`contrib/com.devin-acp-telegram.plist` is a launchd agent that starts the bridge at login and
restarts it if it stops. Edit the paths in it, then:

```bash
cp contrib/com.devin-acp-telegram.plist ~/Library/LaunchAgents/
launchctl load ~/Library/LaunchAgents/com.devin-acp-telegram.plist
```

Caffeine prevents idle sleep, but **closing the lid still sleeps the Mac** unless it is on
power with an external display.

## Security

- Only users in `TELEGRAM_ALLOWED_USERS` are obeyed. Messages and button taps from anyone else
  are ignored. A group is used only after an allowed user sends `/link` in it.
- Devin's permission prompts are kept: risky actions still need your tap. `bypass` mode
  disables them, so use it knowingly.
- Your bot token gives control of the bridge: keep the config file private (`chmod 600`).
  The token is never logged.
- Nothing listens on the network. The bridge only makes outbound HTTPS calls to
  `api.telegram.org`.
- One bridge per bot: a lock in the state directory prevents a second instance from
  answering the same messages.

## Alternatives

Generic ACP ↔ Telegram bridges exist and may suit you better if you use other agents:
[telegram-acp-bot](https://github.com/mgaitan/telegram-acp-bot) (Python) and OpenACP
(Node, also Discord and Slack). This project focuses on Devin CLI: mid-turn messages, Devin
modes and commands, one topic per session, project aliases, caffeine, and a single static
binary with no runtime to install.

## Limitations

- Text only for now: no images or voice messages.
- One `devin acp` process per bridge: sessions run concurrently inside it.
- Telegram drops undelivered messages after 24 h.

## License

[MIT](LICENSE)
