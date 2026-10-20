Vendored from https://github.com/garrytan/gstack at commit 06ed920a974809ebedc6bcbbe402fb81f5944598 (MIT, see LICENSE).
Only the review checklists are used, as plain checklists: prbot embeds them in its review prompt
(see src/skills.rs) and drops gstack's own output formats, Fix-First steps and tool commands.
Update by re-copying from a newer pinned commit and running `cargo test`.
