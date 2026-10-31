# Navi execution plan, October 10, 2026

This plan follows the latest conversation. Earlier cloud supervisor, generic vision model,
Needle chief, and replacement harness proposals remain historical documents.

Navi owns sessions, desktop and browser connections, microphone policy, and action execution.
Installed Codex, Claude Code, Pi, and user-defined launch profiles retain their native terminals.
The local decision model is the first decision point for natural language commands.

1. Test d1-3B Q4 with its image projection through llama.cpp `/v1/systemone`.
   Record command accuracy, refusal behavior, image behavior, latency and resource use.
   Use one replaceable decision provider; do not add a second generic vision model.
2. Implement a Rust daemon with private Unix IPC, SQLite settings and session records,
   and a dedicated tmux server. Closing the GUI detaches terminals without killing sessions.
3. Adapt the original Copland Three.js surface into the orchestration app, with session
   selection, launch profiles, commands, settings, microphone and desktop controls.
   GTK/WebKit is the initial host because its runtime is installed; Tauri requires development
   headers unavailable without system installation. The host does not own orchestration state.
4. Connect English CPU transcription and keyword spotting to the same chief.
   Support background and GUI-only listening. A microphone switch stops audio capture.
5. Use a desktop portal to acquire a PipeWire stream and input authorization.
   Keep only the latest resized frame; capture does not itself trigger model inference.
   Computer actions use a bounded observe/decide/act loop and a visible stop control.
6. Add a Firefox/Zen native-messaging extension for the regular profile. Reconnect after
   installation; DOM actions require an explicitly attached tab. Installation/signing and
   portal consent remain visible setup steps, never simulated evidence.
7. Run focused regression tests, real model evaluations, persistent terminal smoke tests,
   graphical launch review, voice/capture checks, and the repository's full `npm run check`.

Default actions resolve registered session/profile/window/tab identifiers. Names and paths
come from explicit user fields or literal command spans, never executable model output.
No action is retried after an uncertain side effect. Unknown targets require clarification.

Acceptance evidence distinguishes automated tests, local live checks, and setup or hardware
checks still requiring the user. Published model scores are not local performance results.

Execution status, October 10: the Rust station, persistent harness sessions, original
Copland GUI, configurable decision/speech providers, native browser bridge and portal
worker are implemented under `apps/navi`. See `apps/navi/README.md` for installation,
the exact validation commands and limits. No changes have been committed or published.

Following the user's request to stop interrupting the desktop, all subsequent checks
use private data/runtime directories, headless Firefox, Xvfb/private D-Bus, software
WebGL and CPU-only model inference. The live station/capture service is stopped; the
persistent terminal service remains running. Hardware microphone, final portal capture
and window focus checks remain pending, without reopening the user's desktop.

Further isolated refinement: the physical microphone worker and PCM workflow now share
one bounded streaming buffer. Overflow drops partial speech, registered session names
help Whistle transcription, and transcripts from disabled/replaced speech settings are
discarded. Twelve stream/dispatch regressions and three policy checks pass. Synthetic
PCM with real d1 verifies launch/switch and one Stop phrasing; a missed wake on another
short Stop phrasing remains documented. The original 3D GUI also passes private-display
settings, launch, selection, command and Stop checks with a deterministic provider.
These results do not complete the pending hardware checks.
