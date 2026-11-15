# Direct text injection matrix

`tools/text_matrix.py` proves that `handkite_text` inserts final text exactly, in
every script, under each active keyboard. The agent never types through an input
method: the keyboard is switched by the operator as setup, then each case is one
guarded `handkite_text` call.

- Spec: `tools/benchmarks/text-matrix.json` (ASCII, synthetic text only). Each
  case has an id, class, input, required keyboards, expected read-back,
  comparison rule, the builds it may run on, the expected result per keyboard,
  and whether it must be re-run on a build containing this branch.
- The text classes are the same strings as the Rust unit-test matrix in
  `crates/handkite-host/src/text_matrix.rs`; `tools/test_text_matrix.py` fails
  if they drift.

## What the runner does

For each case, once:

1. Full observation. It requires an unlocked phone, Notes in front, exactly one
   visible text view and a visible keyboard. At the start the text view must be
   **empty**, so it cannot run in a note that already has content.
2. One `handkite_text` call (default `selection: "current"`, the qualified
   typing path; `--selection strict` omits it).
3. The value read back from that call's own observation must start with the
   value seen before the call. The remainder is what was typed.
4. Comparison, rule `exact`: the typed remainder equals `expected_readback`
   scalar for scalar. Text that differs only by canonical equivalence is
   reported as `nfc_equal`, never as `pass`; no field is assumed to normalise.
   Rule `unchanged` (refusal cases): the tool must reject the arguments and the
   value must be identical before and after.
5. An Enter key puts the next case on a new line. Nothing is deleted.

Outcomes: `pass`, `nfc_equal`, `mismatch`, `refused`, `stop_pre_input`,
`uncertain`, `not_run`.

- `stop_pre_input`: a fixed pre-input refusal such as `AdapterNativeBefore`.
  Nothing reached the device; the case is not sent again and the run continues.
- `uncertain`: delivery is unknown. The runner makes one read-only observation,
  records whether the editor changed, and **stops**; later cases are `not_run`.
  Start a fresh empty note and pass the remaining ids with `--cases`. Do not
  include the uncertain case again unless a fix has been installed.

The report contains ids, outcomes, sizes, timings and, for a mismatch, the index
and code point of the first differing scalar. It contains no typed text, tree,
screenshot or device identifier. Keep reports outside the repository.

## Phone setup

1. Settings, General, Keyboard, Keyboards: have English (US), Chinese
   Simplified Pinyin, Japanese Romaji, Japanese Kana and Emoji installed.
   Traditional Zhuyin and any other keyboard are optional.
2. Leave Smart Punctuation and the "." shortcut as they are for the first
   pass, so their effect is measured. Note their state in the result.
3. Auto-Lock Never, phone unlocked.
4. For each keyboard: create a **new empty note** in Notes (not the benchmark
   fixture note), tap the body so the keyboard is up, and switch to that
   keyboard with the globe key. Do not type anything.

## Run

One command per keyboard, with that keyboard active in a new empty note:

```sh
python3 tools/text_matrix.py run --keyboard english --build installed \
  --output /private/path/text-english.json -- \
  /path/to/handkite-mcp --service --capabilities observation,navigation,interaction
```

Keyboards: `english`, `pinyin`, `zhuyin`, `japanese_romaji`, `japanese_kana`,
`emoji`, `other` (add `--keyboard-label NAME`). Use `--build installed` for a
build without this branch and `--build branch` for one that contains it.
`list` in place of `run` prints the selected cases and expectations without
contacting anything.

`--build installed` leaves out `refuse_line_separator` and
`refuse_function_key`: a build without the shared validation rule would send
U+2028 and U+F700 to the device, and XCTest reads U+F700 as the up arrow key.

## Timing

The `english` run includes `time_ascii_1`, `_20`, `_100`, `_500` and
`time_mixed_500`. The report's `typing_rate` is a least-squares fit of client
request time against UTF-16 units over the passing timing cases:

- `ms_per_utf16_unit`: dispatch cost per unit, the typing rate;
- `fixed_ms_per_action`: everything else in a text action (context, focused
  preparation, native query, final focus check, read-back, capture).

If service timing is already enabled, also record the spans `text_prepare`,
`text_context_transaction`, `text_context_query`, `text_final_focus`,
`text_readback` and the derived `input_request` for those operations. The
runner does not enable or read service timing.

`deadline_ascii_2000` is opt-in (`--include-deadline-case`) and runs last. At
the runner's default 60 per second it should exceed the 30 second request
deadline and end `uncertain`. Run it once, in its own note.

### Typing frequency (branch build only)

A build containing this branch reads `HANDKITE_TYPING_FREQUENCY` once at
service start: a plain decimal from 60 through 600, sent as `frequency` with
each text request. Unset or invalid keeps the runner default. For each
candidate rate (120, 240, 480), with the service started with that value:

```sh
python3 tools/text_matrix.py run --keyboard english --build branch \
  --typing-frequency 240 --output /private/path/text-english-240.json -- \
  /path/to/handkite-mcp --service --capabilities observation,navigation,interaction
```

`--typing-frequency` only labels the report. Accept the highest rate at which
every `english` case that passed at the default still passes; a dropped,
reordered or duplicated character shows as `mismatch`.

## Which build

| Cells | Installed build without this branch | Build containing this branch |
| --- | --- | --- |
| All text classes and `multiline_enter_key`, every keyboard | Run | Re-run only the cases marked `rerun_on_branch` on `english` |
| `refuse_newline`, `refuse_tab`, `refuse_over_limit` | Run | Re-run |
| `refuse_line_separator`, `refuse_function_key` | Do not run | Run |
| `time_*` at the default rate | Run (baseline) | Re-run (matched) |
| `time_*` with `HANDKITE_TYPING_FREQUENCY` | Not available | Run per rate |
| `deadline_ascii_2000` | Optional, once | Optional, once per rate |

## Expected failures

- Latin letters while Pinyin, Zhuyin or Romaji is active: XCTest sends key
  events, so the input method should hold them as uncommitted composition or
  convert them (`ascii_sentence`, `accented_latin_*`, `bidi_mixed`,
  `mixed_english_cjk_emoji`).
- `ascii_rewrite_bait` on every keyboard: the runner turns off auto-correction
  and prediction but not smart quotes, smart dashes or the period shortcut.
- Decomposed forms (`*_decomposed`, `korean_conjoining_jamo`,
  `japanese_halfwidth_katakana`) are marked `unknown`: they may read back
  composed.

A cell that stays `mismatch` after fixes is a documented limit. The fallbacks,
none built: keep an ASCII-capable keyboard active while the agent types; turn
off Smart Punctuation on the test phone; or a pasteboard insert through WDA's
`/wda/setPasteboard`, which overwrites the phone clipboard and needs extra
guarded edit-menu steps.
