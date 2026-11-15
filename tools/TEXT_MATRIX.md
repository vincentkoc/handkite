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

## The tree shows at most 512 UTF-16 units of a value

WDA builds the page source for an application from XCTest's fast ("standard")
snapshot, which cuts every `value` at 512 UTF-16 units. The cut is made by
XCTest, not by Handkite, and nothing marks it. A direct attribute read
(`/element/:id/attribute/value`) uses WDA's slower custom snapshot and returns
the whole value; the owner's read-only direct check confirmed this. WDA's
`enforceCustomSnapshots` setting would make the page source use custom
snapshots too, at a capture cost that has not been measured here.

An agent reading a long field through the tree therefore sees a silently
truncated value. The matrix cannot read an element value directly: the guarded
MCP path has no such tool. It works inside the cap instead:

- Cases are split into **notes**. One note holds cases only while its whole
  value stays under 500 units, so every comparison sees the full value. The
  500-unit timing cases each get a note of their own.
- A case that alone exceeds the cap (`deadline_ascii_2000`) is reported as
  `tree_capped`: the visible 512 units must match the expected prefix exactly,
  and the row carries `direct_read_required`, the expected unit count and a
  SHA-256 of the expected value for the owner's read-only direct check.

## What the runner does

For each case, once:

1. Full observation. It requires an unlocked phone, Notes in front, exactly one
   visible text view and a visible keyboard. At the start of a note the text
   view must be **empty**, so it cannot run in a note that already has content.
2. One `handkite_text` call (default `selection: "current"`, the qualified
   typing path; `--selection strict` omits it).
3. The value read back must start with the value seen before the call. The
   remainder is what was typed.
4. Comparison, rule `exact`: the typed remainder equals `expected_readback`
   scalar for scalar. Text that differs only by canonical equivalence is
   reported as `nfc_equal`, never as `pass`; no field is assumed to normalise.
   Rule `unchanged` (refusal cases): the tool must reject the arguments and the
   value must be identical before and after.
5. An Enter key puts the next case on a new line. Nothing is deleted.

Every input after the first in a note, including the Enter between cases and
each step of `multiline_enter_key`, starts from its **own fresh observation**,
and the editor must still hold exactly the value proved so far. An action's own
capture can precede the end of keyboard animation; using its revision for the
next input is what produced `ViewChanged` in the first live run. That refusal
is the product working as designed and is not relaxed.

Outcomes: `pass`, `nfc_equal`, `mismatch`, `tree_capped`, `refused`,
`stop_pre_input`, `partial`, `uncertain`, `not_run`.

- `stop_pre_input`: a fixed pre-input refusal such as `AdapterNativeBefore` or
  `ViewChanged`. Nothing reached the device; the case is not sent again and the
  run continues.
- `partial`: a multi-step case stopped before all its inputs were sent. The
  delivered part is never resumed or repeated; the note's remaining cases are
  `not_run`.
- `uncertain`: delivery is unknown. The runner makes one read-only observation,
  records whether the editor changed, and **stops**; later cases are `not_run`.

After any stop, start a new empty note and pass the remaining ids with
`--cases`. Do not include a `partial` or `uncertain` case again unless a fix
has been installed.

The report contains ids, outcomes, sizes, timings and, for a mismatch, the index
and code point of the first differing scalar. It contains no typed text, tree,
screenshot or device identifier. Keep reports outside the repository.

## Phone setup

1. Settings, General, Keyboard, Keyboards: have English, Chinese Simplified
   Pinyin, Japanese Romaji, Japanese Kana and Emoji installed. Traditional
   Zhuyin and any other keyboard are optional.
2. Leave Smart Punctuation and the "." shortcut as they are and note their
   state in the result.
3. Auto-Lock Never, phone unlocked.
4. For each note: create a **new empty note** in Notes (not the benchmark
   fixture note), tap the body so the keyboard is up, and make the wanted
   keyboard active. Do not type anything.

Switching the keyboard is setup and is done **by hand on the phone** (globe
key). `handkite_click_button` refuses the globe key on this phone: its reported
rectangle ends one point below the screen, and button admission requires the
whole rectangle on screen. That guard is unchanged; see the review note in
`VALIDATION.md`.

## Run

`list` shows the notes for one keyboard and build without contacting anything:

```sh
python3 tools/text_matrix.py list --keyboard english --build installed
```

Then one command per note, each in a new empty note with that keyboard active:

```sh
python3 tools/text_matrix.py run --keyboard english --build installed --note 1 \
  --output /private/path/text-english-1.json -- \
  /path/to/handkite-mcp --service --capabilities observation,navigation,interaction
```

Keyboards: `english`, `pinyin`, `zhuyin`, `japanese_romaji`, `japanese_kana`,
`emoji`, `other` (add `--keyboard-label NAME`). Use `--build installed` for a
build without this branch and `--build branch` for one that contains it.
`--note` may be omitted when the selection fits one note. With `--cases`, notes
are numbered over the selected cases.

`--build installed` leaves out `refuse_line_separator` and
`refuse_function_key`: a build without the shared validation rule would send
U+2028 and U+F700 to the device, and XCTest reads U+F700 as the up arrow key.

## Timing

Timing cases (`time_ascii_1`, `_20`, `_100`, `_500`, `time_mixed_units_500`)
ask for the optimized **image-only** response, so `request_ms` is the text
action without a paired tree capture. The read-back is a separate observation,
reported as `verify_ms` and excluded from `request_ms`. `--timing-capture
paired` restores the earlier behaviour for a matched comparison with the first
run. Non-timing cases always use paired capture.

The report's `typing_rate` is a least-squares fit of `request_ms` against
UTF-16 units over the passing timing cases of that run:

- `ms_per_utf16_unit`: dispatch cost per unit, the typing rate;
- `fixed_ms_per_action`: everything else in a text action.

The long cases live in other notes, so fit across the reports of notes that
contain timing cases, or use the service spans. With one sample per size the
fit is indicative only; overhead noise can exceed the slope at high rates. If
service timing is enabled, record `text_prepare`, `text_context_transaction`,
`text_context_query`, `text_final_focus`, `text_readback` and the derived
`input_request` for those operations: `input_request` is the dispatch time.
The runner does not enable or read service timing.

`deadline_ascii_2000` is opt-in (`--include-deadline-case`) and gets the last
note. At 60 per second it should exceed the 30 second request deadline and end
`uncertain`; at a higher rate it should end `tree_capped`.

### Typing frequency (branch build only)

A build containing this branch reads `HANDKITE_TYPING_FREQUENCY` once at
service start: a plain decimal from 60 through 600, sent as `frequency` with
each text request. Unset or invalid keeps the runner default of 60. Add
`--typing-frequency N` to label the report; it does not set the rate.

No rate is the default yet. Before one is promoted:

1. `time_ascii_500` and `time_mixed_units_500` pass by exact tree comparison,
   each in its own note, at the default and at the candidate rate.
2. A repeat cohort of at least twenty typing trials at the candidate rate with
   the benchmark runner, zero mismatches.
3. The short Unicode classes pass at the candidate rate on English.
4. The same long cases under each installed input method, since a higher key
   rate is most likely to disturb composition there.

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
  `mixed_english_cjk_emoji`). Not yet tested.
- `ascii_rewrite_bait` **passed** on the English keyboard with Smart Punctuation and the period shortcut on: XCTest
  text synthesis was not rewritten. Its result under other keyboards is open.
- Decomposed forms passed exactly on English.

A cell that stays `mismatch` after fixes is a documented limit. No pasteboard
fallback is indicated by the results so far, and a pasteboard would not lift
the tree cap.
