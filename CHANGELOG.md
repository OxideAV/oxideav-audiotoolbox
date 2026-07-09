# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Round 401: typed `OSStatus` error taxonomy (`status` module)**.
  Every CoreAudio failure code the bridge can encounter now has a
  typed home instead of surfacing as a bare integer in an error
  string. `AtStatus::from_raw` classifies all 29 documented codes from
  the platform SDK's `AudioConverter.h` / `AudioCodec.h` /
  `AudioFormat.h` / `CoreAudioBaseTypes.h` error enums (with an
  `Unknown(raw)` tail-case so classification is total and
  `from_raw(s).as_raw() == s` always); `name()` recovers the platform
  constant name; `kind()` buckets each code into the coarse semantics
  a caller acts on (`Unsupported` / `InvalidData` /
  `ResourceExhausted` / `Usage` / `Other`); and the `Display` impl
  renders `name ('fourcc' / raw)` so logs carry both the human name
  and the greppable integer (the historically inscrutable
  `1650549857` now reads `kAudioCodecBadDataError ('bada' /
  1650549857)`). Behind the `registry` feature, `status_error(op,
  raw)` converts an operation + status pair straight into the
  matching `oxideav_core::Error` variant — `'fmt?'` → `Unsupported`
  (registry falls back to the pure-Rust impl), `'bada'` →
  `InvalidData`, `'hwiu'` / `'!buf'` / `-108` → `ResourceExhausted`.
  7 unit tests pin the full raw↔variant↔name↔kind table, value
  distinctness, Display rendering, and the core-error mapping.

- **Round 401: converter-property + global AudioFormat sys surface**.
  The `sys` module grows the introspection half of the AudioConverter
  property set — buffer sizing (`'mobs'` / `'cibs'` / `'cobs'`),
  current input/output stream descriptions (`'acsd'` / `'acod'`),
  applicable/available encode bit-rate and sample-rate queries
  (`'aebr'` / `'vebr'` / `'aesr'` / `'vesr'`), edge-priming control
  and introspection (`'prmm'` prime method with its Pre / Normal /
  None values, `'prim'` prime info), and the two quality selectors
  (`'srcq'` / `'cdqu'` with the five Max…Min values) — plus the two
  C ABI structs those properties traffic in (`AudioValueRange` with
  `is_discrete` / `contains`, `AudioConverterPrimeInfo` whose
  `leading_frames` is the AAC encoder-delay/priming figure) and the
  *global* converter-less AudioFormat query surface:
  `audio_format_get_property_info` / `audio_format_get_property`
  bindings with the `'fmti'` / `'acof'` / `'acdf'` / `'aebr'` /
  `'aesr'` / `'fvbr'` / `'fexf'` property IDs. 5 new tests pin every
  FourCC byte-for-byte, the predicate behaviour, the C struct sizes
  (16 / 8 / 40 / 16 bytes), and that both AudioFormat entry points
  resolve from the real framework.

- **Round 319: `AudioFormatId` family / codec-id / FourCC accessors**
  (sys-surface introspection). Round 265 added the typed `AudioFormatId`
  classifier and its `is_linear_pcm` / `is_compressed_audio` predicates.
  This round extends that classifier with the four introspection axes
  CoreAudio FFI consumers actually need beyond "PCM vs compressed",
  making the enum the single source of truth for the
  format-id ⇒ codec metadata mapping that `lib.rs::register()`
  previously hand-coded inline:
  - **`AudioFormatId::codec_id_str`** — the canonical oxideav codec-id
    string each AudioToolbox slot maps to (`"aac"` / `"alac"` / `"ilbc"`
    / `"amr_nb"` / `"amr_wb"` / `"mp1"` / `"mp2"` / `"mp3"` / `"flac"` /
    `"opus"` / `"pcm"`), matching the registry keys in `register()`.
    The five AAC AOT slots collapse to the single `"aac"` id; `Unknown`
    returns `None`.
  - **`AudioFormatId::is_lossless`** — true only for ALAC + FLAC (the
    slots whose output PCM width must preserve full source bit depth via
    the S32 path rather than truncating to S16).
  - **`AudioFormatId::is_aac_family`** — groups the five MPEG-4 AAC AOT
    slots (LC / HE / HE-v2 / LD / ELD) that share the `"aac"` codec id
    and the one AudioConverter encode/decode path.
  - **`AudioFormatId::fourcc_str`** — renders the packed big-endian
    FourCC back to a clean four-character string (non-printable bytes
    map to `'.'`), for debug / error reporting; `Unknown` renders its
    raw value the same way.
  - **`AudioStreamBasicDescription::is_lossless` / `codec_id_str`** —
    convenience accessors forwarding to the typed enum from an
    already-built ASBD.
  - 7 new unit tests pin the surface (fourcc-string render over all 15
    wired constants + non-printable/unknown handling; codec-id mapping
    per slot; a lockstep pin against the `register()` registry keys;
    `is_lossless` restricted to ALAC + FLAC; `is_aac_family` over the
    five AOTs; ASBD-side lossless/codec-id accessors). Existing FFI
    paths and constructors are byte-identical — the surface is purely
    additive (205 → 212 lib tests).

- **Round 265: typed `AudioFormatId` classifier for ASBD `format_id`**
  (sys-surface tightening). The CoreAudio C surface treats
  `AudioStreamBasicDescription::format_id` as a raw `u32` FourCC
  (`kAudioFormat*`). That works for setting the field but loses type-
  system traction the moment a caller wants to introspect "which codec
  is this ASBD describing" — every introspection ends up as a `match`
  ladder against bare integer constants. This round adds:
  - **`AudioFormatId` enum** in `src/sys.rs` with one variant per wired
    `K_AUDIO_FORMAT_*` constant (15 named arms: `LinearPcm` +
    14 compressed codec slots covering AAC-LC / AAC-HE / AAC-HE-v2 /
    AAC-LD / AAC-ELD / ALAC / iLBC / AMR-NB / AMR-WB / MPEG Layer I /
    II / III / FLAC / Opus) plus an `Unknown(u32)` tail-case that
    preserves the raw value for unwired FourCCs so the classifier stays
    total.
  - **`AudioFormatId::from_u32` / `as_u32`** — round-trip pair;
    `from_u32(raw).as_u32() == raw` for every value (the `Unknown`
    tail-case carries the input verbatim).
  - **`AudioFormatId::is_linear_pcm` / `is_compressed_audio`** — typed
    predicates that split the wired set on the only axis CoreAudio
    FFI consumers actually branch on. `Unknown` returns `false` on
    both; callers needing to act on an unwired FourCC must match the
    variant directly.
  - **`AudioStreamBasicDescription::audio_format_id`** — typed view of
    the `format_id` field; equivalent to
    `AudioFormatId::from_u32(self.format_id)` but reads naturally at
    call sites that already hold an ASBD.
  - **`AudioStreamBasicDescription::is_linear_pcm` /
    `is_compressed_audio`** — convenience predicates forwarding to
    the typed enum.
  - 5 new unit tests pin the surface:
    `audio_format_id_classifies_every_wired_constant` (round-trip
    over all 15 named variants), `audio_format_id_unknown_round_trips_raw_value`
    (a `'zzzz'` = 0x78787878 FourCC stays as `Unknown` and round-trips
    + reports `false` on both predicates),
    `audio_format_id_predicate_axes` (every wired variant satisfies
    `is_linear_pcm() XOR is_compressed_audio()`),
    `asbd_audio_format_id_accessor_matches_constructors` (17 ASBD
    constructors covering every named variant: three PCM
    constructors + every compressed-codec constructor),
    `asbd_predicates_split_pcm_vs_compressed_constructors`
    (constructor-side check that PCM constructors satisfy
    `is_linear_pcm()` only and every compressed constructor
    satisfies `is_compressed_audio()` only), and
    `asbd_unknown_format_id_falls_through_predicates`
    (hand-crafted unwired FourCC reports `false` on both predicates).
  - Adding a new codec slot is now a four-line change: a
    `K_AUDIO_FORMAT_*` const, an enum variant, and the two map arms
    in `from_u32` / `as_u32`. Existing FFI paths and constructors are
    byte-identical — the surface is purely additive.

- **Round 224: Opus decoder + encoder via `kAudioFormatOpus`** (RFC
  6716 + RFC 7845 + RFC 8251 / Apple `CoreAudioBaseTypes.h`
  `kAudioFormatOpus = 'opus'`). Wires both halves of the symmetric
  AT Opus bridge in a single round.
  - New `src/opus.rs` — wall-isolated OpusHead parser / builder per
    RFC 7845 §5.1 Figure 2 (8-byte `OpusHead` magic + 1-byte version
    + 1-byte channel count + 16-bit pre-skip + 32-bit input sample
    rate + 16-bit Q7.8 output gain + 8-bit mapping family + optional
    mapping table). All multi-byte fields little-endian per §5.1
    bullets 4–6. Mapping family 0 (RTP RFC 7587, mono/stereo)
    serialises to a 19-byte cookie; families 1 and 255 include the
    extra mapping-table tail. `frames_per_packet_48k` maps the six
    valid RFC 6716 Table 2 frame durations
    (2.5 / 5 / 10 / 20 / 40 / 60 ms) to PCM-frame counts
    (120 / 240 / 480 / 960 / 1920 / 2880) via
    `fpp = sample_rate * duration_ms / 1000`.
  - New `src/opus_decoder.rs` (`OpusAtDecoder`) implementing
    `oxideav_core::Decoder`. Input: one raw Opus packet per
    `Packet` (RFC 6716 §3.1 TOC byte + per-code framing); output:
    interleaved S16 PCM at the configured output rate (8 / 12 / 16
    / 24 / 48 kHz per RFC 6716 §2.1.1). Magic cookie resolved from
    `CodecParameters::extradata` when present (must parse as a
    valid OpusHead) or synthesised from explicit `(sample_rate,
    channels)` for standalone-test paths.
  - **Empirical AT-side discovery (r224 probe)**: AT's Opus
    decoder slot **locks the converter at end-of-stream the moment
    the input callback returns `0 packets`** — a single empty-
    queue callback is enough to enter the lockout state, after
    which subsequent FCB calls produce 0 PCM frames regardless of
    further input. The fix is a strict "drain only while the queue
    holds at least 2 packets" discipline plus bounding the per-call
    PCM output to one packet's worth of frames
    (`io_number_data_packets = sample_rate * 20 / 1000`) so AT
    consumes one Opus packet per FCB and the slack invariant
    holds.
  - New `src/opus_encoder.rs` (`OpusAtEncoder`) implementing
    `oxideav_core::Encoder`. Input: interleaved S16 or F32 PCM at
    one of the five valid Opus rates; output: one raw Opus packet
    per encoded block. Bit-rate plumbed through
    `kAudioConverterEncodeBitRate` (default 96 000 bit/s; AT
    quantises to its own grid and the actual value is read back
    into `output_params.bit_rate`). Frame duration configurable via
    `options.insert("frame_duration_ms", "<one of
    2.5/5/10/20/40/60>")`; default per-packet duration: 20 ms
    (`fpp = sample_rate * 20 / 1000`). Persistent `Box<PcmContext>`
    PCM feeder with the one-packet-of-slack discipline (same shape
    as the FLAC encoder's). The encoder publishes an RFC 7845 §5.1
    OpusHead through `output_params.extradata` for downstream Ogg
    / MP4 / WebM muxer use; AT's own compression-magic-cookie
    property returns an opaque AT-internal blob whose layout is
    not documented in `CoreAudioBaseTypes.h`, so the bridge does
    not forward it to consumers.
  - New `tests/opus_roundtrip.rs`: 2-second 1 kHz sine at 48 kHz
    stereo → encode → 100 raw Opus packets at ≈ 96 kbit/s →
    decode → ≈ 191 760 / 192 000 PCM samples recovered, peak SNR
    ≈ 26.1 dB per channel after pre-skip alignment search. Opus
    is a perceptual codec (RFC 6716 §1), so a pure sine is
    intentionally adversarial; the SNR floor is set deliberately
    low at 6 dB — the goal is wired-pipeline verification, not a
    transparency measurement. Mono companion case exercises the
    1-channel mapping-family-0 path.
  - `src/lib.rs::register()` now installs `register_opus(ctx)` —
    factories registered under `CodecId::new("opus")` with
    `priority = 10`, `hardware_accelerated = true`, `lossy = true`,
    max 2 channels, max 48 kHz, claiming tags `FourCC('Opus')`
    (ISO/IEC 14496-12 Opus sample-entry FourCC) and Matroska
    `A_OPUS`.

- **Round 218: FLAC encoder.** Completes the symmetric AudioToolbox
  FLAC bridge — round 10 shipped decode via `kAudioFormatFLAC`
  (`'flac'`), round 218 adds the encoder side. Implementation
  `flac_audiotoolbox`, `priority = 10`, `hardware_accelerated = true`,
  `lossy = false`, max 8 channels, max 192 kHz.
  - New `src/flac_encoder.rs` with `FlacAtEncoder` implementing
    `oxideav_core::Encoder`. Input: interleaved S16 or S32 PCM in
    `AudioFrame::data[0]`. Output: one raw FLAC packet per encoded
    block (no container framing — the `fLaC` signature + metadata
    chain lives at the file or `dfLa`-in-container level, not on
    every packet).
  - **Empirical AT-side discovery #1** (round 218 probe across every
    `(PCM input width × source-data flag)` combination): AT's FLAC
    encoder slot ACCEPTS S16 → FLAC(16), S16 → FLAC(24), S32 →
    FLAC(16/20/24); REJECTS S32 → FLAC(32) with
    `kAudioConverterErr_FormatNotSupported` (`'fmt?'` = 1718449215).
    AT ships no 32-bit FLAC compression tier on macOS 14/15 slots.
    The encoder caps compressed bit depth at 24-bit for S32 input
    (lossless within the 24-bit range; S32 PCM bytes that fit there
    round-trip byte-identically through the 24-bit compressor).
  - **Empirical AT-side discovery #2**: AT's FLAC encoder calls back
    **twice** per `FillComplexBuffer` invocation (one frame-of-input
    request + one look-ahead pull) and the converter locks itself
    into a permanent end-of-stream state the moment the PCM input
    callback returns 0 bytes — that lock is unrecoverable; no amount
    of fresh PCM injected on later FCB invocations will unstick the
    encoder. The fix is a **persistent PCM feeder context** held in
    a `Box<PcmContext>` on the encoder, with a one-packet-of-slack
    discipline (the encoder only invokes FCB once the queue holds at
    least two packets' worth so the look-ahead pull is always
    satisfied). Same shape as the iLBC / AMR-NB / HE-AAC slack
    patterns the decoder side already used, applied to the
    compressed-output side.
  - **Magic-cookie emission**: encoder reads back
    `kAudioConverterCompressionMagicCookie` after configuring the
    converter — AT vends a fully formed `dfLa` ISOBMFF box (or a
    near-equivalent FLAC-in-MP4 metadata chunk) that a downstream
    muxer can paste verbatim into a `dfLa` sample-entry box. The
    cookie is published through `output_params.extradata`. If the
    property query fails (on an older macOS slot), the encoder
    synthesises a minimal `dfLa` cookie from the configured
    `(sample_rate, channels, output_bit_depth)` using the
    `flac::build_magic_cookie` builder the decoder side validated
    round 10.
  - **`PcmContext`**: queue-backed feeder with `read_pos` cursor +
    periodic compaction (drains the front half once the cursor
    crosses it). Held in a `Box` so the address AT receives via the
    callback user-data pointer stays stable across `Vec` mutations.
    The callback always serves the largest available subset of
    what AT asks for, only signalling EOF when truly empty and the
    flush path has been entered.
  - **Block size**: `DEFAULT_FRAME_LENGTH = 4096` (RFC 9639 §9.1.2
    Table 1 code 11), matching every fixture in
    `docs/audio/flac/fixtures/`.
  - **Per-packet PTS**: each emitted packet advances `pts` by
    `frame_length` samples; the encoder is intra-only (each FLAC
    frame is independently decodable).
  - **Tags claimed**: cascaded through the decoder's already-claimed
    `fourcc('flac')` + Matroska `A_FLAC` — the encoder shares the
    `CodecId("flac")` so the registry routes both directions.
  - New `tests/flac_roundtrip.rs` end-to-end test: 2 seconds of
    48 kHz / 16-bit stereo PCM (440 Hz sine + 12-bit deterministic
    LCG noise so the entropy coder does real work) →
    `FlacAtEncoder` → 23 raw FLAC packets + encoder-vended `dfLa`
    cookie → `FlacAtDecoder` → **188,416 / 192,000 i16 samples
    bit-exact at zero priming offset**. Searches a one-packet-wide
    priming window and demands ≥ 3 full packets (24,576 samples)
    survive bit-exact — proves the codec isn't just lossy with a
    lucky prefix.
  - 7 new unit tests (`make_encoder_succeeds_s16`,
    `make_encoder_succeeds_s32`,
    `make_encoder_rejects_unsupported_sample_format`,
    `make_encoder_rejects_too_many_channels`,
    `encoder_publishes_dfla_magic_cookie`,
    `output_params_echo_input_format`,
    `s32_cookie_declares_24bit_compressed_depth`).
  - `register_flac` in `src/lib.rs` now installs **both** decoder
    and encoder factories (round 10 + round 218 together complete
    the FLAC row); the `register_installs_flac_factories` test
    asserts both halves register.

- **Round 212: ALAC decoder S32 output path.** Before this round
  `AlacAtDecoder` always wired its output ASBD to `pcm_s16`, so
  24-bit and 32-bit ALAC tracks silently lost their low-order
  bits — defeating the codec's lossless contract on its native
  bit depths. The decoder now picks between S16 and S32 from the
  caller-supplied `CodecParameters::sample_format`:
  - `None` (default) and `Some(SampleFormat::S16)` keep the
    legacy S16 output path — every existing caller is byte-
    identical.
  - `Some(SampleFormat::S32)` routes through a new `pcm_s32` ASBD
    constructor (`AudioStreamBasicDescription::pcm_s32`, signed-
    integer + packed, distinct from `pcm_float32`) so the full
    32-bit sample word survives across decode. Against a 16- or
    20-bit cookie the request is harmless: AudioConverter sign-
    extends the source word into the high bytes.
  - `AlacAtDecoder::output_sample_format()` introspector lets
    downstream consumers learn which width they will see.
  - New `tests/alac_s32_roundtrip.rs` exercises the full encode →
    decode loop at S32 with a 440 Hz sine plus a deterministic
    24-bit low-bit noise term that sits entirely below the S16
    quantisation floor — a regression seal that fails if anyone
    reverts to `pcm_s16`-always. Result: 190,464 / 192,000 i32
    samples bit-exact end-to-end.
  - Unit tests `default_output_is_s16`,
    `explicit_s32_switches_output_width`,
    `s32_with_24bit_cookie_accepted`, `asbd_pcm_s32_geometry`,
    `asbd_pcm_s32_distinct_from_float32`.

- **Round 10: FLAC (Free Lossless Audio Codec, RFC 9639) decode** via
  `AudioConverterRef`. AudioToolbox exposes `kAudioFormatFLAC`
  (`'flac'`) on macOS 13+ as both a decompression and a compression
  target; this round installs the decode side. Tags claimed:
  `fourcc(b"flac")` (AT / ISOBMFF) and `matroska(A_FLAC)`.
  Implementation `flac_audiotoolbox`, `priority = 10`,
  `hardware_accelerated = true`, `lossy = false`, max 8 channels,
  max 192 kHz.
- New `src/flac.rs` module covering:
  - `StreamInfo` — 34-byte STREAMINFO body parser per RFC 9639 §8.1
    (16 + 16 + 24 + 24 + 20 + 3 + 5 + 36 + 128 bits) plus the inverse
    serialiser. Validates `sample_rate != 0` and
    `bits_per_sample >= 4`.
  - `FrameHeader` + `parse_frame_header` — RFC 9639 §9.1 walker
    covering the 15-bit `0b111111111111100` sync code, 1-bit
    blocking strategy, 4-bit block-size code (RFC 9639 §9.1.2 Table 1),
    4-bit sample-rate code (Table 2), 4-bit channel-assignment code
    (§9.1.3) and 3-bit bits-per-sample code (§9.1.4) with
    STREAMINFO fallback for code `0`.
  - `ChannelAssignment` enum — `Independent(n)`, `LeftSide`,
    `SideRight`, `MidSide` per §9.1.3. Reserved codes 11..=15
    surface as `None` from `from_code`.
  - `bit_depth_flag` — maps a FLAC bit depth (4..=32) to the
    `K_AF_APPLE_LOSSLESS_*` source-data flag value (same convention
    AT uses for FLAC per the `CoreAudioBaseTypes.h` header comment).
  - `build_magic_cookie` + `parse_magic_cookie` — produce / consume
    the **Xiph "FLAC in ISOBMFF" `dfLa` box** required by AT. The
    cookie layout was discovered empirically: bare STREAMINFO body,
    `fLaC + STREAMINFO`, and full `.flac` file prefixes all return
    `'!dat'` / `'!siz'` from
    `AudioConverterSetProperty(kAudioConverterDecompressionMagicCookie,
    …)`; only the `dfLa`-boxed form validates. The box layout is
    8-byte BoxHeader (`size`, `'dfLa'`) + 4-byte FullBox header
    (version=0, flags=0) + metadata block chain (≥ 1 STREAMINFO).
    Maximum cookie size AT accepts is **256 bytes**
    (`MAGIC_COOKIE_MAX_LEN`).
- `src/flac_decoder.rs` — `FlacAtDecoder` implementing
  `oxideav_core::Decoder`. Resolves the magic cookie via a three-path
  fallback: (1) full `dfLa` box in `CodecParameters::extradata`, (2)
  bare 34-byte STREAMINFO body in `extradata` → wrap in `dfLa`, (3)
  synthesise from `sample_rate / channels / sample_format`. Validates
  every incoming packet against the latched STREAMINFO (sample-rate /
  channel-count / bit-depth switches mid-stream return typed
  `Error::unsupported`; block-size changes are allowed since
  variable-blocksize FLAC streams are in scope per RFC 9639 §9.1.1).
  Persistent input-queue + one-packet-of-slack lookahead matching
  the iLBC / AMR-NB / AMR-WB / MP3 pattern so AT never sees
  "0 packets" mid-stream. `flush()` drains AT's internal lookahead;
  `reset()` clears the queue + calls `AudioConverterReset`.
- New `K_AUDIO_FORMAT_FLAC = 'flac'` constant + matching
  `AudioStreamBasicDescription::flac()` constructor in `sys.rs`
  (compressed source: `bytes_per_packet = 0` for variable-rate
  input, `format_flags = K_AF_APPLE_LOSSLESS_*` for source bit
  depth, `frames_per_packet` carries the max blocksize from
  STREAMINFO).
- Integration test `tests/flac_decode.rs` against the bundled
  `tests/fixtures/flac-mono-16bit-44100/` corpus (one 1-second
  mono / 16-bit / 44.1 kHz FLAC stream, 10 × 4608-sample
  fixed-blocksize frames with the last frame trimmed to fit the
  44 100-sample total):
  - `flac_decoder_decodes_mono_44100_fixture_lossless` — feeds the
    10 sync-split frames through the bridge, asserts the decoded
    stream is **exactly 44 100 i16 samples** (matching
    `STREAMINFO.total_samples`) and **byte-exact** to
    `expected.wav`. FLAC is lossless, so anything else means the
    bridge lost or distorted samples.
  - `flac_decoder_resets_state` — verifies `reset()` re-arms the
    decoder for new packets after previous-run state.
  - `flac_decoder_rejects_short_packet` — pins the bridge's
    surface-area check for packets too short to carry a header.
  - `flac_decoder_uses_extradata_cookie_verbatim` — confirms
    a caller-supplied cookie round-trips through
    `parse_magic_cookie` and is accepted by AT.
- New unit tests for `StreamInfo` roundtrip (44.1 kHz stereo 16-bit
  and 96 kHz mono 24-bit) + invariants (zero-sample-rate rejection,
  short-buffer rejection, fixed-vs-variable blocksize predicate),
  fixture STREAMINFO body parse (extracted from
  `docs/audio/flac/fixtures/stereo-16bit-44100-fixed/input.flac`),
  magic-cookie roundtrip + multi-block parse + wrong-box-type
  rejection + size-ceiling pin (≤ 256 bytes), `bit_depth_flag`
  canonical map, sample-rate / block-size / channel-assignment
  table lookups, frame-header parse (fixed + variable blocksize +
  STREAMINFO fallback for code `0`) + bad-sync rejection +
  reserved-channel-assignment rejection. Decoder construction
  succeeds in three scenarios (no cookie / full cookie / bare
  STREAMINFO body in extradata); mid-stream channel-count /
  bit-depth switches return typed errors; the FLAC ASBD geometry
  matches expected `(format_id, format_flags, channels_per_frame,
  frames_per_packet)` for both 16-bit stereo and 24-bit mono cases;
  the FourCC byte mapping `K_AUDIO_FORMAT_FLAC == 'flac'` is pinned.

- **Round 9: MP3 (MPEG-1 / MPEG-2 / MPEG-2.5 Audio Layer III) decode**
  via `AudioConverterRef`. AudioToolbox exposes `kAudioFormatMPEGLayer3`
  (`'.mp3'`) as a **decode-only** target (AT ships no MPEG-audio
  encoder), so the registry installs only a decoder factory under
  codec id `"mp3"` with `implementation = "mp3_audiotoolbox"`,
  `priority = 10`, `hardware_accelerated = true`, `lossy = true`.
  Tags claimed: `fourcc(b".mp3")` (AT / ISOBMFF), `mp4_object_type
  (0x6B)` (ISO/IEC 14496-1 MPEG-1 Audio), `matroska(A_MPEG/L3)`, and
  `wave_format(0x0055)` (Microsoft `WAVE_FORMAT_MPEGLAYER3`).
- New `src/mp3.rs` module exposing the `FrameHeader` parser for all
  MPEG audio frame headers: the `Version` enum (MPEG-1 / MPEG-2 LSF
  / MPEG-2.5 Fraunhofer extension), `Layer` enum (Layer I / II / III),
  `ChannelMode` enum (stereo / joint-stereo / dual-mono / mono),
  per-(version × layer) bitrate tables per ISO/IEC 11172-3 §2.4.2.3 +
  ISO/IEC 13818-3 §2.4.2.3, per-version sample-rate tables (MPEG-2.5
  rates 8 / 11.025 / 12 kHz from `docs/audio/mp3/MPEG-2.5-GAP.md`'s
  three primary clean sources — EBU TR 283 Popp/Brandenburg + USPTO
  RE44,897 + datavoyage header reference), and on-wire frame-length
  computation per ISO 11172-3 §2.4.3.1.
- `src/mp3_decoder.rs` — `Mp3AtDecoder` implementing
  `oxideav_core::Decoder`. Lazily constructs the AudioConverter from
  the first frame header (caller-supplied `CodecParameters` are
  advisory) so the bridge picks up the actual stream geometry rather
  than container-metadata reflections. Subsequent frames must match
  the latched (version × layer × sample-rate × channel-mode); bitrate
  changes are accepted (VBR is in scope). Mid-stream layer / version
  / sample-rate / channel-mode switches surface typed
  `Error::unsupported`. Persistent input-queue + one-packet-of-slack
  lookahead so AT never sees "0 packets" mid-stream (same shape as
  iLBC / AMR-NB / AMR-WB).
- `tests/mp3_decode.rs` — integration smoke against the bundled
  `tests/fixtures/mp3-layer3-stereo-44100-128kbps/` corpus (33 ×
  1152-sample MPEG-1 LIII 128 kbit/s stereo frames). Decodes to
  **76 032 interleaved i16 samples** and matches the staged
  reference WAV at **≈ 89.8 dB SNR** after priming alignment.
  Fixture is copied into `tests/fixtures/` so the standalone-repo
  GitHub Actions CI sees it without the umbrella's `docs/` submodule.
- `src/sys.rs` constants `K_AUDIO_FORMAT_MPEG_LAYER_{1,2,3}` (the
  `'.mp1'` / `'.mp2'` / `'.mp3'` FourCCs) and matching
  `AudioStreamBasicDescription::mpeg_layer{1,2,3}` constructors. Only
  Layer III is wired through the registry; the Layer I + II
  constructors are included for sys-level completeness.

## [0.0.3](https://github.com/OxideAV/oxideav-audiotoolbox/compare/v0.0.2...v0.0.3) - 2026-05-30

### Other

- round 7: AMR-NB narrow-band speech decode via AudioConverter
- round 6: iLBC narrow-band speech decode + encode via AudioConverter
- report AT's quantised bitrate via output_params.bit_rate
- round 5: AAC-LD + AAC-ELD encode + decode via AudioConverter
- round 4: HE-AAC v1 + v2 encode + decode via AudioConverter
- round 3: ALAC decode + encode via AudioConverter
- add .gitignore + drop committed Cargo.lock

### Added

- **Round 8: AMR-WB (Adaptive Multi-Rate Wideband, 3GPP TS 26.171 /
  TS 26.201 / RFC 4867) decode** via `AudioConverterRef`. AudioToolbox
  exposes `kAudioFormatAMR_WB` (`'sawb'`) as a **decode-only** target
  — there is no paired AT encoder (same asymmetry as AMR-NB), so the
  registry installs only a decoder factory under codec id `"amr_wb"`
  with `implementation = "amr_wb_audiotoolbox"`, `priority = 10`,
  `hardware_accelerated = true`, `lossy = true`. Tags claimed:
  `fourcc(b"sawb")` (3GPP / ISOBMFF) and `matroska(A_AMR/WB)`.
- New `src/amr_wb.rs` module exposing the `FrameType` enum (9 speech
  modes — MR660 6.60 kbit/s through MR2385 23.85 kbit/s — plus SID
  (FT=9) and NO_DATA (FT=15)), the per-mode storage-format byte-count
  table (1, 6, 17, 23, 32, 36, 40, 46, 50, 58, 60 bytes per packet
  per RFC 4867 §5.3), TOC-byte parsing + builder (`from_toc` /
  `make_toc`), per-mode bitrate lookup, and the
  `FRAMES_PER_PACKET = 320` constant (20 ms @ 16 kHz analysis window).
- `src/amr_wb_decoder.rs` — `AmrWbAtDecoder` implementing
  `oxideav_core::Decoder`. Validates each incoming packet against its
  TOC-derived size table before queueing, returning `Error::invalid`
  for reserved frame types (FT 10..=14 for AMR-WB — one fewer than
  AMR-NB because AMR-WB has nine speech modes) and for size
  mismatches. Persistent input-packet queue with one-packet-of-slack
  lookahead matching the iLBC / AMR-NB / HE-AAC pattern so AT never
  sees "0 packets" mid-stream. `flush()` drains the trailing PCM
  held back by the slack policy.
- New `K_AUDIO_FORMAT_AMR_WB = 'sawb'` constant + matching
  `AudioStreamBasicDescription::amr_wb()` constructor in `sys.rs`
  (16 kHz mono, `bytes_per_packet = 0` for variable-rate input,
  `frames_per_packet = 320` for the 20 ms AMR-WB analysis window).
- Integration test `tests/amr_wb_decode.rs`:
  - `amr_wb_decoder_accepts_all_frame_types` — feeds one
    syntactically-valid packet of every defined frame type
    (NO_DATA, SID, MR660..MR2385) and verifies PCM emerges
    (counted both mid-stream and after `flush`, since AT's
    AMR-WB decoder may vend PCM eagerly rather than holding all
    of it back for the slack-tail drain like AMR-NB does).
  - `amr_wb_decoder_handles_long_no_data_run` — drives 20
    consecutive NO_DATA packets to exercise the 1-byte
    input-descriptor path.
  - `amr_wb_decoder_rejects_size_mismatch` — pins the size-check
    surface area (e.g. MR2385 = 60 bytes; 59-byte packet refused).
  - `amr_wb_decoder_rejects_reserved_frame_type` — pins the FT
    validation surface area (FT 13 → `Error::invalid`).
  - `amr_wb_decoder_reset_clears_state` — verifies `reset()`
    re-arms the decoder for new packets after a previous run.
- New unit tests for `FrameType::from_toc` + `bytes_per_packet`
  + `bit_rate` + `make_toc` round-trip + FT-index uniqueness, the
  AMR-WB ASBD geometry, the `'sawb'` FourCC byte mapping, factory
  sample-rate / channel-count rejection, factory acceptance of
  every defined frame type, and registry presence (decoder only —
  encoder must NOT be registered since AT is decode-only).

- **Round 7: AMR-NB (Adaptive Multi-Rate Narrowband, 3GPP TS 26.071 /
  RFC 4867) decode** via `AudioConverterRef`. AudioToolbox exposes
  `kAudioFormatAMR` (`'samr'`) as a **decode-only** target — there is
  no paired AT encoder, so the registry installs only a decoder
  factory under codec id `"amr_nb"` with
  `implementation = "amr_nb_audiotoolbox"`, `priority = 10`,
  `hardware_accelerated = true`, `lossy = true`. Tags claimed:
  `fourcc(b"samr")` (3GPP / ISOBMFF) and `matroska(A_AMR/NB)`.
- New `src/amr.rs` module exposing the `FrameType` enum (8 speech
  modes — MR475 4.75 kbit/s through MR122 12.2 kbit/s — plus SID
  and NO_DATA), the per-mode storage-format byte-count table
  (1, 6, 13, 14, 16, 18, 20, 21, 27, 32 bytes per packet per RFC
  4867 §5.1), TOC-byte parsing + builder (`from_toc` / `make_toc`),
  per-mode bitrate lookup, and the `FRAMES_PER_PACKET = 160`
  constant (20 ms @ 8 kHz analysis window).
- `src/amr_decoder.rs` — `AmrNbAtDecoder` implementing
  `oxideav_core::Decoder`. Validates each incoming packet against
  its TOC-derived size table before queueing, returning
  `Error::invalid` for reserved frame types (FT 9..=14) and for
  size mismatches. Persistent input-packet queue with
  one-packet-of-slack lookahead (matching the iLBC / HE-AAC
  pattern) so AT never sees "0 packets" mid-stream. `flush()`
  drains the trailing PCM held back by the slack policy.
- New `K_AUDIO_FORMAT_AMR = 'samr'` constant + matching
  `AudioStreamBasicDescription::amr_nb()` constructor in `sys.rs`
  (8 kHz mono, `bytes_per_packet = 0` for variable-rate input,
  `frames_per_packet = 160` for the AMR analysis window).
- Integration test `tests/amr_nb_decode.rs`:
  - `amr_nb_decoder_accepts_all_frame_types` — feeds one
    syntactically-valid packet of every defined frame type
    (NO_DATA, SID, MR475..MR122) and verifies PCM emerges
    after `flush`.
  - `amr_nb_decoder_handles_long_no_data_run` — drives 20
    consecutive NO_DATA packets (the smallest variable-size
    packet path) to exercise the 1-byte input-descriptor path.
  - `amr_nb_decoder_rejects_size_mismatch` — pins the size-check
    surface area (e.g. MR122 = 32 bytes; 31-byte packet refused).
  - `amr_nb_decoder_rejects_reserved_frame_type` — pins the FT
    validation surface area (FT 12 → `Error::invalid`).
  - `amr_nb_decoder_reset_clears_state` — verifies `reset()`
    re-arms the decoder for new packets after a previous run.
- New unit tests for `FrameType::from_toc` + `bytes_per_packet`
  + `bit_rate` + `make_toc` round-trip, the AMR-NB ASBD geometry,
  the `'samr'` FourCC byte mapping, factory sample-rate /
  channel-count rejection, factory acceptance of every defined
  frame type, and registry presence (decoder only — encoder must
  NOT be registered since AT is decode-only).
- Empirical-finding note: AT's AMR-NB decoder vends PCM in
  **120-sample S16 mono blocks** (15 ms at 8 kHz) per
  `FillComplexBuffer` call rather than the 160-sample analysis-
  frame size — an internal AT chunking detail. The test suite
  asserts only the S16 mono byte-count invariant
  (`af.samples × 2 == af.data[0].len()`) and ≥ 1 frame produced
  rather than a fixed per-frame sample count.

- **Round 6: iLBC (Internet Low Bitrate Codec, RFC 3951) decode + encode**
  via `AudioConverterRef`. Adds the `kAudioFormatiLBC` (`'ilbc'`) format ID
  with a matching `AudioStreamBasicDescription::ilbc()` constructor.
  Fixed 8 kHz mono. Two block sizes selected via
  `CodecParameters::options.insert("mode", ...)`:
  - `"20"` (also `"20ms"` / `"ms20"`) — 160 PCM frames per packet,
    38 compressed bytes, 15.2 kbit/s net.
  - `"30"` (also `"30ms"` / `"ms30"`, default) — 240 PCM frames per
    packet, 50 compressed bytes, 13.33 kbit/s net.
- New `src/ilbc.rs` module with `IlbcMode` enum (parser + frame /
  byte geometry / round-trip tag helpers).
- `src/ilbc_decoder.rs` — `IlbcAtDecoder` implementing
  `oxideav_core::Decoder`. Persistent input-packet queue with
  one-packet-of-slack lookahead (mirroring the HE-AAC decoder
  pattern) — AT's iLBC analysis filter draws samples from one or
  two prior compressed packets per emitted PCM block, so a
  callback that returned 0 mid-stream would put the converter into
  a permanent EOS state. `flush()` drains the trailing PCM that
  the slack policy held back.
- `src/ilbc_encoder.rs` — `IlbcAtEncoder` implementing
  `oxideav_core::Encoder`. Internal staging buffer accumulates
  S16 PCM until a full `mode.frames_per_packet()` chunk is
  available, then emits one fixed-size iLBC packet per drain step.
  F32 PCM input is auto-converted to S16 via a length-vs-samples
  heuristic. `flush()` zero-pads any partial trailing PCM up to a
  full packet (standard CBR speech-codec EOS convention).
  Encoder publishes the active mode through
  `output_params.options["mode"]` and the net bitrate through
  `output_params.bit_rate` so downstream muxers can stamp it.
- `register()` now also installs iLBC decoder + encoder factories
  under codec id `"ilbc"` with `implementation = "ilbc_audiotoolbox"`,
  `priority = 10`, `hardware_accelerated = true`, `lossy = true`.
  Tags claimed: `fourcc(b"ilbc")`, `matroska(A_REAL/iLBC)`.
- Integration test `tests/ilbc_roundtrip.rs`:
  - `ilbc_30ms_roundtrip` — 2-second 1 kHz sine at 8 kHz mono via
    30 ms mode, peak SNR ≈ 10.7 dB (≥ 6 dB floor).
  - `ilbc_20ms_roundtrip` — same signal via 20 ms mode, peak SNR
    ≈ 7.8 dB.
  - `ilbc_packets_have_nonzero_payload` — ≥ 4 nonzero 50-byte
    packets within a 16k-frame feed.
- New unit tests for `IlbcMode` parse + geometry + round-trip
  tag, the iLBC ASBD geometry for both modes, the `'ilbc'`
  FourCC byte mapping, sample-rate / channel-count rejection,
  factory mode selection, and bitrate publishing.

### Fixed

- **AAC encoder: `output_params.bit_rate` now reports AT's actual
  delivered rate**, not the requested rate. AudioConverter quantises
  the requested bitrate onto a per-profile / per-sample-rate grid
  (e.g. AAC LC @ 48 kHz stereo accepts a limited set of values; off-grid
  requests are silently rounded to the nearest supported point). The
  encoder now queries `kAudioConverterEncodeBitRate` back after
  `SetProperty` and publishes the post-quantisation value through
  `output_params.bit_rate`. Falls back to the requested value if the
  get-property query is unavailable (older macOS, exotic ASBD). Two new
  regression tests pin the round-trip invariant — `bit_rate` published
  through `output_params` is non-zero, equals the request for the
  canonical 128 kbit/s LC operating point, and lands inside the
  plausible 32k-260k band for off-grid requests.

### Added

- **Round 5: AAC-LD + AAC-ELD encode + decode** via `AudioConverterRef`.
  Adds the `kAudioFormatMPEG4AAC_LD` (`'aacl'`, AOT 23) and
  `kAudioFormatMPEG4AAC_ELD` (`'aace'`, AOT 39) format IDs with matching
  `AudioStreamBasicDescription::mpeg4_aac_ld` / `mpeg4_aac_eld`
  constructors (**512 PCM frames per packet** — the shortened low-delay
  core, with no SBR upsample at the converter boundary, so the decoder's
  output sample rate equals the configured input rate). Two new
  `AacProfile` variants `Ld` / `Eld`, selected via
  `CodecParameters::options.get("profile")` = `"ld"` / `"eld"` (also
  `"LD"` / `"aac-ld"` / `"ELD"` / `"aac-eld"`).
- **Raw-framing generalisation**: a private `AacProfile::is_raw()` now
  drives the ADTS-vs-raw output decision. Only bare AAC LC is wrapped in
  a 7-byte ADTS header; every extended AOT (HE / HE-v2 / LD / ELD) is
  emitted as raw AAC bytes with the AOT carried out-of-band in the
  encoder-vended magic cookie. LD / ELD have no ADTS representation
  (ADTS profile bits encode only Main/LC/SSR/LTP), so the cookie path is
  mandatory: the decoder forwards it via
  `kAudioConverterDecompressionMagicCookie`, mirroring the HE path.
- Integration test `tests/ld_aac_roundtrip.rs` with three cases:
  - `aac_ld_roundtrip` — 2-second 1 kHz sine, 48 kHz stereo @
    128 kbit/s AAC-LD, encode → decode → per-channel SNR ≥ 20 dB
    (measured: ~29 dB; LD is near-transparent at full-band bitrates).
  - `aac_eld_roundtrip` — same signal @ 128 kbit/s AAC-ELD, per-channel
    SNR ≥ 12 dB (measured: ~26 dB).
  - `ld_packets_have_nonzero_payloads` — sanity check that the encoder
    emits ≥ 4 nonzero raw AAC packets and a non-empty magic cookie.
- New unit tests for the `Ld` / `Eld` profile parse + frames-per-packet
  (512), the `is_raw()` framing predicate, the two new ASBD geometries,
  and the `'aacl'` / `'aace'` FourCC byte mappings.

### Fixed

- The `register_tests` module in `lib.rs` is now gated on
  `#[cfg(all(test, feature = "registry"))]`. It references the
  `registry`-only `register()` entry point and `oxideav_core`, so a
  macOS `cargo test --no-default-features --lib` previously failed to
  compile. (CI's standalone job runs on Linux, where the whole
  `#![cfg(target_os = "macos")]` crate compiles away, so it never
  surfaced there — but a local macOS standalone test build did.)

- **Round 4: HE-AAC v1 + v2 encode + decode** via `AudioConverterRef`.
  Adds the `kAudioFormatMPEG4AAC_HE` (`'aach'`) and
  `kAudioFormatMPEG4AAC_HE_V2` (`'aacp'`) format IDs with matching
  `AudioStreamBasicDescription::mpeg4_aac_he` / `mpeg4_aac_he_v2`
  constructors (2048 PCM frames per packet, reflecting the SBR 2×
  upsample). Profile selection happens through
  `CodecParameters::options.get("profile")`: `"lc"` (default), `"he"`,
  `"he-v2"`. New `AacProfile` enum exported from `encoder` for
  programmatic use; HE-v2 mono is rejected at construction time per
  the spec (PS only meaningful for stereo).
- **Encoder magic cookie**: the AAC encoder now reads back its
  vended ISO/IEC 14496-1 `esds` descriptor via
  `kAudioConverterCompressionMagicCookie` and exposes it through
  `output_params.extradata` (42 bytes for HE / HE-v2, 2 bytes for
  bare LC AudioSpecificConfig). Required for downstream HE-AAC
  decode because the AOT extension descriptor is not present in
  any ADTS-like framing.
- **Decoder magic cookie**: the AAC decoder accepts the cookie via
  `CodecParameters::extradata` and forwards it through
  `kAudioConverterDecompressionMagicCookie`. Without the cookie,
  AT rejects HE / HE-v2 bitstreams with `kAudioCodecBadDataError`
  (`'bada'`).
- **HE / HE-v2 output framing**: HE-AAC packets are emitted as raw
  AAC bytes (no ADTS wrapper). ADTS only carries the base AAC LC
  profile bits, so wrapping HE-AAC payload in an ADTS header would
  be misleading and trigger decoder mis-identification. The
  encoder's `output_params.extradata` carries the AOT extension
  descriptor needed by downstream consumers. LC encode still emits
  the 7-byte ADTS header for back-compat with stock AAC decoders.
- **Encoder PCM staging buffer** and **decoder input packet queue**:
  both sides now keep enough lookahead beyond the immediate work
  unit to satisfy AT's HE-AAC SBR analysis without ever returning
  "0 packets" from the input callback mid-stream (which would put
  AT into a permanent EOS state and silently halt output). Tuned
  to 5 packets of PCM (encoder) / 4 packets of compressed input
  (decoder) — empirically matches AT's HE-AAC lookahead.
- `encoder.rs` now publishes `output_params` with `sample_rate`,
  `channels`, `bit_rate`, `extradata`, and `options["profile"]`
  echoed back so a downstream consumer (e.g. an MP4 muxer or a
  paired decoder) can reconstruct full configuration from the
  encoder's vended values alone.
- Integration test `tests/he_aac_roundtrip.rs` with three cases:
  - `he_aac_v1_roundtrip` — 2-second 1 kHz sine, 48 kHz stereo @
    64 kbit/s HE-AAC, encode → decode → per-channel SNR ≥ 8 dB
    (measured: ~11 dB; SBR's patch-and-scale reconstruction caps
    the recoverable phase fidelity below transparency).
  - `he_aac_v2_roundtrip` — same signal @ 32 kbit/s HE-AAC v2 with
    Parametric Stereo, per-channel SNR ≥ 6 dB (measured: ~10 dB).
  - `he_aac_packets_have_nonzero_payloads` — sanity check that the
    encoder emits ≥ 4 nonzero raw AAC packets within a 16k-frame
    feed and that the cookie starts with `0x03` (ES_DescrTag).

### Changed

- AAC LC encoder semantics: `send_frame` now stages PCM in an
  internal buffer and drains in `frames_per_packet`-sized chunks
  rather than encoding one frame per send. Behaviour is identical
  for callers that already submitted 1024-frame chunks; callers
  passing differently-sized chunks now see the expected
  packetisation. Required for HE / HE-v2 (which want 2048-frame
  packets) but applied uniformly so the contract is consistent.
- AAC LC decoder semantics: `send_packet` now queues the packet
  and lets the converter pull from the queue across multiple
  `FillComplexBuffer` calls, instead of feeding one packet per
  call. PCM frames are accumulated into a queue drained by
  `receive_frame`. Backwards compatible — `receive_frame` returns
  the same frame sequence.
- `encoder::flush()` drains AT's internal SBR lookahead with up to
  16 zero-input `FillComplexBuffer` calls so the last few packets
  of every stream are recovered. Without this drain, HE-AAC
  streams lose ~4 trailing packets.
- `decoder::flush()` mirrors the encoder: drains the AT decoder's
  internal buffer with extra zero-input pulls.

### Round 3 — ALAC (Apple Lossless) decode + encode

- ALAC decode + encode via `AudioConverterRef` with magic-cookie wiring.
- `alac.rs` — 24-byte `ALACSpecificConfig` magic-cookie builder + parser
  (big-endian wire format per Apple's `ALACMagicCookieDescription` doc
  snapshot in `docs/audio/alac/`). Default tuning constants (frame_length
  4096, pb=40, mb=10, kb=14, max_run=255) match Apple's documented
  recommendation. `bit_depth_flag()` maps PCM bit depth to the
  AudioFormatFlags value the AT ASBD expects.
- `alac_decoder.rs` — `AlacAtDecoder` implementing `oxideav_core::Decoder`.
  Reads the magic cookie from `CodecParameters::extradata` and forwards
  it to AT via `kAudioConverterDecompressionMagicCookie`. If no cookie
  is supplied, synthesises a minimal-but-valid one from the explicit
  `sample_rate / channels / sample_format` parameters. Output is
  interleaved S16 PCM, one `AudioFrame` per ALAC packet.
- `alac_encoder.rs` — `AlacAtEncoder` implementing `oxideav_core::Encoder`.
  Internal staging buffer accumulates PCM until a full 4096-frame
  packet is available, then emits one ALAC packet per drain step. The
  encoder-vended magic cookie is read back via
  `kAudioConverterCompressionMagicCookie` and exposed through
  `output_params.extradata` so downstream muxers (mov / m4a / caf) can
  emit a working ALAC track.
- `sys.rs` — added FourCC + property constants for ALAC:
  `kAudioFormatAppleLossless`, the four
  `kAppleLosslessFormatFlag_*BitSourceData` values, and the
  `kAudioConverterDecompression/CompressionMagicCookie` property keys.
  New `AudioStreamBasicDescription::apple_lossless()` constructor.
- `register()` now also installs ALAC decoder + encoder factories under
  codec id `"alac"` with implementation `alac_audiotoolbox`,
  priority 10, `hardware_accelerated = true`, `lossy = false`. Tags
  claimed: `fourcc(b"alac")`, `matroska(A_ALAC)`.
- Integration test `tests/alac_roundtrip.rs`: 2-second 48 kHz / 16-bit
  stereo sine+LCG-noise mix, encode → decode through the AT bridge,
  asserts **190,464 / 192,000 samples bit-exact** with zero priming
  silence — proves the codec is truly lossless end-to-end.

## [0.0.2](https://github.com/OxideAV/oxideav-audiotoolbox/compare/v0.0.1...v0.0.2) - 2026-05-06

### Other

- drop dead `linkme` dep
- clarify load-vs-init fallback + document require_hardware opt-out
- apply cargo fmt (rustfmt CI compliance)
- round 2: real AAC LC decode + encode via AudioConverterRef
- auto-register via oxideav_core::register! macro (linkme distributed slice)

### Added

- **Round 2: real AAC LC decode + encode factories** via `AudioConverterRef`
  loaded at runtime through `libloading`.
- `sys.rs` — full AudioConverter FFI bindings:
  `AudioConverterNew`, `AudioConverterDispose`, `AudioConverterReset`,
  `AudioConverterSetProperty`, `AudioConverterGetProperty`,
  `AudioConverterGetPropertyInfo`, `AudioConverterFillComplexBuffer`.
  Property keys `kAudioConverterEncodeBitRate` and
  `kAudioConverterPropertyMaximumOutputPacketSize` inlined as constants.
  `AudioStreamBasicDescription`, `AudioBufferList1`,
  `AudioStreamPacketDescription` and `AudioBuffer` structs defined.
  `AudioStreamBasicDescription::pcm_float32`, `pcm_s16`, `mpeg4_aac`
  convenience constructors added.
- `adts.rs` — lightweight ADTS framing helpers:
  `parse()` (strip header on decode side),
  `build_header()` (synthesise 7-byte ADTS header on encode side),
  `sample_rate_index()` (Hz → sampling-frequency-index table lookup).
- `decoder.rs` — `AacAtDecoder` implementing `oxideav_core::Decoder`.
  Accepts ADTS-framed AAC input packets; output is interleaved F32 PCM.
  Converter configured lazily on first packet from ADTS header.
  `make_decoder()` factory function registered with the codec registry.
- `encoder.rs` — `AacAtEncoder` implementing `oxideav_core::Encoder`.
  Accepts interleaved F32 (or S16) PCM `AudioFrame`s; output is
  ADTS-framed AAC. Target bitrate set via
  `kAudioConverterEncodeBitRate`. ADTS header synthesised from the
  configured sample-rate index + channel configuration.
  `make_encoder()` factory registered with the codec registry.
- `register()` installs both factories under codec id `"aac"` with:
  `implementation = "aac_audiotoolbox"`, `priority = 10`,
  `hardware_accelerated = true`, `lossy = true`.
  Tags claimed: `wFormatTag(0x00FF/0x706D/0x4143/0xA106)`,
  `mp4_oti(0x40)`, `matroska(A_AAC)`.
  Runtime dlopen failure → log + no-op (pure-Rust fallback kicks in).
- `decoder` and `encoder` modules now gated behind `#[cfg(feature = "registry")]`
  so `--no-default-features` builds remain free of `oxideav-core`.
- Integration test `tests/roundtrip.rs`: 2-second 440 Hz sine wave,
  48 kHz stereo, encoded at 128 kbit/s and decoded; per-channel SNR
  measured with sliding-window delay search — both channels ≥ 25 dB
  (measured: **36.7 dB**).

### Changed (round 1 → round 2)

- `register()` now installs real decoder and encoder factories instead
  of being a no-op.
- Initial scaffolding: `#![cfg(target_os = "macos")]` crate that
  dlopens AudioToolbox + CoreFoundation via `libloading` on first
  use. Smoke test verifies symbol resolution for `AudioConverterNew`
  + `CFRetain`.
- Unified `register(&mut RuntimeContext)` entry point matching the
  framework convention.
- Standalone-friendly: default-on `registry` feature gates the
  `oxideav-core` dep + the `register` fn.
- README documents the priority-10 placement (hardware preferred over
  pure-Rust) and the `--no-hwaccel` CLI opt-out.
