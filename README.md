# prbot

A Slack bot that finds the GitHub PRs requesting your review, has Claude review them (Opus 5.5 at effort medium by default), and posts each review on the PR with inline comments. In Slack you get a short summary with a link. Every posted review is a plain comment, marked as written by Claude and not reviewed by you. prbot never approves, requests changes or merges.

It runs on your own machine with your own accounts: GitHub through the `gh` CLI, Claude through Claude Code (`claude`), or an Anthropic API key if you set one. Slack connects over Socket Mode, so prbot needs no public URL.

## Quickstart

1. **Install and log in to the two CLIs:** [GitHub CLI](https://cli.github.com) and [Claude Code](https://claude.com/claude-code). `prbot setup` offers to log you in if you haven't yet.

2. **Download prbot** (no Rust needed).

   Windows (PowerShell):

   ```powershell
   New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\prbot" | Out-Null
   gh release download --repo al-noori/prbot --pattern prbot-windows-x86_64.exe --output "$env:LOCALAPPDATA\prbot\prbot.exe" --clobber
   cd "$env:LOCALAPPDATA\prbot"
   ```

   macOS and Linux (pick `prbot-macos-arm64`, `prbot-macos-x86_64`, `prbot-linux-x86_64` or `prbot-linux-arm64`):

   ```bash
   mkdir -p ~/.local/bin && gh release download --repo al-noori/prbot --pattern prbot-macos-arm64 --output ~/.local/bin/prbot --clobber && chmod +x ~/.local/bin/prbot
   ```

3. **Run `prbot setup`.** It checks GitHub and Claude, opens Slack with the app manifest already filled in, and asks for two tokens:
   - the **app-level token** (`xapp-…`): Basic Information → App-Level Tokens → Generate, with the scope `connections:write`;
   - the **bot token** (`xoxb-…`): Install App → Install to workspace.

   If your workspace needs admin approval, click **Request to Install** and run `prbot setup` again once it's approved. Setup picks up where you left off.

4. **Send the bot any message in Slack** when setup asks. That's how prbot learns your Slack member ID; only you can use the bot.

5. **Start it with `prbot`**, then send `/prreview help` in Slack. Checks start **off**; turn them on with `/prreview on`.

`prbot doctor` checks the configuration and every connection.

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
| `/prreview <PR URL>` | Review that PR now (or just DM the bot the link) |

The Home tab has the same controls: on/off, check now, schedule, model, effort and follow-ups.

To review a PR in the terminal without Slack, run `prbot review https://github.com/OWNER/REPO/pull/123`.

## Follow-ups

With `/prreview followup on`, prbot keeps an eye on every PR it has posted a review on. It checks every 2 minutes, within your working hours:

- **New commits** get a re-review right away. Claude sees its previous review, says which findings are fixed and focuses on what changed.
- **Replies** to Claude's inline comments, and PR comments that @mention you, get an answer from Claude in the same thread, marked as written by Claude and not reviewed by you. If the reply is just a thanks, or meant for someone else, Claude doesn't answer.

Comments by bots and by you are never answered, and neither is a comment you've already replied to yourself. Each PR gets at most 5 replies a day. A PR stops being followed when it's closed or merged, or after 14 days without activity. Every re-review and reply also shows up as a short message in Slack.

## For teammates

Anyone in the workspace can find your app in Slack, but it's personal: it reviews *your* review requests with *your* accounts, and answers everyone else with a pointer to this README. Each person runs their own prbot with their own Slack app, which takes about 5 minutes with the quickstart above. `prbot setup` names the app after your GitHub login, so the apps are easy to tell apart.

Every app registers `/prreview`. If Slack ever routes your `/prreview` to someone else's app, you'll get that pointer instead. DMs and the Home tab of your own app always reach your prbot and accept the same commands. Some workspaces need an admin to approve each new app once.

## Run at login

**Windows:** this adds a shortcut to your Startup folder that runs prbot without a window. It doesn't need admin rights.

```powershell
$exe = "$env:LOCALAPPDATA\prbot\prbot.exe"
$lnk = (New-Object -ComObject WScript.Shell).CreateShortcut("$([Environment]::GetFolderPath('Startup'))\prbot.lnk")
$lnk.TargetPath = "conhost.exe"; $lnk.Arguments = "--headless `"$exe`""; $lnk.Save()
```

To stop prbot, run `Stop-Process -Name prbot`. To remove it from login, delete `prbot.lnk` from the Startup folder (`shell:startup`).

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
