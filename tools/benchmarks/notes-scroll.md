# Notes-only performance fixture

`notes-scroll.json` uses one new, disposable Notes note. Its body is
`"Alpha\n" + "".join(chr(0x4e00 + n) for n in range(480)) + "\n"`: a wrapped
paragraph of distinct characters and 487 UTF-16 units,
below the 512-unit accessibility value limit. Setup needs two text calls and
two Enter actions, because text injection does not accept embedded newlines.
Existing notes must not be used or changed.

Before a cohort, verify the exact body, no active composition, and the top of
the note visible. The first-line tap coordinates need independent visual
confirmation on the selected device. The prepared iPhone layout places the first
line at y=146–166; this fixture uses y=156 for its first-line taps. The paragraph must be scrollable with the
keyboard visible, and both swipe endpoints must lie inside the observed editor,
above the keyboard. Setup is outside the measured samples.

Typing inserts and removes one `X` in the first line. Swipes require an
acknowledged action, changed image, unchanged exact text, visible keyboard and
the same unlocked app. The inverse swipe and Done are cleanup; any failure stops the
cohort without replay. Both swipe directions also need coherent content movement
inside a fixed image region above the keyboard. A changed caret or system bar
alone does not qualify. Image bytes stay in memory until this check and are not
included in reports. The native image converter uses private temporary files.
Distinct characters avoid ambiguous matches between repeated text rows. The
120-point gesture stays within the verifier's overlap window. This fixture
does not prove an exact scroll distance.
Cleanup checks the acknowledged swipe and immediate non-scroll invariants,
then takes one later observation for the unchanged motion proof against the
original pre-swipe image. It never repeats the input or retries a failed proof.
After the inverse swipe, settling checks the paragraph below the first line
(`x=15, y=180, width=360, height=260`). The first-line caret can blink while the
paragraph is stationary. Every RGB channel in this region must still match;
the full tree, exact text and device state checks remain unchanged. This does
not change the larger region used to prove either swipe's content movement.
For a failed cleanup proof, the explicit failure-image export can retain the
two private images for offline diagnosis; it is disabled by default.
The two swipe cleanup settles use `settle_region` with viewport 393x852. The
first uses ROI `[15,115,360,325]`; the second uses `[15,180,360,260]` as above.
They still require consecutive exact device, state and
full-tree equality, then the unchanged body/app/lock/keyboard expectations.
Only their image comparison uses every decoded RGB channel in that ROI,
without sampling or tolerance. They retain the latest real observation and
revision, the existing read limit, and the normal input guards. Other settles
still compare the whole image.

Three later read-only frames had identical RGB pixels throughout the first ROI
(0 changed out of 1,052,025 pixels in every pair). Full-frame differences were
600-612 pixels, all above the ROI. The thin amber stroke was compatible with
a blinking caret under the toolbar; its identity was not proven. This evidence justifies
excluding that area from these cleanup comparisons; it does not establish the
pixel differences in the earlier failed settle frames or prove quiescence
outside the ROI. Invalid geometry or image decoding refuses the comparison.
Launch checks use Clock followed by Notes. No Settings action is present.

Compare only runs using this identical plan, capture mode, case order and
sample count. Earlier Settings-fixture timings are historical context, not a
matched baseline for this fixture.
