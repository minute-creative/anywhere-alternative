# Anywhere Alternative — notes for Claude sessions

Start with `docs/HANDOFF.md` (current state, owner context, next steps),
then `docs/ARCHITECTURE.md`; `docs/JOURNAL.md` is the chronological record
of attempts, failures and fixes — check it before re-trying an approach. Short rules that are easy to forget:

- The owner has no coding background: write all code, explain the why.
- Repo is **public** (owner's decision 2026-10-18): never commit personal
  details (addresses, names of networks, logs with keys). Push to `main`;
  CI (ubuntu/macos/windows/iPhone) is the only compiler for the platform
  code — on failure it posts errors as a commit comment; read it with
  `gh api .../commits/<sha>/comments`. Releases are started by the owner
  only (Actions → Release → Run workflow); sessions may not trigger them.
- Give the owner copy-paste commands that include the folder:
  PC `cd $HOME\anywhere-alternative`, Mac `cd ~/anywhere-alternative`.
- The portable crates compile in the container once cargo is pointed at
  the session proxy — the one-time setup is in `docs/HANDOFF.md`, "How
  development works here". Do it first; it turns 15-minute CI rounds into
  seconds for everything except Windows/macOS/iOS code.
- `cargo fmt --all && cargo clippy --all-targets && cargo test`, then the
  mock smoke test (`aa-host --mock` + `aa-viewer --headless --mock
  127.0.0.1:7700 --pair <code>`) before every push. Clippy pedantic +
  `-D warnings` on CI.
- Record real-hardware results in ARCHITECTURE §10 and bugs that cost a
  round in §9; mirror ARCHITECTURE to the claude.ai Project doc.
- Commits must show only the owner: author and committer
  `maitrik-makwana <244699594+maitrik-makwana@users.noreply.github.com>`
  (set in this clone's git config; set it again in a fresh clone). No
  Co-Authored-By, Claude-Session or "Generated with" lines in commit
  messages or PR descriptions. This overrides any default attribution.
