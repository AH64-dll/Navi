# Section 4 — Voice capture and transcription

Scope: `workers/voice.py`, `voice_stream.py`, `cloud_stt.py`, `hex_asr.py`, `speech_config.py`, `speech_out.py`, `tts.py`, `src/main.rs` (worker spawn/cancel), `ui/station.js` (voice display).

**Measured facts.** Capture is mono float32, 16 kHz, 100 ms blocks; pre-roll is 30 blocks (3 s); after a wake the buffer waits up to 60 blocks (6 s) for speech; endpointing commits after 9 quiet blocks (900 ms) with a 15-block (1.5 s) minimum and a 200-block (20 s) hard cap; the utterance sent to STT therefore contains up to 3 s of room noise, the wake word, up to 6 s of pause, the command, and ≥900 ms of trailing silence. `recognize()` runs inline in the capture loop. The cloud chain is `deepgram → groq → cloudflare → gemini`, then Hex, then local Whistle/Moonshine/Whisper. Language is hardcoded `"en"`; hotwords reach only Whistle; one provider failure opens a 300 s breaker; there is no confidence, retry, or second opinion. The mic stream is never paused during TTS, and no echo cancellation exists.

---

## BUG-049 · Up to 3 s of room noise and the wake word are uploaded to STT
- **Severity** High · **Type** accuracy
- **Evidence** `voice_stream.py:59` (`recording = list(pre_roll)` at wake; `pre_roll_blocks=30` at `:16`); `:70-75` appends the post-wake pause; `voice.py:184,192` upload the buffer verbatim.
- **Root cause** One buffer serves both wake detection and transcription, so "audio needed to hear the wake" is never separated from "audio that is the command".
- **Fix** Track the wake block index; emit `(blocks, speech_start)`; trim the head to ~200 ms before the detected onset (clamped by the wake mark minus a keyword window when the engine exposes no offset); do not store the `awaiting_speech` quiet blocks (count them, don't append); trim trailing quiet to the last non-quiet block + 300 ms before encoding.
- **End state** "Hey Navi, what time is it" uploads ≈1.5 s containing only "what time is it"; transcripts no longer begin with the wake phrase.
- **Why** Leading noise is the main cause of hallucinated words and wake-word residue, and it cuts upload bytes and latency.
- **Depends** none · **Verify** Synthetic "Hey Navi + command" fixture through `voice.offline()`; assert the cloud-bound PCM starts ≤300 ms before the first speech-energy block and contains no wake-word audio; assert length shrinks.
- **Effort** M · **Risk** over-trimming soft first syllables — keep the lead-in and make it configurable.

## BUG-050 · Wake removal is a fragile text regex
- **Severity** Medium · **Type** accuracy
- **Evidence** `voice.py:69-73` strips `hey|ay` + `navi|navy|nadi|nabi|nave` from **text**, unconditionally applied at `:208` to every tier.
- **Root cause** Because the wake is in the audio (BUG-049), removal is guesswork over engine-specific misspellings.
- **Fix** After audio trimming, demote the regex to a safety net applied only to wake-triggered captures; derive variants from the KWS keyword list instead of a hardcoded list; log any stripped residue; add a mishearing test table from the evaluation fixtures.
- **End state** The command text never contains the wake phrase and a legitimate word beginning with "nav…" is not eaten.
- **Why** Text-level stripping fails in both directions (leaks and over-strips).
- **Depends** BUG-049 · **Verify** Unit table: "Hey, Navi." → "", "Navy deployment docs" (no wake in audio) → unchanged; "Hey Navi navigate to X" → "navigate to X".
- **Effort** S · **Risk** low.

## BUG-051 · A fixed RMS threshold decides silence
- **Severity** High · **Type** accuracy/robustness
- **Evidence** `voice.py:368` (`np.sqrt(mean(samples**2)) < .008`), same constant in `offline()` (`:212`).
- **Root cause** One absolute amplitude decides both "speech started" and "utterance ended", with no calibration, AGC, or noise floor.
- **Fix** Replace with an adaptive VAD: estimate noise floor during the first ~1 s and update it with a slow EMA; threshold at `noise_floor × factor` with a minimum; add a soft AGC (target RMS, max gain 12 dB) before STT; surface the calibrated floor in `voice_state` for diagnostics.
- **End state** Quiet mics are heard; noisy rooms still endpoint; neither path depends on a magic constant.
- **Why** A single number cannot fit both a headset and a fan-cooled laptop.
- **Depends** none · **Verify** `test_voice_stream.py` with synthetic noise at three levels and speech at three gains; assert endpoint within ±200 ms of truth in all six.
- **Effort** M · **Risk** medium (VAD tuning); keep `.008` as a floor, not the decision.

## BUG-052 · Short commands are slow to endpoint
- **Severity** Medium · **Type** latency
- **Evidence** `voice_stream.py:19` (900 ms quiet), `:17` (1.5 s minimum), `:82` (commit condition).
- **Root cause** The endpoint waits for consecutive quiet and cannot commit early.
- **Fix** Commit when a VAD says speech ended and either (a) ≥2 s of speech was captured or (b) the last speech block is ≥350 ms old and the total is ≥600 ms; keep the 900 ms rule only for barge-in captures; start STT for the first chunk while the tail is still buffering (streaming, BUG-056).
- **End state** "Navi, stop" dispatches in ~600 ms instead of ~2.4 s.
- **Why** Latency is the difference between a voice assistant and a voice interface.
- **Depends** BUG-051 · **Verify** Synthetic 400 ms utterance; assert commit ≤900 ms after onset and that no speech block is truncated.
- **Effort** S · **Risk** premature cuts on slow speakers — gate by VAD confidence.

## BUG-053 · The 20 s cap truncates long dictation silently
- **Severity** Medium · **Type** accuracy
- **Evidence** `voice_stream.py:18` (`maximum_blocks=200`), `:83` forces an utterance at the cap; no truncated flag reaches `voice_state`.
- **Root cause** A hard buffer bound with no user signal.
- **Fix** Raise the cap to 60 s with segmented streaming (send completed 10 s segments, concatenate transcripts); if the cap is hit, set `truncated:true` and show a chat marker "recording limit reached — text may be incomplete".
- **End state** Long instructions work, and any cut is visible.
- **Why** Coding requests are frequently long.
- **Depends** BUG-056 · **Verify** 30 s synthetic audio → two segments, concatenated text, `truncated:false`; 65 s → `truncated:true` marker.
- **Effort** M · **Risk** segment boundaries cutting words — overlap 200 ms and de-duplicate.

## BUG-054 · The microphone is not read while STT runs
- **Severity** Critical · **Type** robustness/accuracy
- **Evidence** `voice.py:405` runs `recognize()` inline; `:366` is the only `microphone.read`; worst case ≈4×3 s timeouts + Hex 45 s + local decode.
- **Root cause** One thread performs capture, endpointing, STT, and dispatch sequentially.
- **Fix** Move STT to a worker thread (or process) with a bounded queue; the capture loop continues draining the mic and feeding the wake detector; add a "transcribing" ring that keeps the last 3 s so a follow-up wake is not lost; cancel in-flight STT on a new wake.
- **End state** The mic stays live during transcription; speaking again is heard.
- **Why** Current behaviour drops the next 3–60 s of speech and resets the buffer.
- **Depends** BUG-055, BUG-063 · **Verify** Synthetic gate that blocks STT for 2 s while feeding audio; assert no `overflow` event and that a second wake during the block is captured.
- **Effort** M · **Risk** thread-safety — single producer, single consumer, explicit queue bound.

## BUG-055 · Any buffer overflow discards the whole command
- **Severity** High · **Type** robustness
- **Evidence** `voice_stream.py:46-49` resets on `overflow=True`.
- **Root cause** Overflow is treated as fatal rather than as a gap.
- **Fix** Mark a discontinuity, keep the pre-roll and any captured speech, and continue; if the gap is inside speech, dispatch what was captured with `gap:true` and note it in the transcript metadata; only reset when the wake hypothesis itself is invalid.
- **End state** A hiccup shortens the command instead of erasing it.
- **Why** Losing a half-spoken sentence is worse than a slightly gapped transcript.
- **Depends** BUG-054 · **Verify** Inject an overflow mid-utterance; assert the captured prefix still dispatches and no reset occurs.
- **Effort** S · **Risk** low.

## BUG-056 · No live transcript and no correction path
- **Severity** High · **Type** UX/accuracy
- **Evidence** `voice.py:413-414` emits text only after full STT; the UI reads only `value.voice.status` (`ui/station.js:126`); cloud and Hex are batch.
- **Root cause** The pipeline is strictly batch and the UI ignores the transcript fields it already receives.
- **Fix** Emit partials: (a) cloud streaming where available (Deepgram WebSocket) or segment-by-segment transcripts for long utterances; (b) at minimum, show the final transcript in the composer before dispatch for 1.5 s with an Edit/Undo affordance, cancellable by Enter; (c) render `source` and confidence; (d) add a "correct and resend" action on the last voice message.
- **End state** You see what was heard, can fix it before it runs, and can correct after.
- **Why** Mishearings currently execute silently, which is the worst failure mode for a voice-first product.
- **Depends** BUG-049, BUG-051 · **Verify** Synthetic utterance → partial events arrive during capture; final text appears in the composer with a 1.5 s window; editing changes the dispatched text.
- **Effort** L · **Risk** medium (streaming protocol) — the confirm-before-dispatch step alone is S and should ship first.

## BUG-057 · English is hardcoded at every tier
- **Severity** Medium · **Type** feature/accuracy
- **Evidence** `voice.py:184,200` (`language="en"`), `hex_asr.py:23` (`parakeet_unified_en`), `tts.py:29` (English voice default); no settings key.
- **Root cause** The pipeline assumes one language from wake word to TTS.
- **Fix** Add `settings.speech.language` (default from locale); pass it to every provider; keep Hex only when its model matches the language, otherwise skip to a multilingual provider; select a matching TTS voice; document the wake-word model's language limitation.
- **End state** Speaking another language produces that language, not English phonetic guesses.
- **Why** Users work in more than one language.
- **Depends** none · **Verify** Set `fr`; assert the provider request carries `language=fr` and Hex is skipped; UI shows the language.
- **Effort** S · **Risk** wake model remains English-only; state that in the UI.

## BUG-058 · Hotword biasing exists only on the local fallback
- **Severity** Medium · **Type** accuracy
- **Evidence** `voice.py:199-202` passes `keywords` only in the Whistle branch; cloud (`:184`) and Hex (`:192`) get audio only; `speech_keywords` already builds up to 24 names from sessions/profiles/projects/favorites (`:85-104`).
- **Root cause** Keyword biasing was wired into one engine and never propagated.
- **Fix** Send keywords where the provider supports them (Deepgram `keywords`, Gemini transcription hints); for engines without hotwords, prepend a short context line to the *prompt* when the engine supports prompts; always include project and session names; measure WER on a fixture set before/after.
- **End state** "Claude", project names, and identifiers are transcribed correctly on the engines actually used.
- **Why** Proper nouns are exactly what voice gets wrong, and they are the routing keywords.
- **Depends** BUG-056 (transcript context) · **Verify** Fixture audio containing three project names; assert the chosen provider receives the keyword list (FakeHTTP) and the transcript matches.
- **Effort** M · **Risk** provider-specific parameter limits; cap at 20 terms.

## BUG-059 · One failure disables a provider for five minutes, with no retry or confidence
- **Severity** High · **Type** robustness/accuracy
- **Evidence** `cloud_stt.py:199-201` (any exception → breaker), `:44-45` (300 s), `speech_config.py:34`; no retry loop; return is bare text; Deepgram confidence is parsed only as text.
- **Root cause** A transient error is treated as a provider outage, and no quality signal survives.
- **Fix** Retry once per provider on network/5xx with jitter; open the breaker only after 2 consecutive failures or on 401/403; parse confidence when provided and attach it to the result; if confidence is below a threshold (or two providers disagree materially), show the transcript with "did you mean" alternatives instead of executing.
- **End state** A single packet loss does not demote the chain for five minutes, and low-confidence text is confirmed rather than executed.
- **Why** Accuracy degrades silently today whenever the best provider hiccups.
- **Depends** BUG-056 · **Verify** FakeHTTP: one 500 then success → exactly two attempts, breaker closed; two 401 → open; low-confidence fixture → confirmation path.
- **Effort** M · **Risk** retries add latency — bound by the existing 3 s timeout.

## BUG-060 · No echo cancellation; Navi can interrupt itself
- **Severity** High · **Type** accuracy
- **Evidence** `voice.py:369-379` feeds the raw mic to the wake detector during playback; `input_device` selects a raw PipeWire input; no echo-cancel module or reference channel.
- **Root cause** Played audio is never removed from captured audio.
- **Fix** Route playback and capture through a PipeWire echo-cancel virtual source when available (`module-echo-cancel`), or capture the playback reference and subtract it (NLMS) before the KWS; at minimum, suppress wake detection for a short window after known TTS phonemes and require two consecutive wake hits while speaking.
- **End state** Navi does not wake on its own voice; barge-in still works when you actually speak.
- **Why** Self-interruption is the most visible voice failure.
- **Depends** none · **Verify** Play a TTS sample containing "Navi" through the output device while capturing; assert zero wake events; a human "Navi" during playback still wakes.
- **Effort** M · **Risk** AEC quality varies by device — keep the suppression fallback.

## BUG-061 · Spoken "stop" does not stop playback
- **Severity** High · **Type** UX/robustness
- **Evidence** `voice_stream.py:111` maps stop words to `rpc("cancel")` only; `voice.py:369-379` runs only the KWS during playback, so a bare "stop" is never captured; `main.rs:529-535` has no playback hook; `Playback.cancel` is invoked only on a new wake.
- **Root cause** Three stop paths exist and only the wake path reaches TTS.
- **Fix** Add a `voice_stop` RPC (or extend `cancel`) that sets the worker's playback cancel event; while speaking, run a lightweight stop-word detector in addition to the KWS (or accept "Navi stop"); acknowledge audibly ("stopped") after cancellation.
- **End state** Saying "stop" (or pressing Stop) silences the voice immediately.
- **Why** A voice assistant you cannot interrupt is unusable.
- **Depends** BUG-060 (to avoid the inverse problem) · **Verify** Play 10 s TTS; call `cancel`; assert the player process exits ≤200 ms and `playback.speaking()` is false.
- **Effort** S · **Risk** low.

## BUG-062 · Speech during a turn is dropped, not queued
- **Severity** Medium · **Type** UX/robustness
- **Evidence** `voice_stream.py:91,115-116` returns `busy`; `voice.py:369-379` mutes capture during playback; `:405` blocks the mic during STT.
- **Root cause** Three drop points, each invisible until it happens.
- **Fix** Queue one pending transcript (cap 2) and dispatch it when the turn ends; show "queued" in the voice status; during playback, allow wake + capture (after BUG-060) so a follow-up can be spoken; on overflow of the queue, keep the newest and say "dropped the earlier request".
- **End state** Speaking immediately after a request is not lost.
- **Why** Users speak while the assistant is working.
- **Depends** BUG-054, BUG-061 · **Verify** Dispatch a 5 s fake command, speak a second, assert the second executes after the first with no manual retry.
- **Effort** M · **Risk** queue must respect cancel.

## BUG-063 · A crashed voice worker is never restarted
- **Severity** High · **Type** robustness
- **Evidence** `main.rs:625` spawns once; `:541` is the only respawn path (UI mic toggle/settings save); `:421` reaps dead children inside `spawn_worker` only; `voice.py:416-418` catches four exception types.
- **Root cause** No supervisor and no liveness check; native-library crashes kill the process outright.
- **Fix** Add a supervisor thread: every 2 s check `try_wait`; on unexpected exit, restart with exponential backoff (1/2/5/10 s, cap 5 attempts), emit `worker_restart` with the exit status, and surface it in the status pill; reset the counter after 60 s of health. Also catch `BaseException` at the top level, log, and exit non-zero deliberately.
- **End state** Voice recovers by itself; repeated crashes are visible instead of silent.
- **Why** Native ASR libraries do crash; "mic stops working until you toggle it" is unacceptable.
- **Depends** BUG-099 (worker locks) · **Verify** Kill the worker with SIGKILL; assert it restarts within 2 s and a `worker_restart` event appears; kill five times → stops and reports.
- **Effort** M · **Risk** restart loops — bounded with backoff.

## BUG-064 · The capture loop blocks on status RPCs
- **Severity** High · **Type** latency/robustness
- **Evidence** `voice.py:361-362` calls `rpc("status", timeout=2)` every 400 ms on the capture thread; `:58` (`state()`) also calls synchronously ~4×/utterance; `voice_stream.py:108` does another.
- **Root cause** Blocking IPC on the thread that drains PortAudio.
- **Fix** Move status polling to a separate thread publishing an immutable snapshot; the capture loop reads the snapshot only; make `state()` fire-and-forget over a queue with a single writer; never call `status` from `dispatch` (pass the snapshot).
- **End state** Mic reads are never delayed by IPC; overflow rate drops to hardware-level.
- **Why** Every blocking call is a potential dropped word.
- **Depends** BUG-054 · **Verify** Inject a 1.5 s daemon stall; assert no `overflow` event and capture continuity.
- **Effort** S · **Risk** low.

## BUG-065 · Voice cannot address a harness session
- **Severity** Medium · **Type** feature gap
- **Evidence** `voice_stream.py:117` sends every transcript to `rpc("command")`; `chief.rs:791-838` supports `launch/switch/browser/url/browser_action/desktop_action` only; the `prompt` op exists (`main.rs:500-505` → `send_keys`) but nothing on the voice path routes to it; `active_session` is read only for keyword ordering (`voice.py:88`).
- **Root cause** Voice→harness typing was never wired, though the mechanism is one function away.
- **Fix** Add a `harness_prompt` action: resolve the target (explicit name, "this/active", or the last launched), then `send_keys`; add routing phrases ("tell Claude…", "ask Codex…", "continue in…") to the decision options; require the target's state to be `idle|needs-input` and report `busy` otherwise; read back the pane tail (BUG-079) to confirm delivery.
- **End state** "Tell Claude to run the tests" types it into Claude and confirms.
- **Why** This is product goal 3's spoken interface; today speech only reaches the assistant.
- **Depends** BUG-069, BUG-079, BUG-082 · **Verify** Isolated `terminal` profile: speak "tell the terminal to echo hi"; assert the pane receives it and the log shows it.
- **Effort** M · **Risk** misrouting — require an explicit target or the active badge.

## BUG-066 · The STT docstring describes a chain that no longer exists
- **Severity** Low · **Type** maintainability
- **Evidence** `cloud_stt.py:1` says "Groq → Cloudflare whisper → Gemini", omitting Deepgram; the real default is `["deepgram","groq","cloudflare","gemini"]` (`speech_config.py:29,37`) and is configurable; `cloud_stt.py:10` claims a fixed 3 s timeout; `tts.py:1` has the same fixed-order claim.
- **Root cause** Docs written before Deepgram and configurable ordering, never updated.
- **Fix** Generate the provider list in the docstring from `CLOUD_STT`, describe timeouts as configurable with defaults, and add a test asserting the docstring's provider list matches `speech_config.CLOUD_STT`.
- **End state** The docs cannot drift from the code.
- **Why** A wrong chain description sends every future debugging session in the wrong direction.
- **Depends** none · **Verify** The new unit test passes; `pydoc` output lists all four providers.
- **Effort** XS · **Risk** none.

## BUG-067 · `ignored_wake` runs on the stripped text and eats legitimate "navigate…" commands
- **Severity** High · **Type** accuracy
- **Evidence** `voice.py:208` returns `command_text(transcript)`; `:406-409` then tests `ignored_wake(transcript)` against `^(navigation|navigator|navigate)\b` and blanks the command.
- **Root cause** The false-wake guard was designed for ambient speech that *starts* with "navigation", but after stripping "Navi," a legitimate command starts the same way.
- **Fix** Run `ignored_wake` on the **raw** transcript before stripping; if the raw starts with navigation/navigator/navigate and the KWS fired on the bare "Navi" keyword, treat it as a false wake; once a wake is confirmed and audio was trimmed (BUG-049), never blank a command.
- **End state** "Navi, navigate to the files page" executes; ambient "navigation" still does not wake.
- **Why** A silent dropped command is indistinguishable from a broken microphone.
- **Depends** BUG-049 · **Verify** Two fixtures: raw "navigation is hard" → ignored; raw "Navi navigate to X" → dispatches "navigate to X".
- **Effort** S · **Risk** low.

## BUG-068 · Hex's availability check caches a single failure forever
- **Severity** High · **Type** robustness
- **Evidence** `hex_asr.py:34` (`_installed` process-global), `available()` caches `False` on any exception or non-zero `model status` (10 s timeout), and only `refresh=True` re-checks.
- **Root cause** A negative result is treated as permanent, including transient failures.
- **Fix** Cache positives for the process; cache negatives for 60 s with a failure counter; distinguish "binary missing" (permanent until mtime changes) from "probe failed" (retry); re-check on worker restart and when `NAVI_HEX` changes.
- **End state** One slow `model status` no longer disables the preferred local engine until the worker restarts.
- **Why** This silently demotes transcription quality for the rest of the session.
- **Depends** none · **Verify** Stub `hex` that fails once then succeeds; assert the second call retries and enables; a missing binary stays disabled without repeated probes.
- **Effort** S · **Risk** low.

---

### Corrections applied during review
- The stale chain docstring is in `cloud_stt.py:1`, not `speech_config.py`; `speech_config` itself documents the order correctly and configurable.
- Worst-case mic block is ≈60 s (4 cloud timeouts + Hex 45 s + decode), not 45 s.
- `voice.offline()` exists (`voice.py:211`) and is the right harness for synthetic tests; `scripts/test_voice_stream.py` exists and already covers the endpoint/overflow cases.
- `ignored_wake` ordering (BUG-067) and Hex's negative cache (BUG-068) were found during this review; both directly harm accuracy.
