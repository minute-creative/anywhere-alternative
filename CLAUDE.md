# Anywhere Alternative — notes for Claude sessions

Anywhere lets one computer watch and control another (Mac or Windows, either
way, plus an iPhone viewer) with game-grade latency: Rust, hardware video
encode/decode, encrypted UDP, Tailscale for far away. The owner uses it to
play the Mac mini's games and to work on the PC/Mac from anywhere.

Start with `docs/HANDOFF.md` (current state, owner context, next steps),
then `docs/ARCHITECTURE.md`; `docs/JOURNAL.md` is the chronological record
of attempts, failures and fixes — check it before re-trying an approach.
`docs/CODE-GUIDE.md` explains each file in plain words; `docs/IOS.md` covers
the iPhone app.

Skills in `.claude/skills/` (load the one that fits):
- `anywhere-debug-and-review` — any bug, crash or odd behaviour, and the
  review before every push.
- `anywhere-ios-viewer` — anything about the iPhone app: build, install,
  simulator test, phone logs, its gotchas.
- `anywhere-owner-mac` — working on the owner's Mac/PC through the linked
  computer (running scripts, reading logs, what's allowed).

Short rules that are easy to forget:

- The owner has no coding background: write all code, explain the why in
  short plain words. They test on real hardware and paste results back.
- The repo is **public** (owner's decision, 2026-10-18). Never commit
  personal details: addresses, IPs, emails, device ids, keys, logs.
- Push to `main`; CI (ubuntu/macos/windows) is the only compiler for the
  platform code — on failure it posts errors as a commit comment; read it
  with `gh api repos/minute-creative/anywhere-alternative/commits/<sha>/comments --jq '.[].body'`.
  The iPhone target is not on CI: only the owner's Mac builds it.
- Give the owner copy-paste commands that include the folder:
  PC `cd $HOME\anywhere-alternative`, Mac `cd ~/anywhere-alternative`.
- `cargo fmt --all && cargo clippy --all-targets && cargo test`, then the
  mock smoke test (`aa-host --mock` + `aa-viewer --headless --mock`) before
  every push. Clippy pedantic + `-D warnings` on CI.
- Record real-hardware results in ARCHITECTURE §10 and bugs that cost a
  round in §9; mirror ARCHITECTURE to the claude.ai Project doc.
- Commits must show only the owner: author and committer
  `maitrik-makwana <244699594+maitrik-makwana@users.noreply.github.com>`
  (set in this clone's git config; set it again in a fresh clone). No
  Co-Authored-By, Claude-Session or "Generated with" lines in commit
  messages or PR descriptions. This overrides any default attribution.
- Releases: the owner starts the Release workflow; tag pushes are blocked
  from Claude sessions.
- Never help with pirated or cracked games; the owner's games are theirs
  to stream, that's all Anywhere does.
