# Focused Notes typing fixture

`notes-typing.json` measures only current-selection typing. Run `--cases type`
with 20 trials and zero warmups; the existing typing budget remains 2000 ms.

Create a new, task-owned disposable Notes note for each cohort. Populate its
body once during setup with `"Alpha " + "".join(chr(0x4e00 + n) for n in
range(480)) + "Z"` (487 UTF-16 units, no newlines) in one text injection.
Finish setup with the keyboard visible, no active composition, and the caret
at the end left by that insertion. Verify
the complete TextView value, Notes foreground, unlocked state and visible
keyboard before starting. Setup is outside measured samples. A full-value
observation does not prove caret position; the measured append must still match
exactly.

Each trial observes that exact base body, inserts one `X` with
`selection: "current"`, and requires exactly `base + "X"`. Cleanup first observes
that same full value before sending one Backspace with current selection, then
requires the exact base body in the action result and a final observation.
The keyboard stays up and focus stays in the editor. There are no coordinate
taps, Done actions, app launches or caret resets between trials.

Stop at the first failed expectation or uncertain delivery. Do not retry input,
accept `X` at another position, or issue cleanup against an unverified value.
Do not reuse a failed note or an `AlphaX` note as this fixture.

Both Background and Interactive need fresh 20-trial typing cohorts with this
identical plan and capture mode. The earlier `a71` coordinate-based typing
baseline is not a matched comparison. Keep those results and the original
`notes-scroll.json` unchanged.
