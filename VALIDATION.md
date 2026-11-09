# Device validation

This is a summary of device validation for the published source snapshot. It is
point-in-time evidence from one test device, not a live health check and not a
guarantee for other devices, iOS releases or host configurations. The detailed
dated run logs, per-run report digests and device evidence are retained privately
and are not part of this repository.

## Method

- **Setup.** One dedicated test iPhone connected over USB to the Mac that runs
  the controller, with a Handkite-owned XCTest runner built from the pinned
  WebDriverAgent source in [`tools/wda/SOURCE.md`](tools/wda/SOURCE.md). The
  device is selected by an exact pinned identity; there is no fallback to
  another device or controller.
- **Harness.** [`tools/benchmark_mcp.py`](tools/benchmark_mcp.py) drives the
  installed service through MCP. Each case needs at least twenty successful
  trials, records per-stage p50/p95, payload sizes and exact client/service
  builds, rejects incomplete traces and stops on unconfirmed input. Paired
  before/after reports gate regressions at the reference p95 plus 5% and 20 ms.
- **Reference.** The [numeric regression reference](tools/benchmarks/reference.json)
  contains only metrics and source/plan hashes.
- **Text matrix.** [`tools/text_matrix.py`](tools/text_matrix.py) runs the
  Unicode and timing matrix described in [the runbook](tools/TEXT_MATRIX.md),
  with an independent readback of each stored value.
- **Custody rules.** Uncertain or failed input is never replayed. Each campaign
  ends by restoring the canonical build, confirming native lock state
  read-only, and verifying that protected files and PIN stop markers are
  unchanged. PIN entry is not exposed through MCP and runs only through the
  one-shot unlock command or the supervised broker.

## Speed reference

### Qualified local USB results

These twenty-trial cohorts used the local USB device host and the executables
selected for the current canonical release. Typing inserts one character into
an existing Notes fixture and includes guarded effect verification. Swipe uses
the corrected native-raster movement check. Both cohorts completed with zero
infrastructure stops, input failures or cleanup failures.

| Full act and verify | n | p50 ms | p95 ms | Target p50 / p95 | Result |
| --- | ---: | ---: | ---: | --- | --- |
| Type | 20 | 1,669.400 | 1,911.124 | <2,000 / <2,000 | Pass |
| Swipe | 20 | 1,883.503 | 4,401.228 | <2,000 / <2,000 | p95 above target |

Three subsequent swipe capture experiments produced no qualifying improvement.
The original runner and host were restored. Full accessibility capture remains
the expensive path on cache misses; these results do not prove a device or WDA
latency floor. The retained native-lock and PIN guards were unchanged.

### Earlier staged references

Twenty completed samples per cell. "Image-only" actions skip the returned XML
capture; full action admission and post-action verification still apply. These
older cohorts are historical references, not timings of the current local host.

| Client request, p50 / p95 (ms) | Accepted baseline | Reference build | p50 budget | Result |
| --- | ---: | ---: | ---: | --- |
| Tap | 5,921.107 / 6,063.047 | 1,532.836 / 2,437.532 | <2,000 | Pass; p95 above 2 s |
| Type | 7,772.821 / 8,566.162 | 4,397.860 / 4,958.275 | <2,000 | Above budget |
| Swipe | 16,922.518 / 17,585.312 | 3,070.576 / 3,412.512 | <2,000 | Above budget |
| Launch | Not completed | 2,125.990 / 2,179.621 | <3,000 | Pass |
| Image-only observe | Not completed | 456.508 / 462.651 | <500 | Pass |
| Observe with tree | Not completed | 880.932 / 896.902 | <1,500 | Pass |

The baseline used paired action responses; the reference uses explicit
image-only capture. This is a measured policy change, not a like-for-like
claim about full semantic verification.

## Earlier focused typing workloads

| Typing workload | n | p50 / p95 ms | Target | Result |
| --- | ---: | ---: | ---: | --- |
| Non-empty editor, one character | 20 | 2,524.367 / 2,598.423 | <2,000 | Functional pass, above target |
| New empty note, one character | 20 | 2,429.326 / 2,734.095 | <2,000 | Functional pass, above target |
| New empty note, 100 characters at 240/s | 20 | 2,816.191 / 3,013.898 | <2,000 | Functional pass, above target |

All cohorts had zero input failures or infrastructure stops, with exact
readback. The 100-character cohort is a different workload, not a matched
comparison with the one-character cohorts. A matched non-empty comparison after
the empty-editor fix exceeded the p95 regression allowance by about 10 ms;
functional qualification does not waive that result.

## Text injection

- Text is sent as one string through WebDriverAgent's `/wda/keys`, which
  performs one XCTest text synthesis. It is not on-screen key taps or USB HID.
- The default rate is 240 characters per second (`HANDKITE_TYPING_FREQUENCY`
  accepts 60 through 600).
- The English keyboard (30 short Unicode classes including composed/decomposed
  forms, emoji sequences and multiline Enter) matched scalar for scalar at 240/s.
  500-unit ASCII and mixed-unit cases matched exactly by independent readback.
- Whole-request fits at 240/s were roughly 6 ms per UTF-16 unit plus about
  2.4 s fixed cost. These are noisy single-sample fits, not pure dispatch rates
  or a device floor.
- An earlier Pinyin run completed 20/20 exact 100-character trials at 240/s
  (2,808.377 / 3,065.213 ms p50/p95). Its separate matrix had 18 exact passes
  and one pre-input stop. It used an older runner and is not a current-build
  performance comparison. No composition or candidate-picking interference
  was observed in delivered cases.
- The latest Japanese Kana campaign delivered 20 exact 100-character values
  over 22 attempted trials, with two preparation stops. Pooled text-action
  p50/p95 were 2,000.058 / 2,199.281 ms; action plus independent full-value
  readback was 4,358.980 / 4,677.514 ms. This is descriptive pooled evidence,
  not an uninterrupted twenty-trial qualification.
- On the Emoji keyboard, eight requests passed with exact cumulative readback:
  single emoji, variation selectors, keycaps, ZWJ family, skin tone, ZWJ with
  skin tone, flags and subdivision flag. No requests were replayed. Text-action
  p50/p95 were 2,815.982 / 3,408.186 ms; separate full-value readback was
  4,156.162 / 4,359.373 ms. This small functional cohort is not a matched speed
  comparison. An earlier pre-input failure exposed the extra Emoji search
  field; the strict prepared-text fix was qualified on a new empty note.
  Japanese Romaji keyboard reliability remains unqualified.

## Locked profile admission

One read-only invocation of the production profile reader accepted an available
profile in 44 ms, with native Locked state confirmed before and after. Regression
fixtures cover unavailable, expired and oversized profile inventories and fatal
profile diagnostics in broker output. The live result proves readability on the
tested device, not negative expiry behavior. Subsequent signed installations
passed the production startup gate, reached readiness and completed read-only
observation and the Emoji cohort.

An earlier startup exhausted its owner-authorization window and was contained.
The owner later authorized a new supervised start and completed the XCTest
prompt. No agent PIN entry, Settings changes or automatic replay occurred.

## Qualification status

| Surface | Status | Limit |
| --- | --- | --- |
| Device identity and ownership | Pinned device, exclusive service lock and native runner cleanup checked | No implicit fallback to another device or controller |
| Benign interaction | Tap, type, swipe, launch, Home and App Store search/install/launch flows passed | No messaging, signup or account changes performed |
| PIN and native lock | One-shot PIN unlock through the supervised broker confirmed on the test device | PIN never passes through MCP; uncertain attempts are not retried |
| Service recovery | Service termination and owned-runner loss each produced one healthy successor | Physical reconnect and Mac reboot not qualified |
| Release selection | Rollback to a retained release and return passed | Old runtime was not bootstrapped |
| Signing expiry | Build verifier checks profile and certificate validity, identity and device coverage; startup refuses unavailable or expired profile evidence | Locked profile readability, installed readiness and live Emoji qualification passed. Negative expiry behavior is fixture-tested; no renewal or future-validity claim. |
| Signing renewal | Not qualified | Rebuilding with the same profile does not extend validity |
| Agent integration | Direct MCP qualified | Authenticated agent acceptance not qualified |
| Anisette deployment | Local asset/proxy fixtures only | Domain, hosted TLS and fresh signing flow not qualified |

## Provenance

The WebDriverAgent source pin and embedded upstream excerpts are recorded in
[`tools/wda/SOURCE.md`](tools/wda/SOURCE.md). The GrandSlam provisioning
endpoints used by the Anisette client are the exact pair listed in the README.
