# Local models for Navi: research snapshot, October 10, 2026

## Scope after clarification

The user accepts English for commands such as opening a window, launching Codex or Claude Code, and switching sessions. The local model controls Navi; installed coding harnesses perform coding work. A conversational local assistant is optional.

Recommendation: evaluate **Needle 3** first as the command model, paired with **Whistle** if speech recognition is also local. Keep the model behind a replaceable interface until the actual command evaluation passes. This is a research recommendation, not a measured local performance result.

Hardware checked in this session: RTX 4050 Laptop GPU, 6,141 MiB VRAM; 22 GiB system RAM, approximately 14 GiB available at the check. CPU-oriented command inference would leave the GPU available for other work.

No models or runtimes were installed. Existing architecture documents and runtime code were not changed.

## Shortlist for English commands

Sizes below are **published files**, in decimal MB/GB, checked through Hugging Face file metadata. They are not resident memory requirements or latency measurements.

| Candidate | Published size checked | Output and fit | Main limitation |
| --- | --- | --- | --- |
| [Needle 3](https://huggingface.co/Cactus-Compute/needle3) | [Full .cact: 35.3 MB](https://huggingface.co/Cactus-Compute/needle3/blob/main/needle3.cact) | Tool calls with arguments; strongest first experiment for launch/focus/switch commands | Custom Cactus runtime; application-specific reliability still untested |
| [GLiNER2.5-small-v1](https://huggingface.co/fastino/gliner2.5-small-v1) | [FP32 weights: 295.6 MB](https://huggingface.co/fastino/gliner2.5-small-v1/blob/main/model.safetensors) | 74M English extraction/classification model; identify intent and extract named targets | Navi must convert extracted fields into calls and resolve targets |
| [d1-omni-600M](https://huggingface.co/LiquidAI/d1-omni-600M) | [Q8 trunk plus Q8 multimodal projection: about 670 MB](https://huggingface.co/LiquidAI/d1-omni-600M-GGUF/tree/main) | Experimental 587M model; choose predefined actions from text or English audio | Does not generate arbitrary argument strings; early research checkpoint |
| [d1-3B](https://huggingface.co/LiquidAI/d1-3B) | [Q4 language weights: 1.67 GB; Q8 image projection adds 583 MB](https://huggingface.co/LiquidAI/d1-3B-GGUF/tree/main) | More capable typed decisions over text and screenshots | Larger; unnecessary for basic launch commands unless smaller candidates fail |

### Needle 3

The publisher describes a 29–121M parameter family with smaller subnetworks. It produces structured calls and extractions rather than conversational replies. The advertised size range is 8–29 MB; the current full published binary is 35,335,380 bytes. Use the current artifact size for budgeting. [Release description](https://cactuscompute.com/needle), [current artifact](https://huggingface.co/Cactus-Compute/needle3/blob/main/needle3.cact).

Integration should call `complete()` and let Navi execute the returned call. The package also offers `run()`, which executes decorated functions itself. Tool retrieval selects only five tools when a catalogue is larger; the selected set must cover multi-action requests. Source documentation describes default binary telemetry; configure `NEEDLE_TELEMETRY=0` and `DO_NOT_TRACK=1` for the local deployment and verify offline behavior. [API and retrieval](https://cactuscompute.com/blog/needle-python-docs), [repository](https://github.com/cactus-compute/needle).

### GLiNER2.5

The family was announced August 24, 2026. The small English checkpoint is suitable for CPU extraction and classification. The multilingual extraction checkpoint is 287M. Separate Decide checkpoints, announced September 24, specialize in selecting labels; they are not conversational models. The small extraction model can be more useful than a pure label selector when commands contain a session name or window title. [Family and sizes](https://huggingface.co/fastino/gliner2.5-small-v1), [Decide announcement](https://fastino.ai/blog/gliner-2-5-decide-open-weight-decision-model).

For English-only Navi, there is no immediate reason to pay the memory cost of the multilingual variant. A span extractor still needs a deterministic resolver for installed harnesses, session IDs, and windows.

### Liquid d1

The open checkpoints were released October 7, 2026. d1 answers typed questions with choices, booleans or ratings in one forward pass. d1-omni handles text with images or text with audio; its audio training uses English requests. Direct audio input could remove the separate transcription step for commands selecting existing targets, but arbitrary new names and prompts still require another extraction path. [Release](https://www.liquid.ai/blog/d1-open), [omni model card](https://huggingface.co/LiquidAI/d1-omni-600M).

The publisher reports Decision Index 48.57 for d1-3B versus 15.95 for the experimental omni checkpoint. These are publisher results, not evidence that either handles Navi's command catalogue accurately. Its GGUF interface uses `/v1/systemone`; generic chat-completion examples generated by hosting platforms are not the decision API. [Release evaluation](https://www.liquid.ai/blog/d1-open), [GGUF usage](https://huggingface.co/LiquidAI/d1-3B-GGUF).

## Local English voice

**Whistle**, released October 2, 2026, is the smallest recent speech candidate found that fits the revised scope. Its file is 16,919,407 bytes, it runs on CPU, and it supports English plus six European languages. It handles clips up to 30 seconds and shares Needle's runtime. Full Needle 3 plus Whistle files total **52,254,787 bytes, about 52.3 MB**. Runtime and audio buffers add memory. [Announcement](https://www.cactuscompute.com/blog/whistle), [artifact](https://huggingface.co/Cactus-Compute/whistle/blob/main/whistle.cact).

Its published speed results were measured on an Apple M4 Pro; do not treat them as timings for this Ryzen laptop. Test the pronunciation of “Codex,” “Claude,” project names, and window titles.

If recognition is inadequate, [Qwen3-ASR-0.6B](https://huggingface.co/Qwen/Qwen3-ASR-0.6B) is a larger streaming/offline alternative supporting English and Arabic. If spoken acknowledgements are wanted, [Supertonic 3](https://huggingface.co/Supertone/supertonic-3) is approximately 99M parameters, runs through ONNX on CPU, and supports English and Arabic. Speech synthesis is optional for command execution; a fixed success/failure tone or text acknowledgement can suffice.

## How this fits Navi

```text
Microphone -> local speech recognition -> local command model
                                           |
                                      proposed call
                                           |
                       Navi resolves target and executes adapter
                                           |
                             actual result -> acknowledgement
```

Example expected interpretation, not a tested model output:

```text
"Open Codex"                 -> launch_harness(harness="codex")
"Switch to the Navi session" -> switch_session(query="Navi")
"Focus the browser"          -> focus_window(query="browser")
```

The model is the first decision point. Navi maintains actual sessions, process handles, installed harnesses, and window metadata. “This window” must resolve through the current selection or desktop state; a language model cannot infer an unseen pointing target. Browser commands require a browser adapter, and window focus requires the desktop's window-control interface.

Start with explicit tools for launch, session switching, and window focus. Add browser commands as their adapters become available. These command capabilities do not require the local model to solve coding tasks or autonomously navigate arbitrary websites.

## Larger models retained for a broader chief

These remain alternatives if the intended control layer later needs conversation, open-ended planning, or complex tool sequences. Four-bit files are quantized weights, with additional runtime memory required.

| Model | Release | Published Q4_K_M file checked | Reason to retain |
| --- | --- | --- | --- |
| [LFM2.5-2.6B](https://www.liquid.ai/blog/lfm2-5-2-6b) | August 4, 2026 | [1.67 GB](https://huggingface.co/LiquidAI/LFM2.5-2.6B-GGUF/tree/main) | CPU-oriented tool use and multi-step workflows |
| [Spark-X2.5-1.7B / 4B](https://github.com/XHToken/Spark-X2.5) | September 1, 2026 | [1.11 GB](https://huggingface.co/XHToken/Spark-X2.5-1.7B-GGUF/tree/main) / [2.60 GB](https://huggingface.co/XHToken/Spark-X2.5-4B-GGUF/tree/main) | Compact general tool-calling candidates |
| [MiniCPM5-2B](https://github.com/OpenBMB/MiniCPM) | September 7, 2026 | [1.56 GB](https://huggingface.co/openbmb/MiniCPM5-2B-GGUF/tree/main) | Small model aimed at local assistants and tool use; 2.52B total parameters |
| [Granite 4.2-3B](https://research.ibm.com/blog/introducing-granite-4-2) | August 25, 2026 | [2.24 GB](https://huggingface.co/ibm-granite/granite-4.2-3b-GGUF/tree/main) | Tool calling, multilingual dialogue, selectable thinking effort |

[LFM's card](https://huggingface.co/LiquidAI/LFM2.5-2.6B) says it always thinks before answering, so low latency for trivial commands needs measurement. [MiniCPM's card](https://huggingface.co/openbmb/MiniCPM5-2B) describes XML-style calls and recommends SGLang's dedicated tool parser. A GGUF file alone does not validate tool parsing in a particular runtime.

Other current candidates: [Qwen3.5-0.8B/2B/4B](https://huggingface.co/Qwen/Qwen3.5-4B), [Nanbeige4.2-3B](https://huggingface.co/Nanbeige/Nanbeige4.2-3B), [Gemma 4 E2B](https://huggingface.co/google/gemma-4-E2B-it), and [Ling-3.0-tiny](https://huggingface.co/inclusionAI/Ling-3.0-tiny). Naming can hide memory costs: Nanbeige has approximately 4.17B total parameters; Gemma E2B has 5.1B including embeddings; Ling has 7.9B total despite 1.3B active per token. These are lower priority for the narrowed command scope.

## Evaluation before choosing

Use a small dry-run command set with the same exposed tools and targets for every candidate. Include:

- Paraphrases: “open Codex,” “start Codex here,” “launch Claude in the Navi folder.”
- Negation and ambiguity: “don't open Claude,” “open that one” with no selected target, duplicate session names.
- Combined commands: “open Codex, then switch to the existing Claude session.”
- Names: installed harnesses, real project aliases, session names and window titles.
- Voice conditions: quiet speech, normal room noise, clipped starts, pauses and corrections.

Measure exact action/argument accuracy, wrong-target actions, clarification behavior, end-to-end latency, and peak memory on this laptop. Keep classification scores separate from actual command success. Published benchmarks use different tasks and runtimes and do not establish a universal best model.

Choose Needle 3 if it passes this narrow evaluation. Use GLiNER2.5-small with a resolver if simple intent/extraction is sufficient and easier to maintain. Consider d1-omni for direct English-audio decisions over known targets. Move to a larger generative model only if the smaller options fail required command behavior.
