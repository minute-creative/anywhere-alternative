# Anywhere Alternative — notes for Claude sessions

Start with `docs/HANDOFF.md` (current state, owner context, next steps),
then `docs/ARCHITECTURE.md`; `docs/JOURNAL.md` is the chronological record
of attempts, failures and fixes — check it before re-trying an approach. Short rules that are easy to forget:

- The owner has no coding background: write all code, explain the why.
- Repo is private; stay private. Push to `main`; CI (ubuntu/macos/windows)
  is the only compiler for the platform code — on failure it posts errors as
  a commit comment; read it with `gh api .../commits/<sha>/comments`.
- Give the owner copy-paste commands that include the folder:
  PC `cd $HOME\anywhere-alternative`, Mac `cd ~/anywhere-alternative`.
- `cargo fmt --all && cargo clippy --all-targets && cargo test`, then the
  mock smoke test (`aa-host --mock` + `aa-viewer --headless --mock`) before
  every push. Clippy pedantic + `-D warnings` on CI.
- Record real-hardware results in ARCHITECTURE §10 and bugs that cost a
  round in §9; mirror ARCHITECTURE to the claude.ai Project doc.
