Read @AGENTS.md

This is a personal fork with custom features and a local build/install workflow.
Before building, installing, or extending it, also read @FORK_NOTES.md.

**Never build or install the app without Martin's explicit go-ahead.** `bun run tauri build`,
replacing `/Applications/Handy.app`, and quitting/relaunching Handy all wait for his direct green
light — he dictates through it all day, and every rebuild takes it away mid-work. Do everything
else (code, tests, `cargo check`, `tsc`, commits, docs, mockups) on your own, then ask once, when
the work is finished. Approval covers one build, not the rest of the session.

Dictation hardware: the mic in use is a METADOX VEKTA soundproof mask (wired USB, worn over the
mouth). Before tuning audio input, mic gain, or anything whisper/quiet-speech related, read
@docs/vekta-mask.md — it has the device's USB/CoreAudio identifiers, specs, known weak spots and a
tuning playbook, so no web research is needed. Market context and alternatives:
@docs/silent-dictation-research.md.
