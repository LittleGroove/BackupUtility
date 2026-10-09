# BackupUtility

Rust workspace for a Windows disk/volume backup engine (see README.md).
Cargo is not on the default PATH here; prepend `$env:USERPROFILE\.cargo\bin` first.

## Portability rule (do not break)

This project folder must be movable to another PC as a plain folder copy, with
Claude's memory, plans and session history intact. Everything Claude needs
lives inside `.claude/`:

| Path | What | How it gets there |
|---|---|---|
| `.claude/memory/` | Claude auto-memory (MEMORY.md + one file per fact) | `autoMemoryDirectory` in `.claude/settings.local.json`, kept pointing at this folder's absolute path by the SessionStart hook |
| `.claude/plans/` | Plan-mode files | `plansDirectory` in `.claude/settings.json` |
| `.claude/transcripts/` | Mirror of session `.jsonl` transcripts | Stop hook copies them from `~/.claude/projects/<slug>/` |
| `.claude/portable.ps1` | The script the hooks run (`-Setup` / `-Export`) | - |

Rules for Claude:

- Write and update memories in `.claude/memory/` (the memory directory given in
  the system prompt should already resolve there; if it still points under
  `~/.claude/projects/`, run `.claude/portable.ps1 -Setup` and tell the user to
  restart the session).
- Never store project-specific state only under `~/.claude/`. If a new kind of
  out-of-project state appears, extend `portable.ps1` so it travels too.
- Do not hardcode this machine's absolute path anywhere except
  `.claude/settings.local.json`, which the hook regenerates per machine.

After moving the folder to a new PC: open it in Claude Code once (the
SessionStart hook rewrites `settings.local.json` and restores transcripts),
then start a fresh session so memory loads from `.claude/memory/`.

## Commits

No AI attribution of any kind: no `Co-Authored-By` trailers, no "generated with" lines in
commit messages, pull requests or release notes. Subject and body only.
