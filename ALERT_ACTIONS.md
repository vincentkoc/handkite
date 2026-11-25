# Guarded native alert actions

Status: implementation candidate following the approved design. Synthetic
qualification and any live result are recorded in VALIDATION.md. Notification
permission support is deliberately narrower than generic system-alert support.

## Ordinary native taps over moving app content

A native tap binds a unique native ID, exact attributes and rectangle, display
mapping, and a pixel digest of the target rectangle plus the existing
one-raster-pixel margin. Admission reads fresh native identity and attributes
between two screenshots. Both guarded crops must equal the published digest;
changes outside the crop do not decide this proof. A matching whole image alone
cannot skip native revalidation. App, lock, execution owner, runner/session,
revision, single-use handle and original deadline checks remain mandatory.

Moving pixels inside the target crop still refuse, including translucent app
surfaces whose background shows through. This does not change coordinate, text
or alert admission or native tap effect verification. A freshness refusal is
not assumed to be a pixel failure. Qualification and latency are recorded
separately in VALIDATION.md.

### Why native element staleness does not prove attribute freshness

The unchanged-frame alternative was tested against main's actual admission
path. A synthetic native button keeps its ID, geometry and identical pixels
while its accessibility `value` or `selected` changes. The frame shortcut
admits it; fresh scoped admission refuses it. The reproducible fixture and
results are in [VALIDATION.md](VALIDATION.md).

Pinned WDA's [element-ID click](https://github.com/vincentkoc/handkite-wda/blob/1262085ed9a13ef4e0d0e222cf0a918ecfe4b565/packages/webdriveragent/WebDriverAgentLib/Commands/FBElementCommands.m#L241-L253)
resolves the [cached object](https://github.com/vincentkoc/handkite-wda/blob/1262085ed9a13ef4e0d0e222cf0a918ecfe4b565/packages/webdriveragent/WebDriverAgentLib/Routing/FBElementCache.m#L59-L87)
with a [snapshot existence check](https://github.com/vincentkoc/handkite-wda/blob/1262085ed9a13ef4e0d0e222cf0a918ecfe4b565/packages/webdriveragent/WebDriverAgentLib/Categories/XCUIElement%2BFBUtilities.m#L90-L108),
then taps that object. The request carries no expected old attributes. A
still-live object whose semantics changed therefore need not produce a
stale-element error. App/lock/owner checks and a two-second identical-frame
window constrain other risks but do not prove these attributes unchanged. This is a synthetic
contract counterexample, not a live phone incident. The owner accepted this gap
and retained fresh attributes on every tap; the remaining gate is latency.

### Overlap processing without moving the acquisition bracket

The first screenshot response still completes before native identity/attribute
reads start, and those reads complete before the last screenshot starts.
Joining an image request directly with the native query could place the query
outside the two images; unchanged pixels would then hide same-ID semantic
changes outside the narrowed bracket. The acquisition order therefore remains mandatory.

The first frame's decoding, mapping and crop computation overlap the native,
app and optional focus reads. Final screenshot acquisition overlaps validation
of the returned native rows. Final image decoding/crop comparison overlap the
post-image display read. Each group uses `join!`, not `try_join!`, so ordinary
failures await every started sibling before returning to settings restoration.
All results remain required before dispatch. No worker or detached task is
introduced; original outer timeouts and settings-restoration behavior remain.
The final hittability/app checks and fresh native-lock proof keep their order.

## Problem and scope

A system alert can own the foreground as SpringBoard while another app remains
visible underneath it. Current ordinary native targets reject SpringBoard,
Alert, Sheet and SecureTextField contexts. A navigation-only observation cannot
authorize a coordinate tap or supply a native alert target. Reusing that revision
for alert input therefore fails safely, but leaves the agent unable to respond.

Add a separate, tap-only alert capability. Keep ordinary target, text, gesture,
Home and PIN admission unchanged. An alert observation must never populate
ordinary action authority or imply that SpringBoard is displaying Home.

The first supported class is the standard English notification permission
alert, with its complete expected title/body and Allow / Don't Allow button set.
It must be owned by positively proven `com.apple.springboard`, with a positive
PID tied to the alert's native hierarchy. A foreground bundle read or an XML
application label alone is insufficient. An app-owned lookalike refuses, even
when its text and buttons match exactly; other alert owners require separate
qualification.
A complete, versioned classifier fixture must cover each accepted variation
before implementation admits it. Other locales, custom prompts, sheets and
other permission classes remain unsupported until separately qualified. Button
labels alone do not identify an alert class or the app requesting permission.

Unknown dialogs, passcode/PIN entry, device or computer trust, XCTest
Enable UI Automation, biometric authentication, account sign-in, password,
verification-code, purchase/payment and terms dialogs stay unavailable to this
path. Never use the PIN broker or generic tap as an automatic fallback.

## Caller contract

Tools:

- `handkite_observe_alert`: read one supported alert and return alert-scoped
  authority only. An image is omitted by default. Unsupported or ambiguous
  context returns a fixed refusal code and no actionable revision or targets.
- `handkite_tap_alert`: require the exact observation revision, opaque alert ID
  and opaque native button target ID. There is no coordinate, label, arbitrary
  text, accept/dismiss shorthand or default button parameter.

The tap requires the existing authenticated execution owner and a live,
explicitly acquired control lease. Publishing a target does not grant permission
to choose it. The agent must have the user's authorization for that particular
permission decision; a request to observe, browse or benchmark is not permission
to answer a prompt. No automatic allow/deny policy is introduced.

Example request, using synthetic opaque handles:

```json
{
  "expected_revision": 42,
  "alert_id": "a_11111111111111111111111111111111",
  "target_id": "ab_22222222222222222222222222222222"
}
```

The response identifies `scope: alert`, the supported alert class, the full
alert title/body and the caller-selectable button labels and rectangles. Button
handles have a namespace distinct from ordinary target IDs. Owner PID,
native element IDs, fingerprints and credential material remain internal.
The requesting app is reported only if directly proven; foreground SpringBoard
alone cannot establish it. No permission-state change is inferred from labels.

## Evidence and state

Maintain dedicated alert state in the controller and adapter. Do not widen
`public_state`, `safe_context`, ordinary `Targets`, the Home hint, or the existing
PIN parser. An alert revision must fail through every ordinary input endpoint,
and an ordinary or navigation revision must fail through the alert endpoint.

Bind the private observation to:

- pinned USB device, WDA runner/session, display selection and execution owner;
- native foreground owner bundle and PID before and after capture;
- fresh native Unlocked and WDA unlocked proofs before and after capture;
- two complete context snapshots bracketing the native hierarchy proof, with
  matching alert structure, versioned class and content digest;
- the complete native button set, descendant ancestry, IDs and attributes;
- full alert and button rectangles, screen scale and screenshot mapping;
- the original capture window, revision and monotonic expiry.

The first implementation uses a complete bounded context, not a filtered query
that could hide a second alert or authentication field. Exactly one visible,
enabled supported alert must exist. Every published button must be a unique
native Button descendant, visible, enabled and hittable, with a nonempty label
and a rectangle fully inside both the alert and display. Unknown attributes,
truncation, duplicate identity, nested/multiple alerts or inconsistent native
and XML inventories refuse the entire alert observation.

Scan the complete context for contradictory secure fields, keypad/passcode
markers and authentication dialogs, including hidden nodes. Positive class
recognition and complete structure are required; absence of a few English
security keywords is insufficient. When the native APIs cannot establish the
required class or complete context, fail closed rather than infer support.

### Why the alert proof is native, not pixel-based

iOS renders notification alerts as translucent material. A moving app can
change pixels inside an unchanged alert, including behind its text and buttons.
Exact region equality therefore rejects a stable native object. A glyph mask
would depend on contrast thresholds and compositing; it would not establish
OS ownership or distinguish a visually identical replacement object.

For this narrow native-button path, the authority is the SpringBoard PID,
native application/alert/button identities and ancestry, complete context,
exact content and geometry, and current visibility/hittability. Two complete
XML snapshots bracket the native hierarchy inventory; both must pass the
global authentication exclusions and have identical modal fingerprints and
viewport. This catches a title/body change or hidden authentication field
introduced while native descendants are queried. Input also requires exact
identity agreement with the original observation. A different native alert
with the same words still refuses.

The screenshot is decoded and its display mapping checked, but its pixels do
not grant or invalidate alert authority. This is specific to native element-ID
clicks; ordinary coordinate/pixel admission is unchanged. Neither the former
pixel comparison nor this non-atomic API sequence can eliminate a change after
the last proof. The bound native-ID click retains native stale-element checks,
the original expiry and single-use delivery semantics; it never substitutes
coordinates or another alert. OCR and inferred visibility cannot create
authority.

Both XML reads use the existing cheap outline source, which omits `visible`
but does not filter nodes or content. XML supplies structure, content and the
global authentication scan. Native descendant queries independently prove
that the alert, both buttons and both text nodes are displayed; buttons also
require enabled and hittable. Text enabled state must agree with XML but may
be false, since text is not an input target. The strict legacy modal parser
still requires explicit XML visibility.

The initial native lock read overlaps the read-only capture. Both operations,
including response-profile restoration, drain before any authority is
published. A second fresh native lock proof remains required afterward. The
five-second capture budget is unchanged. Content-free timing records identify
the active predicate when a failure or timeout interrupts capture.

Dismissal requires two safe, complete contexts without an alert, scoped native
absence and an explicit no-such-alert response. Moving background pixels are
not evidence that the alert remains present, or that it was dismissed.

Fresh native lock proof must retain the existing deadline and transport guards.
`AdapterNativeBefore` is not assumed to mean an alert is present; preserve its
actual failure stage. Unknown/locked state, stale native evidence, owner/PID
change, disconnected transport, session change or restoration uncertainty
returns no authority. An image of an unlocked-looking app is not a lock proof.

## One-shot transaction

1. Observe the supported alert with the evidence above. Clear old ordinary and
   alert handles before attempting the capture; failure leaves none active.
2. On tap, validate lease owner, device, alert revision, alert ID, target ID and
   original expiry. Any intervening accepted observation, input, release or
   session/display change invalidates the old alert authority.
3. Settle prior input verification. Immediately recapture the complete alert and
   its native descendants, with fresh lock/app/owner proofs. Require the same
   owner, alert native ID, class, title/body, complete button set, chosen native
   button identity, attributes, geometry and complete modal fingerprint. Do not refresh the
   original observation's expiry to make a stale request usable.
4. Restore and verify any temporary WDA response settings before input. If
   restoration is uncertain, poison further input through the existing path.
5. Consume all alert handles before entering dispatch. Send one native
   element-ID click with the bounded socket-write deadline. Never call WDA's
   generic alert accept/dismiss endpoint and never fall back to coordinates.
6. Preserve the transport receipt independently of verification. A rejected or
   uncertain send is never replayed. Only failures proven before entering the
   dispatch future may report `delivery: not_sent`; all other missing delivery
   evidence reports `unknown`.
7. Make a bounded, fresh post-action observation. A safely parsed dismissal or
   SpringBoard/app-return transition, or the unchanged original alert still
   present, permits one second read under the same five-second deadline. A
   same-bundle PID change, replacement alert, authentication context, malformed
   evidence, lost ownership/session, lock failure or uncertain settings
   restoration does not permit a retry. Neither read can resend input.
   Report `Changed` only when
   complete context and fresh lock/owner proof establish dismissal of the exact
   original alert, with no replacement/unsupported modal. Cache eviction or a
   missing old element-ID lookup alone does not prove dismissal. Owner may
   return from SpringBoard to the underlying app, but the new context still
   needs independent proof.
8. Otherwise return acknowledged/unknown delivery with `Unconfirmed`. Renew the
   lease only after verified success. Return no next input authority; require a
   new ordinary or alert observation. The first version is synchronous and
   does not reuse the deferred ordinary-target effect queue.

`Changed` proves the alert was dismissed. It does not assert that permission
was granted, denied or persisted. A replacement alert is a separate decision,
not an instruction to continue automatically.

## Implementation boundary

| Layer | Change |
| --- | --- |
| Core controller | New typed alert observation, tap request, preparation and result; separate revision/handle storage; existing lease/device/window rules. |
| WDA adapter | New alert capture/classifier, native-ID binding, final revalidation, one-shot click and synchronous dismissal proof. |
| Wire | Separate observe-alert and tap-alert messages; fixed stage-specific errors; no generic-error fallback. |
| MCP | Separate tools under existing observation/interaction capability gates; explicit alert scope and empty ordinary target inventory. Clear stale Home hints. |
| Agent runbook | Explain explicit user decision authorization, unsupported/authentication refusals, and no retry after uncertain delivery. |

The legacy `NativeModalTap` coordinate exception is a separate existing route.
Before the new path can be called guard-preserving, add a regression proving
that unsupported/authentication alerts cannot bypass the same exclusions through
that route. Do not silently broaden it to make the new implementation pass.
Any necessary tightening must be explicit in the implementation diff and tests.

Current code references:

- [Ordinary target state gate](crates/handkite-core/src/controller/targets.rs).
- [Native target capture and dispatch](crates/handkite-host/src/wda_adapter/targets.rs).
- [Native context exclusions](crates/handkite-host/src/wda_adapter/targets_fast.rs).
- [Existing modal admission](crates/handkite-host/src/wda_adapter.rs).
- [MCP navigation fallback](crates/handkite-host/src/mcp.rs).

## Required qualification

Use synthetic fixtures only in source and PR evidence. Cover both a system alert
over Home and one over an app; stable alert with an animated background; moved,
resized, hidden or disabled buttons; changed title/body/button set; replaced
alert with the same labels; duplicate IDs; multiple/nested alerts; missing
hittability; display/scale/PID/session changes; locked/unknown native state;
restoration failure; expired lease/revision; cross-scope handles; pre-send
refusal; lost acknowledgement; post-read failure; replacement alert; and cache
eviction that must not prove dismissal. Hidden secure fields, PIN/keypad,
XCTest/trust, sign-in/code, payment and unknown localized dialogs must refuse.
Include an app-owned notification lookalike with identical text and buttons;
it must refuse with zero input. Every failed admission test must assert zero
input dispatches. Delivery-unknown
tests must assert one dispatch and no replay.

Before claiming device support, qualify the exact signed candidate through the
normal upgrade/rollback route. Use one currently displayed, explicitly
authorized permission decision; do not reset permissions in Settings to make
a fixture. Verify stale-target refusal before the single authorized click and
verify dismissal afterwards. If the required native class/identity evidence is
unavailable, keep the candidate unqualified and retain the existing release.

Photos screenshots, thumbnails, library contents and labels stay out of PRs,
`VALIDATION.md`, fixtures and Git history. Public evidence contains only timings,
counts, fixed outcome codes and synthetic examples. Internal transient frames
must be deleted after classification; any optional returned alert image must
be cropped to the proven alert region and must not include the underlying app.
No performance target justifies skipping these proofs.
