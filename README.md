# oxideav-audiotoolbox

macOS AudioToolbox hardware decode/encode bridge for the [oxideav](https://github.com/OxideAV/oxideav) framework.

## Why a bridge crate?

Apple's [AudioToolbox](https://developer.apple.com/documentation/audiotoolbox) exposes the dedicated audio codec engine on Apple Silicon (and equivalent paths on Intel Macs). For AAC encode/decode this is the canonical "hardware AAC" path historically credited with iPhone-quality encodes at very low CPU cost.

This crate is a **thin runtime-loaded bridge** — no compile-time link dependency on AudioToolbox, no Objective-C / Swift. The framework is opened via [`libloading`] on first use.

## Fallback behaviour

Two distinct failure paths fall back automatically to the pure-Rust codec:

1. **Load failure** — older macOS, missing framework, sandboxed environment without AT entitlements. `register()` logs and returns without registering, so the SW codec is the only candidate at dispatch.
2. **Init failure** — `AudioConverterNew` returns a non-zero `OSStatus` for the requested ASBD. Common triggers: unsupported sample rate / channel layout, encoder bitrate the device doesn't accelerate, hardware codec slot busy (concurrent-converter cap on iOS-class hardware). The factory returns `Err`; the registry retries the next-priority impl (typically the SW one).

Pipelines that require hardware can opt out of the SW fallback by setting `CodecPreferences { require_hardware: true, .. }` — the registry will then surface the `OSStatus` error instead of degrading silently.

## Platform gating

The whole crate is `#![cfg(target_os = "macos")]`. On Linux / Windows it compiles to an empty rlib; the umbrella `oxideav` crate gates the `register` call behind the same cfg.

## Priority

Hardware factories register with `priority = 10` — **lower numbers win at resolution time**, so on macOS the AudioToolbox path is preferred over the pure-Rust implementation (default priority 100).

## Coverage

| Codec     | Decode  | Encode  | HW-accelerated |
|-----------|---------|---------|----------------|
| AAC LC    | yes     | yes     | yes (Apple Silicon / hardware audio codec engine) |
| HE-AAC v1 | yes     | yes     | yes (LC + SBR, 2× frame upsample)                  |
| HE-AAC v2 | yes     | yes     | yes (LC + SBR + Parametric Stereo, stereo only)    |
| AAC-LD    | yes     | yes     | yes (low-delay AOT 23, 512-frame core, ~20 ms)    |
| AAC-ELD   | yes     | yes     | yes (enhanced low-delay AOT 39, 512-frame core)   |
| ALAC      | yes     | yes     | yes (lossless, S16 / S32 PCM)                     |
| iLBC      | yes     | yes     | yes (RFC 3951 narrow-band speech, 8 kHz mono, 20 ms + 30 ms modes) |
| AMR-NB    | yes     | n/a     | yes (3GPP TS 26.071 narrow-band speech, 8 kHz mono, 8 speech modes + SID + NO_DATA; AT is decode-only) |
| AMR-WB    | yes     | n/a     | yes (3GPP TS 26.171 / 26.201 wideband speech, 16 kHz mono, 9 speech modes + SID + NO_DATA; AT is decode-only) |

Round 2 SNR measurement: encode → decode 440 Hz sine at 48 kHz / stereo / 128 kbit/s → **36.7 dB** per channel (well above 25 dB threshold).

Round 3 ALAC round-trip: encode → decode a 2-second sine+LCG-noise mix at 48 kHz / 16-bit stereo, **190,464 / 192,000 samples bit-exact** (zero priming silence on the AT path) — proves the encoder's vended magic cookie + decoder property wiring is correctly wired and the codec is truly lossless end-to-end.

Round 4 HE-AAC round-trip: encode → decode a 2-second 1 kHz sine at 48 kHz / stereo, **HE-v1 @ 64 kbit/s ≈ 11 dB SNR, HE-v2 @ 32 kbit/s ≈ 10 dB SNR** per channel — SBR's patch-and-scale upper-band reconstruction caps recoverable phase fidelity well below transparency, so the test asserts only that the pipeline is wired correctly (the framework's encoder quality is not under our control). Profile selection is via `CodecParameters::options.insert("profile", "he" | "he-v2")`. HE encoder publishes a 42-byte ISO/IEC 14496-1 esds descriptor (AOT extension) through `output_params.extradata`; the matching decoder consumes it via `kAudioConverterDecompressionMagicCookie` to bypass the `kAudioCodecBadDataError` (`'bada'`) rejection that plain AAC LC config triggers on HE bitstreams.

Round 5 AAC-LD / AAC-ELD round-trip: encode → decode a 2-second 1 kHz sine at 48 kHz / stereo @ 128 kbit/s, **AAC-LD ≈ 29 dB SNR, AAC-ELD ≈ 26 dB SNR** per channel. These are the conferencing-oriented low-delay AOTs (`kAudioFormatMPEG4AAC_LD` = `'aacl'`, AOT 23; `kAudioFormatMPEG4AAC_ELD` = `'aace'`, AOT 39): the shortened analysis/synthesis window cuts algorithmic delay to ~15-20 ms (against AAC LC's ~100+ ms) — the win is latency, not compression, so they run at full-band LC-class bitrates and reach near-transparent SNR. AudioConverter packetises both at **512 PCM frames per packet** with **no SBR upsample** at the converter boundary (output rate = input rate). Selected via `CodecParameters::options.insert("profile", "ld" | "eld")`. Neither has an ADTS representation (ADTS profile bits encode only Main/LC/SSR/LTP), so — like HE — packets are emitted as raw AAC bytes and the AOT travels out-of-band in the encoder-vended magic cookie (`output_params.extradata`), consumed on decode via `kAudioConverterDecompressionMagicCookie`.

Round 7 AMR-NB (Adaptive Multi-Rate Narrowband, 3GPP TS 26.071 / RFC 4867) **decode**: a 20 ms / 8 kHz mono narrow-band speech codec with **8 speech modes** (MR475 4.75 kbit/s → MR122 12.2 kbit/s), plus an SID (silence-descriptor) frame and a NO_DATA (DTX-skip) marker. AudioToolbox exposes AMR-NB through `kAudioFormatAMR` (`'samr'`) as a **decode-only** target — there is no paired AT encoder, so the registry only installs an `amr_nb` decoder factory. Storage-format packets per RFC 4867 §5.1: one **TOC byte** (`F FFFF Q PP`) carrying frame-type + follow-up + quality bits, followed by the per-mode compressed payload (1, 6, 13, 14, 16, 18, 20, 21, 27 or 32 total bytes including the TOC). The decoder validates each incoming packet against the TOC-derived size table before queueing, rejecting both reserved frame types (FT 9..=14) and size mismatches with `Error::invalid`. Empirical AT behaviour: the decoder vends PCM in **120-sample S16 mono blocks** (15 ms at 8 kHz) per `FillComplexBuffer` call rather than the 160-sample analysis frame size — an internal AT chunking detail that the per-frame size invariant (`af.samples × 2 == af.data[0].len()`) handles transparently. Persistent input-queue + one-packet-of-slack lookahead pattern (matching iLBC / HE-AAC) so AT never sees "0 packets" mid-stream. Tags claimed: FourCC `'samr'` (3GPP / ISOBMFF) + Matroska `A_AMR/NB`. No magic cookie — the per-packet TOC byte carries everything AT needs.

Round 8 AMR-WB (Adaptive Multi-Rate Wideband, 3GPP TS 26.171 / 26.201 / RFC 4867) **decode**: a 20 ms / 16 kHz mono wideband speech codec with **9 speech modes** (MR660 6.60 kbit/s → MR2385 23.85 kbit/s), plus an SID (silence-descriptor) frame at FT=9 — one slot higher than AMR-NB's FT=8 — and a NO_DATA (DTX-skip) marker at FT=15. AudioToolbox exposes AMR-WB through `kAudioFormatAMR_WB` (`'sawb'`) as a **decode-only** target (same asymmetry as AMR-NB — no paired AT encoder), so the registry only installs an `amr_wb` decoder factory. Storage-format packets per RFC 4867 §5.3: one **TOC byte** (`F FFFF Q PP`, layout shared with AMR-NB), followed by the per-mode compressed payload — 1, 6, 17, 23, 32, 36, 40, 46, 50, 58, or 60 total bytes including the TOC (the speech-mode body bit counts are 132/177/253/285/317/365/397/461/477 per 3GPP TS 26.201 Table 2, so `ceil(bits/8) + 1` for the storage byte count). The decoder validates each incoming packet against the TOC-derived size table before queueing, rejecting reserved frame types (FT=10..=14 for AMR-WB) and size mismatches with `Error::invalid`. Persistent input-queue + one-packet-of-slack lookahead pattern (same as iLBC / AMR-NB / HE-AAC) so AT never sees "0 packets" mid-stream. Tags claimed: FourCC `'sawb'` (3GPP / ISOBMFF) + Matroska `A_AMR/WB`. No magic cookie — the per-packet TOC byte carries everything AT needs.

Round 6 iLBC (Internet Low Bitrate Codec, RFC 3951) decode + encode: a narrow-band telephony speech codec, fixed at **8 kHz mono**. Two block sizes — 20 ms (160 PCM frames → 38 compressed bytes, 15.2 kbit/s) and 30 ms (240 PCM frames → 50 compressed bytes, 13.33 kbit/s) — selected via `CodecParameters::options.insert("mode", "20" | "30")`, defaulting to 30 ms (the compression-favoured mode used by most SIP / RTP gateways). AudioConverter (`kAudioFormatiLBC` = `'ilbc'`) wires the mode purely through the compressed-side ASBD `frames_per_packet` field — there is **no magic cookie**, no in-band signalling, no probe descriptor. Round-trip on a 2-second 1 kHz sine at 8 kHz mono yields **peak SNR ≈ 10.7 dB (30 ms)** and **≈ 7.8 dB (20 ms)**; iLBC is a CELP-class voice codec, so a pure sine is intentionally adversarial (the codebook is voice-tuned) and these SNRs prove the pipeline is wired correctly without claiming transparency, which iLBC doesn't target. Decoder uses a persistent input-packet queue (the same one-packet-of-slack lookahead pattern as the HE-AAC decoder) because AT's iLBC analysis filter draws samples from one or two prior compressed packets when producing a PCM block; without the slack the input callback would return zero mid-stream and AT would silently halt. Encoder echoes the active mode through `output_params.options["mode"]` and the net bitrate (15200 or 13333) through `output_params.bit_rate` for downstream muxer awareness.

## Opt-out

Disable hardware acceleration globally via `CodecPreferences { no_hardware: true, .. }` or the `oxideav` CLI's `--no-hwaccel` flag. The runtime context still registers AT — `oxideav list` shows the `aac_audiotoolbox` row regardless of the flag — only resolution is biased toward the SW path.

## Feature flags

| Feature    | Default | Description                                    |
|------------|---------|------------------------------------------------|
| `registry` | on      | Wires in `oxideav-core` Decoder/Encoder traits and registers factories into the runtime codec registry. Turn off (`default-features = false`) to use only the raw AudioToolbox bridge bindings without the oxideav framework dependency. |

## Coverage roadmap

| Codec        | Decode              | Encode               |
|--------------|---------------------|----------------------|
| AAC LC       | done (round 2)      | done (round 2)       |
| ALAC         | done (round 3)      | done (round 3)       |
| HE-AAC v1    | done (round 4)      | done (round 4)       |
| HE-AAC v2    | done (round 4)      | done (round 4)       |
| AAC-LD       | done (round 5)      | done (round 5)       |
| AAC-ELD      | done (round 5)      | done (round 5)       |
| iLBC         | done (round 6)      | done (round 6)       |
| AMR-NB       | done (round 7)      | n/a (AT decode-only) |
| AMR-WB       | done (round 8)      | n/a (AT decode-only) |
| FLAC         | available (decode + encode via AudioConverter, macOS 13+) | available |
| Opus         | available (decode + encode via AudioConverter)             | available |
| MP3          | available (decode-only on macOS)                           | n/a       |

## Workspace policy

Calling a system OS framework via FFI is the same shape as calling `libc::malloc` — it's the platform, not a copied algorithm. The workspace's clean-room rule does not apply to this crate.

## License

MIT.
