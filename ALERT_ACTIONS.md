# Guarded native alert actions

Status: proposed design; no implementation, installation or live alert action
is included in this PR. The proposed API names below are not available tools.

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

Proposed tools:

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

Example proposed request, using synthetic opaque handles:

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
- one complete alert tree, its native ID, versioned class and content digest;
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

Underlying app animation does not invalidate an otherwise unchanged alert.
Require stable alert-region pixels and exact modal semantics inside the fresh
capture bracket. Changes outside that region do not supply or invalidate alert
identity. This exception belongs only to the new alert proof, never to ordinary
whole-screen admission. OCR, guessed visibility and coordinate fallback cannot
create alert authority.

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
   button identity, attributes, geometry and region pixels. Do not refresh the
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
7. Make one bounded, fresh post-action observation. Report `Changed` only when
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
