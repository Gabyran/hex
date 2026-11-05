# @kitlangton/hex

## 0.3.2

### Patch Changes

- 5c5f0f6: `ModelId` also takes `custom-<16 hex digits>`, a model imported with Labs › Use your own speech model, and model listings keep imported models instead of dropping them.
- 54d8f53: `ModelInfo` gains `source`: `download`, `hex`, `imported`, or `found: owner/name@abcdef1` for a model another app downloaded to the Hugging Face cache, which Hex uses where it is. It is `null` from older hosts.
- efa1aa0: `ModelId` gains `gigaam_v3`, ai-sage's GigaAM v3 e2e RNN-T: Russian only, cased and punctuated.
- 8a327aa: `ModelId` gains `granite_speech_5`, IBM's Granite Speech 5.0 470M TurboCTC: English, French, German, Spanish, Portuguese, Italian, and Japanese, cased and punctuated.
- e76afaa: `connect()` reads the macOS discovery file from `~/Library/Application Support/hex`, where Hex now keeps its data.
- d86d494: `ModelId` gains `ivrit_whisper_large_v3_turbo`, ivrit.ai's Hebrew fine-tune of Whisper large-v3-turbo.
- 9c91096: `connect()` finds the running Hex on Linux (`$XDG_DATA_HOME/hex`) and Windows (`%APPDATA%\hex`) as well as macOS.
- 637b1f9: `ModelId` gains `nb_whisper_medium`, the National Library of Norway's Whisper medium fine-tune for Norwegian.
- caa2917: `ModelId` gains `nemotron_3_5_asr`, NVIDIA's Nemotron 3.5 ASR Streaming 0.6B: 27 languages, streaming, and automatic language detection.
- dff0d7e: `ModelId` gains `parakeet_ultra`, Moondream's Parakeet Ultra (Labs › Parakeet Ultra).

## 0.3.1

### Patch Changes

- 04a42da: Decode model progress and dictation levels with shared bounded SSE framing. Large transport chunks containing many small level events no longer fail the event limit, and CR-delimited events dispatch without waiting for more data or EOF. Cancellation, callback failures, and bounded observation buffers remain supported.

## 0.3.0

### Minor Changes

- c5bffe3: Add a model-bound `create({ model, language, onProgress })` API that returns a
  ready transcriber for WAV bytes. The Promise transcriber provides explicit,
  idempotent cleanup; the Effect transcriber is scope-owned. Failed creation and
  in-flight transcription cancellation wait for helper cleanup. Existing low-level
  `create()` and desktop `connect()` remain available.

  Effect scope cleanup failures now surface as defects instead of being logged and
  discarded. The low-level and model-bound option types are distinct so shared
  configuration cannot accidentally change the returned resource's shape.

  Startup cleanup failures are surfaced instead of hiding a potentially live child.
  Cancellation also takes precedence over a buffered result that has not settled.

  Embedded use still requires an explicit native helper command until bundled
  platform artifacts are published.

## 0.2.2

### Patch Changes

- e163753: Support Effect 4.0.0-rc.112 and newer in the Effect client, using the current tagged-error API.

## 0.2.1

### Patch Changes

- c5ba7d4: Stop dictation heartbeat timers after terminal capture errors while preserving retries for transient failures.

## 0.2.0

### Minor Changes

- Require local API v2 for owner-scoped dictation capture, rejecting legacy HEX
  apps before a recording can start and become orphaned.
