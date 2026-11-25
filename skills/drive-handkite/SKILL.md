---
name: drive-handkite
description: Drive an authorized local iPhone task through Handkite native MCP tools or its persistent Python client, using compact elements and deltas with image fallback.
---

# Drive Handkite from an agent

Use the installed `handkite` MCP server. If unavailable, import `Session` from
`tools/handkitectl.py` in this repository. It uses the selected local service;
do not discover a second device host, start a second runner, or rebuild just to
find a client. Service startup and PIN-broker recovery belong to the device owner.

Acquire control explicitly, then observe. Default to
`{"tree_format":"elements","image":"omit"}`. The Python client supplies
these options and carries forward the last received revision. For raw MCP,
pass that revision as `expected_revision` and `response.since_revision` on the
next action. Every successful paired action can return the next observation;
do not issue an extra observe when that response is sufficient.

## Real-time loop (runbook)

1. `handkite_acquire_control`, then `handkite_observe {"targets":true,
   "tree_format":"elements","image":"omit"}` once for the full inventory.
2. Pick one `target_id` and call `handkite_tap_element` with the target revision.
   Expect `effect:"pending"`; never resend it.
3. Immediately observe with `targets:true` and `since_revision` equal to the last
   target revision. Apply `targets_delta`; read the previous tap's verdict from
   `effect_updates`. Repeat from step 2.
4. If the wanted control is missing (an animating screen omits unstable rows),
   observe again. If it is still missing, OCR rows (`source:"ocr"`) or an
   `input_source:"ocr"` row can be tapped the same way; failing that, request
   `image:"always"` and escalate instead of guessing coordinates.
5. Any refusal reports `delivery:"not_sent"`: observe and decide again.
   Uncertain delivery: stop.

Split the work: the strong model plans the task, private-data choices and
recovery; a fast model executes steps 2–4 from compact elements only. One
executor holds the lease; never run two against one phone. Measured loop cost
and qualification status are in `VALIDATION.md`.

## Human-like pacing (opt-in)

Some apps flag input that is too regular or fast. Pacing is off by default and
adds no latency. The device owner can launch the MCP with `--pacing natural` or
`--pacing cautious` (optionally `--pacing-seed N`), and any coordinate action
accepts `"pacing":{"policy":"natural"}`, `"cautious"` or `"off"` for that call.
Paced calls wait a log-normal gap since the previous action and respect an
actions-per-minute cap. Coordinate taps move only inside the smallest element
frame from the same revision; the host varies press timing, curves swipes and
types short ASCII text in bursts. Every action still needs its own fresh
`expected_revision` and passes the usual admission; a wait can make a revision
stale, so observe again on `StaleRevision`. `PacingUnsupported`
(`delivery:"not_sent"`) means the host predates pacing: nothing was sent, so
retry that call with `"pacing":{"policy":"off"}`. Element tools get only the gap
and cap. Pacing is no stealth guarantee and never a reason to replay input; see
`VALIDATION.md` for what is live-verified.

For Home or app switching, call `handkite_status` for a cheap navigation revision,
then `handkite_home` or `handkite_launch_app`. These do not require a coherent XML
snapshot. A response with `revision_scope:"navigation"` admits only these two
actions; it cannot authorize a tap or text. Native lock, app, device, ownership
and freshness checks still apply. After switching apps, observe targets before
interacting. A drifting paired capture can fall back to locally stable XML/OCR
targets, or an image with navigation-only authority. Do not treat that image as a
full-tree revision.

Safari's plain outline (`targets:false`) is a presentation tree with unknown
visibility and a navigation-only revision. It includes the hierarchy without
the expensive visibility pass. Use `targets:true` for native element actions;
do not use an outline's coordinates as input authority. Full-tree requests keep
their existing capture path.

For consecutive target observations, request `tree_format:"elements"` and
`since_revision` equal to the last target observation received. The Python
`observe_targets()` method keeps this baseline across a pending tap; it remains
separate from the actionable revision. A matching native baseline refreshes
changed screen rectangles without fetching a full hierarchy. A lost baseline
uses a full capture and a new inventory.

Treat `elements_delta` as a patch against exactly `base_revision`: remove IDs,
replace changed IDs, add new IDs, then apply `order`. Apply `targets_delta` to
the native handle inventory the same way; `Session.targets` maintains that
inventory in the Python client. A full `elements` or `targets` list replaces
its previous list. Never apply a delta to another session or revision.

The compact `id=eN` fields are presentation IDs. For native input, opt in with
`handkite_observe {"targets":true}` or `phone.observe_targets()`. This returns
`target_only:true`, a target-only revision, and `targets` with opaque `target_id`,
role (`kind`), label, logical rectangle, bounded value and truncation flag.
An active editor may return `text_only:true`: use its text tool, never tap it.
Its admission checks exact native identity, value, safe context and focus; caret
animation does not remove that text capability. Other targets retain pixel checks.
Targets default to compact XML and no image. Request `image:"always"` for visual
recovery. Zero stable targets force an image and `target_actionable:false`;
inspect that fallback before deciding what to do. Never pass an `eN` ID or convert its rectangle into a native handle.

Use `handkite_tap_element {"target_id":"t_...","expected_revision":N}` or
`handkite_text_element` with the same fields plus `value`. Text requires the
same native element to already be focused; it never clicks to focus. These tools
return one structured result with next targets and a compact observation/delta
when available. Owned native taps acknowledge delivery with `effect:"pending"`,
a fresh image and a provisional `target_delta`. This is not verified success.
The consumed handle is listed in `consumed`; no confirmed removal, fresh target
or actionable revision is issued. Never replay that acknowledged input.

The next target observation carries the tap verdict in `effect_updates`,
correlated by token and target ID: `pending`, `changed` or `unconfirmed`. These
are historical verdicts, not new input authority. A control that disappeared
stays `pending` until the exact stale-ID proof runs: while the controller is
idle, before a target overlapping the acted control, or before a full capture.
A non-overlapping next action supersedes it as `unconfirmed`; that is not a
failure and never a reason to replay. Only verified effects renew the lease, so
re-acquire on `InvalidLease` during long navigation runs.

Other target replies can include a fresh image and acted-element `target_delta`
without a full tree. This is a partial result: replace the handle
inventory with `targets`; all previous handles are consumed or invalidated by the
new revision. An empty inventory needs an explicit target observe for the next
step. `target_delta.scope:"acted_element"` must never be applied as a full-screen
element diff. Full trees remain available on explicit observe.
`response:{"image":"always"}` requests a screenshot on other target paths; default
is omit. Handles expire after 15 seconds and are consumed before dispatch. A target
revision cannot admit the legacy coordinate/text tools. Native targets include
Button, MenuItem, TextField, TextView, SearchField, Link and PickerWheel, only
when uniquely matched, visible, enabled and hittable. Visible text rows can
also have OCR-backed tap targets as described below.

Native tap admission reuses the last delivered binding and checks its region
against one fresh screenshot, plus fresh native identity, attributes, app, lock
and ownership. It does not fetch a full XML tree. Native targets require matching
region pixels, identity and full internal attributes. OCR targets require the same unique text and position in both
capture images and in a fresh pre-input image, with unchanged screen mapping.
Outside-target animation may vary; an unstable target is omitted. Missing or
ambiguous mapping, unsafe context or unknown settings restoration refuses input.
A successful click alone is not success: a target-specific observed change is
required, and unrelated animation never renews the control lease. On uncertain
input stop without replay. Missing next targets require a fresh observation.

Region refreshes declare `tree_scope:"native_regions"`. These are flat native
presentation data, not a full accessibility hierarchy. Sources `native` and
`native_cached` distinguish refreshed and retained rows; neither substitutes
for the mandatory pre-input identity and region checks. Retained semantics keep
their original expiry. Newly native-derived fields are tap-only; text needs a
separately admitted full-context field. Do not infer typing capability from a
visible TextView. Native enumeration waits for pending effect proof because it
can evict WDA's element cache. Image-only polling remains read-only.

Rows otherwise identify `source:"xml"` or `source:"ocr"`. OCR rows represent visible
text that has no covering native element. A unique exact text match in an
existing XML row instead carries `input_source:"ocr"` and `target_id`; it keeps
`source:"xml"` and its original label and role. Both forms have stable opaque
target IDs and support `handkite_tap_element` only. The tap uses the recognized
glyph bounds, not the native row center. They never authorize text injection.
The host rechecks the original text and bounds before an OCR tap; agents must
not translate presentation coordinates into input themselves.

If neither source supplies the desired element, request an image and stop the
element-only sequence. Region stability does not prove native semantic
freshness. Treat OCR as independent image evidence; consult `VALIDATION.md` for
the live qualification status.

Use one serial executor. A fast model can choose the next admitted action from
compact elements; keep task planning, private-data decisions and recovery with
the stronger coordinating model. Escalate ambiguous labels or state instead of
guessing a coordinate. Never run two model executors against one phone.

Target preparation and revalidation refusals report `delivery:"not_sent"`.
On a pre-input refusal, observe and make a new decision. On uncertain delivery,
stop: never resend. The CLI stops at the first tool error; the Python client
invalidates the revision. Neither reacquires control automatically. Release
control explicitly when finished; closing the client also asks MCP to clean up.

Phone contents are private. Do not log raw replies, XML, images or typed values
to Git. The client returns data in memory; its CLI prints replies to stdout, so
use the Python interface when handling private forms. For latency logs, record
only action kind, elapsed milliseconds, fixed failure category and revision.

Minimal read-only Python session, run from the repository:

```python
import sys
sys.path.insert(0, "tools")
from handkitectl import Session

with Session() as phone:
    phone.call("handkite_acquire_control")
    observation = phone.observe_targets()
    # Inspect observation in memory, choose one authorized action, then inspect
    # that action's returned observation. No automatic retries.
    phone.call("handkite_release_control")
```

`python3 tools/handkitectl.py` also accepts one JSON object per line:
`{"tool":"handkite_observe","arguments":{}}`. Keep the process open across
steps. See the README's MCP section for native client registration.
