# prbot

A Slack bot that finds the GitHub PRs requesting your review, has Claude review them (Opus 5.5 at effort medium by default), and posts each review on the PR with inline comments. In Slack you get a short summary with a link. Every posted review is a plain comment, marked as written by Claude and not reviewed by you. prbot never approves, requests changes or merges.

It runs on your own machine with your own accounts: GitHub through the `gh` CLI, Claude through Claude Code (`claude`), or an Anthropic API key if you set one. Slack connects over Socket Mode, so prbot needs no public URL.

## Quickstart (Windows, about a minute)

Get the **team code** from a teammate who already uses prbot (they run `prbot invite`). Then, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/al-noori/prbot/main/install.ps1 | iex
```

This downloads prbot and runs `prbot setup`, which:

1. installs the GitHub CLI and Claude Code if they're missing, and logs you in to both;
2. asks for the team code;
3. finds you in Slack by your email and sends you a DM;
4. starts prbot in the background and sets it to start every time you log in.

Then send `/prreview help` to the app in Slack. Scheduled checks start **off**; turn them on with `/prreview on`.

**No team code yet, so you're the first in your workspace?** Run the same command and press Enter at the team-code question. Setup then creates the Slack app with you:
- It opens Slack with the app manifest already filled in. Click **Create**.
- Under Basic Information → App-Level Tokens, click **Generate**, add the scope `connections:write` and paste the `xapp-…` token.
- Under Install App, click **Install to workspace** (or **Request to Install** if an admin has to approve it) and paste the `xoxb-…` token. If you're waiting for approval, run `prbot setup` again once it's approved; setup picks up where you left off.

Afterwards, run `prbot invite` to get the message with the team code for your teammates.

**macOS and Linux:** install the [GitHub CLI](https://cli.github.com) and [Claude Code](https://claude.com/claude-code), download prbot (pick `prbot-macos-arm64`, `prbot-macos-x86_64`, `prbot-linux-x86_64` or `prbot-linux-arm64`), then run `prbot setup`:

```bash
mkdir -p ~/.local/bin && gh release download --repo al-noori/prbot --pattern prbot-macos-arm64 --output ~/.local/bin/prbot --clobber && chmod +x ~/.local/bin/prbot
prbot setup
```

Running the install command again updates prbot. `prbot doctor` checks the configuration and every connection. `prbot status` tells you whether the bot is running, and `prbot stop` stops it. `prbot autostart off` stops it from starting at login. Only one copy runs per configuration, because a second copy would take half of Slack's events.

**Is it running?** Look at the bot's Home tab in Slack, not the green dot next to its name. Slack doesn't let apps control that dot, so it stays green either way. The Home tab says "running since …" and refreshes every 5 minutes. When prbot stops (`prbot stop`, Ctrl+C, closing its window, or SIGTERM), the tab switches to "stopped". If the computer shuts down or the process is killed, the tab can't update, but its "updated" time stops moving.

Settings are saved to `%APPDATA%\prbot\.env` on Windows or `~/.config/prbot/.env` on macOS and Linux. A `.env` in the current directory takes precedence, and `PRBOT_HOME` overrides both. The state file and `prbot.log` are saved next to it.

## Commands

| Command | Effect |
| --- | --- |
| `/prreview` or `/prreview status` | Show status |
| `/prreview on` / `/prreview off` | Turn scheduled checks on or off (manual reviews always work) |
| `/prreview now` | Check now |
| `/prreview every 2h` / `/prreview at 09:00,14:00` | Check on an interval, or at fixed times |
| `/prreview hours 08:00-18:00` / `hours off` | Only check within working hours |
| `/prreview weekdays on\|off` | Skip weekends, or include them |
| `/prreview autopost on\|off` | Post reviews to the PR automatically, or only when you click |
| `/prreview followup on\|off` | Follow up on PRs with a posted review (see below). Off by default |
| `/prreview model opus\|sonnet\|haiku\|fable` | Choose the Claude model (or give a full model ID) |
| `/prreview effort low\|medium\|high\|xhigh\|max` | How hard Claude thinks (Haiku has no effort setting) |
| `/prreview scope me\|team` | Only PRs requesting you directly, or also through your teams |
| `/prreview list` | Post the overview of your PRs now |
| `/prreview done <PR URL>` / `reopen <PR URL>` | Mark a PR as reviewed by you (stops follow-ups), or undo that |
| `/prreview <PR URL>` | Review that PR now (or just DM the bot the link) |

The Home tab has the same controls: on/off, check now, schedule, model, effort and follow-ups.

To review a PR in the terminal without Slack, run `prbot review https://github.com/OWNER/REPO/pull/123`.

## Follow-ups

With `/prreview followup on`, prbot keeps an eye on every PR it has posted a review on. It checks every 2 minutes, within your working hours:

- **New commits** get a re-review right away. Claude sees its previous review, says which findings are fixed and focuses on what changed.
- **Replies** to Claude's inline comments, and PR comments that @mention you, get an answer from Claude in the same thread, marked as written by Claude and not reviewed by you. If a comment needs no answer (a thanks, an acknowledgement, or something meant for someone else), Claude gives it a 👍 instead.

Comments by bots and by you are never answered, and neither is a comment you've already replied to yourself. Each PR gets at most 5 replies a day. Reviews posted before follow-ups were turned on are followed too, starting from the time they were posted. A PR stops being followed when it's closed or merged, or after 14 days without activity. Every re-review and reply also shows up as a short message in Slack.

## Overview of your PRs

Every scheduled check (and `/prreview list`) posts one message listing the PRs on your plate: the ones requesting your review, the ones Claude reviewed and is following, and the ones you marked done.

- ⏳ **waiting:** not reviewed yet (a draft, or the per-check limit was reached)
- 🔄 **in work:** Claude is reviewing it, or reviewed it and the author is on it
- ✅ **done:** you marked it as reviewed from your side

Each row has a ⋯ menu. **Details** answers in the thread with the PR's state, CI checks, Claude's last review and verdict, new commits since then, the discussion, and other reviewers' verdicts. **Mark done** tells prbot you're finished with the PR: it shows ✅ and Claude stops following it. If the author pushes new commits and requests your review again, the PR is no longer marked done and gets reviewed as usual. **Reopen** undoes Mark done. Merged and closed PRs drop off the list.

## Teams: one Slack app for everyone

A team shares **one Slack app**, installed and approved once. Everything else stays personal. Each person's prbot runs on their own computer and uses their own GitHub login and their own Claude account. Reviews are posted under their own name, and nobody's Claude usage pays for anyone else's reviews.

Slack hands each event (a `/prreview`, a click, a DM) to just one of the prbots connected to the app. If that event belongs to someone else, that prbot forwards it into the person's DM with the bot as a short note, and their prbot picks it up within about 10 seconds and deletes the note. If their prbot isn't running, the note stays and says so. Slack allows 10 live connections per app, so in bigger teams some prbots work through forwarding only, which is a little slower.

**The team code contains the app's Slack tokens.** Share it only within your team: anyone who has it can read every DM with the bot (review summaries, including private repos) and post as the bot. It gives no access to anyone's GitHub or Claude account. If it leaks, regenerate the app's tokens in the Slack app settings and send a new code.

**Finding people by email** needs the scopes `users:read` and `users:read.email`. Apps created from the current manifest have them. For an older app, add the two scopes under App Manifest (or OAuth & Permissions) and reinstall the app. Without them, setup asks for the member ID instead (Slack profile → ⋮ → Copy member ID).

## Run at login

**Windows:** this adds a shortcut to your Startup folder that runs prbot without a window. It doesn't need admin rights.

```powershell
$exe = "$env:LOCALAPPDATA\prbot\prbot.exe"
$lnk = (New-Object -ComObject WScript.Shell).CreateShortcut("$([Environment]::GetFolderPath('Startup'))\prbot.lnk")
$lnk.TargetPath = "conhost.exe"; $lnk.Arguments = "--headless `"$exe`""; $lnk.Save()
```

To stop prbot, run `prbot stop` (`Stop-Process -Name prbot` works too, but then the Home tab can't switch to "stopped"). To remove it from login, delete `prbot.lnk` from the Startup folder (`shell:startup`).

**macOS:** run this from a terminal where `gh` and `claude` work, so the agent gets the same `PATH`:

```bash
cat > ~/Library/LaunchAgents/prbot.plist <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>prbot</string>
  <key>ProgramArguments</key><array><string>$HOME/.local/bin/prbot</string></array>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>$PATH</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict></plist>
EOF
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/prbot.plist
```

**Linux (systemd):**

```bash
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/prbot.service <<EOF
[Unit]
Description=prbot
After=network-online.target

[Service]
ExecStart=$HOME/.local/bin/prbot
Environment=PATH=$PATH
Restart=on-failure
RestartSec=30

[Install]
WantedBy=default.target
EOF
systemctl --user enable --now prbot
```

If the network isn't up yet at login, prbot retries every 30 seconds. Checks missed while the computer was asleep run once when it wakes up.

## What Claude sees

The prompt includes the PR's title and description, the unified diff, and the full post-change contents of the changed files. Lock files and binaries are left out. Claude does not see the rest of the repository, so it can miss problems that involve files outside the PR. `MAX_REVIEWS_PER_RUN` (default 5) caps how many reviews one check starts. With an API key, each review shows an approximate cost at list prices.

## Building from source

```bash
cargo build --release
```

On Windows without the Visual Studio build tools, use the GNU toolchain (`rustup default stable-x86_64-pc-windows-gnu`) with a MinGW `bin` folder containing `dlltool.exe` (for example from WinLibs) on your `PATH`.

To publish binaries, push a tag such as `v0.1.0`. The release workflow builds them for Windows, macOS and Linux and attaches them to a GitHub release.
