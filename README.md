# prbot

A Slack bot that finds the GitHub PRs requesting your review, has Claude (`claude-opus-5-5`, effort `medium`) review them, and sends the review to you as a Slack DM.

It **never approves, requests changes, or merges**. The GitHub client has no code for any of those. The only thing it can write to GitHub is a plain *comment* review, and only after you click **Post as PR comment** and confirm.

## What it does

- **Scheduled checks:** every hour by default, or any interval (`every 30m`), or fixed times (`at 09:00,14:00`). Weekends are skipped by default. Checks start **off**.
- **Manual reviews:** DM the bot a PR link, run `/prreview <url>`, or paste the link into the Home tab.
- **On/off:** `/prreview on|off`, or the toggle on the Home tab. Manual reviews keep working when checks are off.
- **No repeats:** each PR is reviewed once per head commit. A new push gets a new review at the next check.
- Each review is one DM message showing the verdict, size, approximate cost, and buttons (**Post as PR comment**, **Re-review**). The full review is in that message's thread.

## Setup

1. **Create the Slack app:** go to https://api.slack.com/apps, choose **Create New App**, then **From a manifest**, and paste `slack-app-manifest.yaml`. Your workspace may need an admin to approve it.
2. **Get the app-level token:** go to **Basic Information**, then **App-Level Tokens**, and generate a token with the `connections:write` scope. This is `SLACK_APP_TOKEN` (`xapp-…`).
3. **Get the bot token:** choose **Install App**, then install to your workspace. This gives you `SLACK_BOT_TOKEN` (`xoxb-…`).
4. **Find your member ID:** open your Slack profile, then **⋮**, then **Copy member ID**. This is `SLACK_OWNER_ID`.
5. **Fill in the config:** copy `.env.example` to `.env`, then add `ANTHROPIC_API_KEY`. GitHub uses the token from `gh auth token` unless you set `GITHUB_TOKEN`.
6. **Run it:**

```bash
cargo run --release
```

On Windows without the Visual Studio build tools, the GNU toolchain works too. Run `rustup default stable-x86_64-pc-windows-gnu`, and make sure a MinGW `bin` folder that contains `dlltool.exe` (for example from WinLibs) is on your `PATH`.

It uses Socket Mode, so it needs no public URL and runs fine on a laptop. Checks missed while the laptop was asleep run once when it wakes up. To keep it running all the time, start it at login (for example with Windows Task Scheduler) or run it on a small server.

To test a review without Slack:

```bash
cargo run --release -- review https://github.com/OWNER/REPO/pull/123
```

## Commands

| Command | Effect |
| --- | --- |
| `/prreview` or `/prreview status` | Show status |
| `/prreview on` / `/prreview off` | Turn scheduled checks on or off |
| `/prreview now` | Check now |
| `/prreview every 2h` | Check on an interval (5 min to 24 h) |
| `/prreview at 09:00,14:00` | Check at fixed local times |
| `/prreview weekdays on\|off` | Skip weekends, or include them |
| `/prreview scope me\|team` | Only PRs requesting you directly, or also through your teams |
| `/prreview <PR URL>` | Review that PR now |

## What Claude sees

The prompt includes the PR's title and description, the unified diff, and the full post-change contents of the changed files. Lock files and binaries are left out. There are budgets of 600k characters for the diff and 400k for file contents. If a PR goes over a budget, the Slack message says what was left out. Claude does not see the rest of the repository, so cross-file problems outside the changed files can be missed.

At Opus 5.5 list prices ($4 / $20 per million tokens), a typical review costs a few cents to about $0.50. Each message shows an estimate. `MAX_REVIEWS_PER_RUN` (default 5) caps how many reviews one check can start.

## Layout

- `src/main.rs`: startup, the CLI `review` mode, and the Socket Mode loop
- `src/bot.rs`: commands, the Home tab, buttons, the scheduler, and posting reviews
- `src/review.rs`: gathers the PR context and holds the prompt
- `src/claude.rs`: the Messages API call over raw HTTP (there is no official Rust SDK)
- `src/github.rs`: the GitHub REST calls (reads, plus the one COMMENT write)
- `src/slack.rs`: the Slack Web API
- `src/state.rs`: the schedule and persisted state (`prbot-state.json`)
