# Handkite

Handkite is a local-first control service for one explicitly selected
iPhone. The Mac connected to the phone over USB runs the controller. A local
stdio MCP bridge exposes screenshots, accessibility trees and guarded actions.

The design specification is kept outside this repository.

Source identifiers, messages, comments and documentation use English. Unicode
and IME test data remain in their original scripts. Run
`python3 tools/check_source_text.py` to check staged and working tracked text
without displaying file contents. The Han-character check exempts only the named
Notes fixtures' exact expected-text value paths; keys and metadata remain checked.
Runtime app labels and accessibility text may use any language.

The qualified route uses a local USB connection and a separately configured PIN
broker; MCP never receives the PIN. Signing profiles expire and must be checked
before installing a runner. See the [runner guide](tools/wda/README.md).
Local typing met the two-second p50/p95 target. Swipe remains above its p95
target; see [measured results and limits](VALIDATION.md).

## Local setup

Handkite controls one explicitly selected iPhone over USB from the Mac it is
attached to. You need:

- an Apple Silicon Mac with Apple's command-line tools and the Rust toolchain
  pinned in `rust-toolchain.toml`;
- a test iPhone with Developer Mode enabled and **Settings > Developer > Enable
  UI Automation** turned on, trusted by the Mac;
- your own Apple development team to sign the XCTest runner (see the
  [runner build guide](tools/wda/README.md)).

Device PIN entry is never exposed through the MCP bridge. Optional PIN unlock
runs as a separate one-shot local command, or through the supervised broker
`tools/supervised_device.py`, guarded by stop markers; it is off until you
create its state directory and supply a PIN source. See
[PIN unlock](RELEASE.md#pin-unlock) and the [PIN stop gate](#pin-stop-gate).

### Startup troubleshooting

1. If the runner fails during automation initialization, check **Settings >
   Developer > Enable UI Automation** on the phone. Toggling it off and on and
   completing the XCTest authorization prompt has cleared failures reported
   with LocalAuthentication code `-4`. Unlocking the phone alone does not
   authorize XCTest.
2. Keep the exact runner failure stage. A timeout or code `-4` alone does not
   prove that the setting was off, that pairing broke, or that an account needs
   repair.
3. Retry startup once with exclusive device ownership. On another failure,
   diagnose that stage; do not repeat startup or input blindly.

### Running the service

Build the service candidates using the pinned [Mac release inputs](RELEASE.md). Stage the existing directory layout described below and place a JSON config such as `{"device_key":"phone","usb_id":"<PINNED_USB_ID>","wda_runner_bundle_id":"com.example.HandkiteRunner.xctrunner"}` in its regular mode-0600 `config/host.json`. After verifying the configured runner is installed and handing off exclusive device ownership, run:

```sh
handkited --owned-wda
handkitectl --socket "$HOME/Library/Application Support/Handkite/runtime/handkite.sock" observe phone
```

Daemon startup derives its configuration and runtime paths from the effective OS account, acquires the fixed process lock, and binds the protected socket before initializing the device adapter. The lock remains held through the serving loop. Explicit `--config` and `--socket-dir` arguments are accepted only when both equal those fixed paths; a second socket directory cannot bypass ownership. Missing or unsafe service directories fail without creating them.

After acquiring the lock and before binding the socket, normal startup loads the
fixed config through `install_config::load_protected`. It requires anchored
account-owned parents, a private config directory, and a regular single-link
exact-mode-0600 file with no ACL entries. The bounded parser and final path/
metadata revalidation must succeed before socket or device initialization.
The returned device binding comes from that protected config; it is not an
independent observation of the attached device. Same-account changes after the
read remain possible.

The config also accepts an optional `wda_runner_bundle_id` string. When supplied,
it must be at most 255 ASCII bytes and contain at least two nonempty prefix
components of letters, digits, or hyphens before `.xctrunner`. An installed
runner may append one dot and a ten-character uppercase letter/digit team suffix.
No other suffix is accepted.
Omit it only when intentionally attaching to an externally managed ready WDA; `null` and malformed IDs are rejected.
Keep the actual ID only in the protected host config. The library API
`install_config::load_protected_config` returns the binding and optional ID
through the same anchored reader. `RetainedConfig::wda_runner_bundle_id` exposes
the original retained value; revalidation still requires the exact original
bytes. Neither config API starts WDA or verifies runner installation. The
default daemon mode uses an externally ready WDA; explicit `--owned-wda` mode
requires this runner ID and starts the configured XCTest task and a new session.

The CLI's `request` subcommand reads one JSON request from stdin and returns the complete JSON response. An `observe` response contains a screenshot and accessibility tree, so keep its raw output inside the protected local boundary. While holding the fixed process lock, the daemon can reclaim an owned mode-0600 socket left by a crash. It first checks endpoint liveness with a nonblocking datagram connect, then rechecks the socket inode, owner, mode, link count, size, and modification/change timestamps before removing it through the anchored runtime directory. Only `ECONNREFUSED` permits reclamation; a live endpoint, symlink, non-socket, unsafe owner/mode, changed metadata, or uncertain probe result stops startup. A datagram probe creates no stream connection and sends no request. Default mode requires a ready external WDA. Explicit `--owned-wda` starts a runner and session after protected config and socket checks. Neither mode yet owns pairing, signing, PIN entry, or remote-agent authentication.

Before each action, default mode checks the existing WDA session on the pinned USB route and validates its session endpoint. It adopts that session on the first action and rejects absent, invalid, stale, or replacement sessions. Owned mode instead retains the exact session it created, checks runner liveness and session identity around observation and input, and refuses drift. Neither mode renews a session automatically. A replaced session requires owner inspection.

An unlocked SpringBoard Home screen can retain the public `Uncertain` lock state.
Only `LaunchApp` may use a private, one-capture Home admission: the same paired
capture must report WDA lock `false`, native lock checks before and after must
be Unlocked, a Home icon must be present, and lock or alert elements must be
absent. The admission expires with that capture and is not serialized or kept
for the next action. LaunchApp rechecks native lock before the write and keeps
the original observation, lease, and capture deadlines. Tap, swipe, text, and
lock retain the ordinary lock gate.

Readiness monitors must distinguish this Home state from an unavailable
controller. A matching, ready service may be observed while its public lock
state is `Uncertain`; that alone does not authorize input. For an unlocked
SpringBoard Home screen, use the existing guarded `LaunchApp` path and verify
the resulting app observation. Do not stop an otherwise healthy service just
because its first status reports `Uncertain`, or reinterpret that value as
`Unlocked`.

The separate `wda_runner::OwnedWdaRunner` library can start one exact installed
`.xctrunner` bundle through pinned `idevice` 0.1.68 at Git revision
`89702099f4d7552f704f93b4526756abda319ceb`. It requires a callback from
that launch's test plan, a still-live task and plan, and HTTP 200 with boolean
`value.ready: true`. One absolute startup deadline covers discovery, a separately
time-bounded InstallationProxy lookup, and readiness. Startup cancellation and
handle Drop abort the owned host task. Awaited `abort_and_join` also checks the
retained native process identity, sends at most one graceful stop when that
process remains, and requires both process absence and a refused pinned WDA
connection before reporting cleanup complete. The caller's service ownership
must remain valid throughout. A changed identity, uncertain stop, or expired
cleanup deadline fails explicitly; it never selects another process or retries
the signal.
Startup receipts distinguish an unproven test-plan callback, an unresolved UI
Automation authorization wait, initialization failure, and bootstrap failure.
Known XCTest initialization failures that carry `LocalAuthentication` or
`enabling automation mode`, or occur during an observed automation wait, use
the authorization category. Other initialization failures remain separate.
Raw XCTest messages are not returned; HTTP readiness alone cannot admit a runner.
Callers must retain the handle inside an active Tokio runtime and establish
exclusive device/WDA ownership before calling it.

`handkite-runner-check --service` is a diagnostic for that exclusive handoff. It
loads the fixed protected config, holds the Handkite service lock, starts the
configured XCTest runner once, then aborts and joins the host task. It creates
no WDA session and sends no phone input, so the phone may be locked. Success
also requires the pinned WDA port to refuse a fresh connection after join.
SIGTERM/SIGINT during startup requests cancellation and reports cleanup
unproven. Any refusal requires an independent vacant-port check before another
controller starts; a fixed startup category never proves cleanup. The command
cannot exclude an external controller or its restart. Establish that boundary
before running it, and keep this diagnostic out of the three-artifact release.

The [independent WDA build and verification tools](tools/wda/README.md) prepare
an Xcode-signed iOS runner from a pinned Appium source revision. Their offline
tests do not qualify a real build, install, or launch.

Owned daemon mode now uses this module, then creates a fresh exact session.
On adapter Drop it attempts one bounded, identity-checked session DELETE before
aborting the host task. Acknowledgement does not prove endpoint ownership or
device-process termination; abrupt process termination may skip Drop entirely.
The module does not install a runner. It returns fixed error categories, discards
callback logs, and configures no logging subscriber; upstream `idevice` tracing
must stay inside the caller's private boundary.

The daemon installs SIGTERM/SIGINT handlers before device startup. Each handler
only sets a shutdown flag. The socket listener stops accepting new work within
its 100 ms idle poll; partial requests have a ten-second absolute read budget
and check the flag between reads. Response writes share one 15-second deadline
and stop if shutdown is requested; a client may therefore receive an incomplete
reply after an action. An already dispatched action is never replayed. Owned
runner startup can be cancelled before session creation. The staged launchd
descriptor allows 180 seconds between SIGTERM and
SIGKILL. These are offline-tested shutdown paths, not proof of a live device exit.

In owned mode, a finished runner task also stops socket admission. The daemon
lets an in-flight action return and runs its existing owned cleanup, then exits with
`owned_runner_exited` (code 2) so launchd can restart it. Intentional shutdown
remains normal; borrowed mode has no runner monitor. A lost reply must not be
replayed, and restart does not retry input or change PIN stop protection.

Run `handkited --check --config /path/to/config.json` for a read-only preflight. It prints the same bounded JSON health flags and native lock state as `handkite-doctor`, with no USB identifier or screen contents. Exit codes are 0 for observation readiness, 1 for failed readiness, and 2 for invalid arguments, configuration, or reporting errors. The check does not bind or remove a socket, start WDA, create a WDA session, or send input. This is a candidate-only readiness check, not signed-install or independent WDA startup proof.

`handkited --check-service-layout` checks the effective account's fixed directory
layout and protected host config, including a configured runner ID. Success
prints only `service_layout=ready`. It does not check that the runner is
installed, inspect the phone, acquire the process lock, bind a socket, or start
WDA. This is the read-only preflight for preparing the owned service layout.

The `--check` diagnostic retains its explicit arbitrary-file config route and
existing file/parser checks. Its success does not establish that the fixed
service configuration satisfies the protected startup checks.

For `act`, `receipt.accepted` is `true` or `false` for a known acknowledgment, or `null` when delivery is unknown. A lock action sends the session-scoped WDA `/wda/lock` request once, then queries native lock state once even if the WDA acknowledgment was lost. No Shortcut is required. Its `native_lock` field is `locked`, `unlocked`, or `uncertain`; non-lock actions return `null`. Only native `locked` produces `effect: "lock_confirmed"`. This confirms the postcondition, not that the request caused a transition. A changed screenshot cannot confirm a lock. The optional `after` capture can be absent even when native lock is confirmed.

Lock retains the existing lease, pinned-identity, revision, and unlocked-screen preconditions. The action POST cannot send bytes after the earlier of observation freshness and lease expiry. Native querying has a five-second command timeout and an eight-second process budget. The complete result, including post-action capture, has no shared deadline, so the local client's 65-second read timeout can still expire before a result arrives. A timeout leaves delivery and lock state unknown to that caller; inspect fresh native state before deciding what to do. The controller never automatically resends a lock action.

## Staged Mac service layout

The library's `ServiceLayout::current_account()` derives fixed paths from the effective user's OS account home, independent of `HOME` and the working directory. It accepts no alternate root. All paths below are relative to `Library/Application Support/Handkite` under that home:

| Purpose | Path |
| --- | --- |
| Host configuration | `config/host.json` |
| Versioned signed releases | `releases/` |
| Current release selection | `releases/current` |
| Selected daemon executable | `releases/current/handkited` |
| Selected MCP executable | `releases/current/handkite-mcp` |
| Private runtime socket | `runtime/handkite.sock` |
| Process ownership lock | `runtime/handkite.lock` |
| Durable state | `state/` |
| Staged launchd descriptor | `launchd/com.handkite.host.plist` |

The pure `launchd_descriptor::render` helper emits the intended supervised
descriptor for this fixed selection path. Its sole daemon argument is
`--owned-wda`, so activation requires a protected installed runner ID. Rendering
writes or loads nothing. Separate operators implement initial selection, guarded
upgrade/rollback, and protected GUI LaunchAgents registration. Registration does
not bootstrap a job. See their contracts and activation preconditions in
[Mac release inputs](RELEASE.md).

`validate_existing_directories()` requires every listed parent directory to exist. The home, Library, Application Support, Handkite root, and state directory must belong to the effective user and reject group/other write access. Config, releases, runtime, and launchd directories must additionally be mode 0700. Ancestors above the home must belong to root or that user and reject group/other writes. Symlinks and non-directory components fail closed. This metadata preflight is a point-in-time check; the installer operators additionally retain anchored directory descriptors and validate individual files.

`ServiceLock::acquire` validates this layout, opens the runtime directory without following symlinks, and holds a nonblocking exclusive OS lock on a regular, single-link, owner-only mode-0600 file. The lock has one fixed name for the account's pinned-device service; device aliases cannot create separate ownership domains. The file is never truncated or removed, and process exit releases ownership without deleting state. Metadata and inode checks reject replaced locks or runtime directories. This is cooperative process ownership within the protected service account, not isolation from code already running as that account.

Daemon startup uses the layout and process lock. Layout validation creates no files; lock acquisition creates only the process lock in an existing runtime directory, and daemon startup binds its fixed socket. No startup step creates directories, config, releases, descriptors, or stop markers. The staged descriptor stays outside automatically loaded directories; explicit registration publishes its protected copy to `~/Library/LaunchAgents/com.handkite.host.plist`.

## Release verification diagnostic

`handkite-release-verify --stage /absolute/private-stage` reads independently
supplied pins and exact canonical manifest bytes from a bounded private regular
stdin file with an empty ACL, then calls the production stage verifier once.
It reports only `release_verify=pass`, `release_verify=reject`, or
`release_verify=invalid`.
See the [input envelope and exit codes](RELEASE.md#release-verification-diagnostic).
This diagnostic is excluded from the three-artifact release inventory. Its tests
do not qualify a real signed stage, installation, or service activation.

## Signed release publication

`handkite-release-publish --stage /absolute/private-stage` uses the same
protected stdin envelope as the verification diagnostic. Under the fixed
installer lock, it copies the independently pinned signed artifacts into one
new private stage, publishes one digest-named release and receipt, then
revalidates the held pair. It never changes `releases/current` or starts a
controller. A failed or uncertain publication leaves its residues untouched;
there is no retry or cleanup. Output contains only a fixed status, phase, and
whether a residue candidate exists. See the [publication command contract](RELEASE.md#release-publication-command).

## Device agreement diagnostic

`handkite-device-agreement --service` self-loads the binding from the protected
fixed service config before acquiring the installer lock. It retains that
binding under the exact lock and calls the bounded device observation once.
It reports only `device_agreement=pass` (exit 0) or
`device_agreement=reject stage=<fixed-stage>` (exit 1), including argument and
setup failures. The stage is a fixed category, never an upstream error or
device value. A `lockdown_query` rejection appends a fixed `substage` for the
failed write, length, body read, decode, or reply validation. Decode failures
append fixed format/cause labels and, for schema failures, a fixed envelope
presence class. No reply data or field names are printed; stderr stays empty.
No alternate config, root, socket, or device arguments are accepted.
Extra reply metadata is ignored. Known fields retain exact string-type,
duplicate, error-presence, and success-result checks before identity comparison.

Lock acquisition can create the fixed `runtime/handkite.lock` file in an existing
protected layout. The diagnostic creates no directories and performs no service,
WDA, pairing, PIN, or stop-marker operation. Success is point-in-time agreement
with a self-loaded config binding, not an independent device pin or installation
authority. This binary is excluded from the three-artifact release inventory.
See the [diagnostic contract](RELEASE.md#device-agreement-diagnostic).

## MCP bridge

For a persistent agent client, use `tools/handkitectl.py` or import its `Session`
class. The [Drive Handkite skill](skills/drive-handkite/SKILL.md) describes the
compact response loop, private-data handling and recovery. The client never
retries input or acquires control implicitly. Closing it releases the MCP
session; each request has a bounded dispatch-and-response deadline.

Register the selected local service with native clients:

```sh
claude mcp add --scope user handkite -- "$HOME/Library/Application Support/Handkite/releases/current/handkite-mcp" --service --capabilities observation,navigation,interaction
```

New client sessions load the registration. One executor acquires the lease;
other agents can plan or review but must not compete for input. Registration
does not start a device runner or unlock the phone.

With a matching daemon and MCP build, `tree_format: "elements"` returns compact
visible nodes and `since_revision` requests an added/removed/changed delta.
Use the paired action response as the next view instead of observing twice.
These IDs currently identify presentation entries; native element targeting
remains unqualified. See [validation](VALIDATION.md) for the Taobao stale-tree
blocker. Do not infer screenshot/tree agreement from a matching image alone.

[OpenClaw integration](OPENCLAW.md) is deferred. Direct MCP operation is described below.

`handkite-mcp --socket /path/to/private-dir/handkite.sock --device phone` speaks MCP over stdio. Its default observation capability advertises only `handkite_status` and `handkite_observe`; the latter returns a paired screenshot and accessibility tree from one capture. The device key and socket are fixed when the process starts. Run it as the socket owner.

`handkite_observe` with `{}` keeps the full XML tree. Opt in with
`{"tree_format":"compact"}` for a bounded XML tree presentation of the same
capture; metadata, revision, capture window and image are unchanged. Unsupported,
malformed or oversized XML returns the original full tree with a fixed fallback
reason. This adds no actionable element IDs. See the
[compact tree contract](RELEASE.md#compact-observation-presentation).

Set `"image":"omit"` on an observation to receive its tree and metadata without
an image attachment. Actions accept the same presentation options in a
`"response"` object, for example `{"tree_format":"compact","image":"omit"}`.
These options only reduce the agent response; the host retains its full capture
and normal action checks. Omission means the image was not returned, not that
the screen was unchanged. Default responses still include the full tree and image.

`"tree_format":"omit"` returns metadata and the image without tree text.
It requires `"image":"always"` (the default) and no `since_revision`. The
response says `capture_kind: "paired"`: this only trims output, and still
acquires the full internal tree. It clears the response's tree-delta baseline.

To skip XML explicitly, use `{"capture":"image_only","tree_format":"omit"}`
on `handkite_observe`, or in an action's `response` object for its final capture.
The result has `capture_kind: "image_only"`, a device, capture window and image;
it has no inferred lock state, tree or actionable revision. Obtain a full
observation before another revision-bound action. Input admission still uses
its existing guarded path. Lock retains paired verification; Home and app launch
use a separate navigation-only image revision with native lock and foreground
checks.
Changed pixels can confirm an acknowledged action; unchanged pixels leave its
effect unconfirmed. Full paired capture remains the default.

`"tree_format":"outline"` selects an explicitly lossy reading format. It removes
XML closing tags and repeated type names, combines equal name/label strings and
groups coordinates as `rect=[x,y,width,height]` in logical points. It collapses
empty generic wrappers and marks omitted, explicitly invisible nodes with counts.
Visible descendants, explicit states, field values, and hidden field/modal/
keyboard/container context remain represented. Missing attributes remain unknown.
The response reports `tree_lossy` and `tree_omissions`; malformed or unsupported
input falls back to the full original tree. This outline supplies no actionable
IDs and never replaces the full tree used for action admission.

For repeated observations, send `"image":"changed"` and `"since_revision":42`,
where 42 is the last observation revision your client received and retained in
this MCP session. The first request still returns a complete view. A subsequent
response can replace `accessibility_tree` with `accessibility_tree_delta`:

```json
{"base_revision":42,"start_line":5,"delete_lines":1,"insert":"replacement line\n"}
```

Apply this splice to the previous returned tree, splitting into lines **with
their line endings retained**. `start_line` is zero-based. The result is the
complete tree for the new metadata revision. Deltas are used only when their
serialized text is smaller; unchanged images are omitted only when that same
acknowledged baseline has a known image. State, app, device or format changes,
device errors, limits and missing baselines cause a full-tree response with a fixed
`tree_delta_reset` reason. One tree of at most 1 MiB is retained per connection.
Omit `since_revision` after losing context, reconnecting, or changing formats.
These options also work inside an action's `response` object. They do not change
`expected_revision`, action admission, internal capture work or device latency.

`handkite-mcp --service` derives the device key from the protected fixed host
config and the socket from the effective account's fixed service layout. It does
not accept a caller-selected device or socket in this mode. This is the preferred
command for a restricted SSH principal once the service is installed. It does
not authenticate the caller: bind the exact invocation and capabilities to an
SSH forced command, with forwarding and PTY allocation disabled. The default
capability remains observation only.

The host can explicitly add `--capabilities navigation` or `--capabilities observation,navigation`. Navigation permits only `handkite_acquire_control`, `handkite_release_control`, `handkite_home`, and `handkite_launch_clock`. Home and Clock require an unsigned integer `expected_revision` and reject every other argument. Clock always means `com.apple.mobiletimer`. Navigation alone does not grant either observation tool. There are no lock, unlock, or PIN tools.

The separate `observation,navigation,interaction` profile adds `handkite_tap`,
`handkite_swipe`, `handkite_text`, `handkite_launch_app`, `handkite_double_tap`,
`handkite_long_press`, `handkite_key`, and `handkite_click_button`. It is opt-in
at host startup and does not change the existing profiles. Every action requires
a current unsigned observation revision and explicit control. Points must be
finite coordinates from 0 through 10,000; swipes and long presses last 1 through
5,000 ms. Text is limited to 4,096 UTF-8 bytes and is sent whole and verbatim in
any script, without normalization. It excludes control characters and line
breaks, U+2028 and U+2029, and U+F700 through U+F8FF, which XCTest reads as
hardware keys; use `handkite_key` for Enter and Tab.
Text uses 240 characters per second by default. The optional
`HANDKITE_TYPING_FREQUENCY` service variable, a plain decimal from 60 through
600 read once at start, overrides that rate; unset or invalid uses 240. The
[text matrix](tools/TEXT_MATRIX.md) qualifies scripts, keyboards and rates.
App identifiers are bounded ASCII bundle identifiers. Arguments reject unknown
or duplicate fields, including nested target fields. Each call dispatches once,
with unknown delivery reported without replay. These tools can perform real
purchases or send messages; grant this profile only to a trusted personal agent
with approval bound to the concrete action. OpenClaw integration is deferred;
the former separate deployment is retired.

Human-like pacing is opt-in and off by default, so default latency is unchanged.
Start the MCP with `--pacing natural` or `--pacing cautious` to pace a whole
session, and `--pacing-seed N` to replay its jitter. Every action tool also
accepts an optional `"pacing":{"policy":"off|natural|cautious","seed":N}`
override for one call. Paced calls wait a seeded log-normal gap measured from
the previous action (agent think time counts) and respect an actions-per-minute
cap: natural 40, cautious 24. Coordinate taps, double taps and long presses move
only inside the smallest element frame known for the same revision, inset from
its edges and within a few points; with no frame the point is unchanged.
Long-press and swipe durations vary within bounded factors. The daemon then
varies press duration, sends swipes along an eased, slightly curved path with
the same endpoints and total duration, and types printable ASCII text of up to
96 characters with per-key cadence and bursts in one W3C key-action request.
Other text stays one whole string at a sampled frequency, so grapheme clusters
are never split. Pacing never splits, retries or reorders input, and never
changes `expected_revision`, admission or verification: the jittered action is
admitted as sent. Element tools receive only the gap and cap. Motion reaches the
one WDA send as an explicit parameter; no state outlives the request. A paced call
needs a daemon from the same release. An older daemon refuses the field before
dispatch, and the MCP reports `PacingUnsupported` with `delivery:"not_sent"`
without resending; retry with pacing off or update the host. Off sends the
unchanged request.

Element target input is separately opt-in: `handkite_observe` with
`{"targets":true}` (or `Session.observe_targets()`) returns compact target rows
with opaque `target_id` values, rectangles and bounded public values. These are
not the presentation `eN` IDs. Use `handkite_tap_element` or
`handkite_text_element` with `target_id` and `expected_revision`; text also takes
`value` and requires that exact native field already focused. Target revisions
cannot be used for legacy coordinate actions. Native action replies include the
next compact targets/delta when available; `response:{"image":"always"}` opts
into a screenshot. Images default to omitted for this path.

Rows carry `source:"xml"` or `source:"ocr"`. On macOS, Apple Vision recognizes
text missing from the native hierarchy. When a unique native text row already
covers the exact text, that row carries `input_source:"ocr"` and its opaque
`target_id`; no duplicate row is added. Its XML identity stays intact. OCR input
uses the recognized glyph bounds, not the native row center. OCR targets are tap-only; they never claim
to be editable native fields. Recognition stays on the host and reuses unchanged
image tiles. An OCR tap rechecks native state, safe context, the surrounding
pixels and fresh recognized text before one coordinate dispatch. Missing or
ambiguous recognition refuses input. Uncertain effects are never replayed.

Native targets require stable target-region pixels, exact native identity and
full attributes, verified viewport mapping, safe context and restored WDA
settings. Handles are one-use and expire after 15 seconds. Outside-region
animation is allowed, but unstable targets are omitted. This does not establish
semantic accessibility freshness; OCR supplies separate image evidence, not a
claim that missing native nodes exist. Live qualification is recorded separately.
See [the agent-driving skill](skills/drive-handkite/SKILL.md).

`handkite_click_button` takes an observed `target` with exact `label`, `x`, `y`,
`width` and `height`. It resolves one visible, enabled, session-bound Button or MenuItem,
verifies its attributes, and clicks that identity once. It can address an overlay
button whose coordinates overlap an underlying row; ambiguous ordinary taps
remain refused. Missing, duplicated or changed targets do not fall back to a tap.

`handkite_key` accepts `backspace`, `enter`, `tab` or `escape`. By default it
requires the exact observed image. Optional `selection: "current"` explicitly
uses iOS's current editing selection, with an eligible field and keyboard,
unchanged tree/state, native unlock and the original deadline. It tolerates
cursor-only screenshot drift but does not prove or preserve a previously
observed selection range. Enter can submit a form; Backspace can delete its
current selection. Arrow keys and modifier shortcuts are unsupported.

Current-selection keys and double-tap/long-press also support a single visible,
enabled TextView with one visible, enabled keyboard. Gestures must land inside
the editor and outside the keyboard and overlapping buttons. Missing focus
metadata remains unknown; explicit false or malformed focus is refused. The
full tree/state, native unlock, session ownership and five-second deadline still
apply. Strict keys and ordinary text insertion keep their previous checks.
`handkite_text` also accepts `selection: "current"`. It applies the same
current-editor proof to insertion, including TextView editors, and allows only
screenshot drift with an unchanged tree and state. Omit it to retain ordinary
text admission. For multiline entry, use current-selection text calls with an
explicit current-selection Enter between them; literal newlines remain excluded
from `handkite_text`. Deploy matching daemon and MCP versions before using this
option.

Tap and swipe coordinates are WDA viewport coordinates in logical points,
passed unchanged, including fractional values. They are not screenshot pixels.
Handkite supplies no screenshot-to-action transform or inferred scale/rotation.
Moving swipes use one W3C touch sequence with a 50 ms press, the requested
movement duration and immediate release. Durations are never shortened implicitly.
Zero-distance swipes retain their native hold behavior.

An app launch from Home tolerates changes in screenshot or tree bytes only when a fresh private Home check proves native unlock before and after capture, WDA reports unlocked, and device state matches the requested observation. Revision, identity, lease, and send deadlines still apply. The explicit owned-runner admissions documented here and in [release inputs](RELEASE.md) preserve their narrower checks; other actions require an exact view match. `StaleRevision` means the requested revision is no longer current; `ViewChanged` means the fresh screen context failed the action check. Both reject input before transport.

A `ViewChanged` refusal includes `structuredContent.changed` with boolean `state`, `accessibility_tree`, and `screenshot` fields. These identify the changed components without returning their contents. They do not authorize a retry or relax the comparison. Invalid diagnostic replies retain the unknown-delivery result.

Adapter failures can include a fixed stage code in `structuredContent.code`,
such as `AdapterNativeBefore`, `AdapterSessionCheck` or
`AdapterOwnershipAfterAck`. The last means a validated action acknowledgement
preceded a failed ownership check. These codes contain no upstream error text.
Every adapter failure still reports unknown delivery; inspect fresh state before
deciding on another action, including after `AdapterInputExpiredBeforeSend`.

An owned runner may admit a tap despite screenshot drift when device state and the complete accessibility tree match exactly. The fresh capture must prove one labelled, visible, enabled button containing the requested point within the application bounds, with no overlapping visible button or lock, alert, sheet, or secure-field context. Native unlock must be confirmed before and after capture and again before sending. The tap deadline is at most two seconds after capture; identity, revision and lease checks remain. Borrowed runners, text, swipes, lock and PIN input retain their existing rules. A matching view does not prove the tap's effect; check its receipt and postcondition separately.

Home has a separate owned-runner admission for screenshot drift with unchanged state and tree. It requires native unlock before and after capture, WDA unlocked state, and one valid application without lock, alert, sheet or secure-field context. Its two-second capture deadline and native check before sending apply only when that admission is used. Admitted Home validates the already-bound session and sends one dedicated Home-screen activation request; it never falls back to a button press. Existing exact-view Home behavior remains available without that admission. Home does not require a button target, and its effect still needs separate verification.

Normal observations retain WDA's visibility, enabled, focus, selection, and placeholder attributes for interaction targets. An absent attribute remains unknown; clients must verify the required target state before sending input. Diagnostic and passcode projections are separate.

The pinned WDA exports the XML `focused` attribute only on tvOS. Do not require it for an iPhone test or interpret its absence as `false`. Text values can contain the placeholder when the actual field is empty. The benign Settings-search test uses the observed search field and keyboard, then verifies the exact resulting text.

The pure `ssh_bridge::render` helper accepts a `ServiceLayout`, one bare canonical
`ssh-ed25519` public key, and an explicit `mcp::Capabilities` value. It returns one
LF-terminated `authorized_keys` entry with `restrict` and a forced command:
`exec '<fixed releases/current/handkite-mcp path>' --service --capabilities <value>`.
Key options, comments, certificates, extra whitespace, and malformed SSH wire
lengths are rejected. The absolute path must be normalized, at most 4096 bytes,
and contain only ASCII letters, digits, slash, space, dot, underscore, and hyphen.
It is derived from the account layout, never a client-supplied command or path.

OpenSSH documents `restrict` as disabling port, agent, and X11 forwarding, PTY
allocation, and user SSH rc execution. Its `command` option replaces the requested
command; the renderer never references `SSH_ORIGINAL_COMMAND`.
See the official [OpenSSH authorized_keys format](https://man.openbsd.org/sshd.8#AUTHORIZED_KEYS_FILE_FORMAT).
The helper reads and writes no files and does not verify the key owner, selected
release, SSH configuration, or effective restrictions. Offline shell fixtures
establish exact argv only. Live sshd enforcement, host identity, key enrollment,
account isolation, and agent acceptance remain unqualified. Account shell startup
and server configuration, including `ForceCommand`, need separate review.

`handkite-ssh-key --capabilities observation < bare-public-key` exposes that
renderer as a read-only CLI. Capabilities are mandatory; `navigation`,
`observation,navigation`, and `observation,navigation,interaction` are the other accepted values. Stdin must contain
the bare key with at most one final LF, followed by EOF. It reads at most 82 bytes
and prints the single entry to stdout; failures emit fixed errors to stderr and
exit 1 without echoing rejected data. It derives the effective account's layout
without inspecting the installation and accepts no path, command, socket, or
device override. It writes no files, enrolls no key, and is excluded from the
three-artifact release inventory. Review the output and the actual SSH account
policy separately before any enrollment.

Acquire control explicitly before an action. The process holds the controller's random lease token internally for a two-minute idle TTL, renewed only after a verified successful action; neither the token nor an owner override is exposed through MCP. Acquire never renews an existing lease. A competing session receives `Busy`. Release is explicit, with best-effort release again on EOF or input/output failure. A killed process relies on expiry. If the acquire response is lost, that process refuses further acquisition: close it and wait two minutes before starting a new session.

Actions return a delivery receipt, observed effect, and optional post-action metadata. When the host grants observation capability, the same response also includes the already captured post-action screenshot and accessibility tree. This adds no device request. Navigation-only clients receive metadata alone. A lost acknowledgment, unexpected reply, post-input identity mismatch, or oversized response reports unknown delivery with `receipt.accepted: null` and `effect: "unconfirmed"`. No action is automatically replayed. A stale revision requires a fresh authorized observation; an expired lease requires another explicit acquisition.

The MCP result sets `isError: true` when the effect is `unconfirmed` or
`receipt.accepted` is not `true`, preserving the receipt, effect, and available
post-action metadata. An unconfirmed effect includes an accepted action that
leaves the view unchanged and a failed post-action capture. Input may have
happened; inspect a fresh authorized observation before another action. An
accepted action with a changed view omits `isError`.

An explicit accepted receipt with `effect: "unconfirmed"` legitimately carries
`isError: true`. Preserve both fields and inspect the returned screen, or take
one fresh read-only observation if none was returned, to check the expected UI
result. Record that semantic confirmation separately
from the original effect. A proved postcondition can admit the next different
planned action; it never authorizes replaying the accepted input. Malformed
replies and false or unknown acceptance stop the sequence.

For remote use, an authenticated SSH key must be bound to a restricted forced command that fixes the socket, device, and capabilities, disables forwarding and PTY allocation, and ignores client-supplied commands. Do not grant that principal an unrestricted shell or direct socket access. A CLI capability flag is host policy, not authentication; SSH access that lets a caller select another command or flags does not enforce capability separation. Keep the Unix socket and WDA off network listeners. Host identity, key enrollment, forced-command restrictions, and the intended OpenClaw connection remain unqualified and require separate setup and acceptance.

The owned local path has passed real-device Clock launch, Home return, Settings launch and two-way scrolling, App Store text entry, and free WeChat installation and launch. These are separate recorded trials; text selection and editing-key qualification remain pending. Remote restricted-SSH enrollment and OpenClaw acceptance remain unqualified. See [validation](VALIDATION.md). Observation returns private screen content to the calling agent.

An authorized client with both capabilities follows this sequence after MCP initialization:

1. Call `handkite_acquire_control` with `{}`.
2. Call `handkite_status` with `{}` for a navigation-only revision, or observe targets when element input is needed.
3. Call `handkite_launch_clock` with only `expected_revision` set to that integer.
4. Inspect the receipt, effect and returned screen. Use its `after.revision` for the next action. Observe again if no post-action screen was returned or the view has since changed.
5. Call `handkite_release_control` with `{}` when finished.

Use `handkite_observe` when the agent needs screen content or element handles.
With the observation/navigation profile, `handkite_status` supplies a fresh
`revision_scope: "navigation"` revision without XML. Only Home and app launch can
consume it. Animated feeds therefore do not trap navigation behind a drifting
tree. Drifted paired observations can fall back to locally stable XML/OCR targets,
or an image with navigation-only authority. A stale or missing revision cannot authorize input; do not guess revisions. After unknown delivery, inspect a fresh observation before deciding on a new action; repeating the earlier request is not recovery proof. The bridge never switches to another device or fallback route.

## Device doctor

Production USB discovery and transport use the Mac's fixed `/var/run/usbmuxd`
socket. `USBMUXD_SOCKET_ADDRESS` cannot select a TCP bridge or alternate socket.
Discovery accepts exactly one USB entry with the configured UDID and retains its
actual UDID and mux device ID. The provider keeps the pinned upstream discovery
and connection transport, including its existing private tracing boundary.
Native lock queries invoke `/usr/bin/xcrun` by absolute path, so `PATH` cannot
replace that executable. The selected Apple developer tools and their environment
remain external inputs; this path pin does not qualify them.

`handkite-doctor --config /path/to/config.json` checks the pinned USB device, WDA status, the existing WDA session, and native lock state. It reports only readiness flags and lock state as JSON; it does not create a WDA session or send device input. Exit code 0 means the locked or unlocked phone can be observed. It does not mean app actions or PIN entry are qualified.

## Repeatable MCP speed benchmark

`tools/benchmark_mcp.py` measures tap, type, swipe, app launch, image-only
observation and full-tree observation through one MCP client. It uses the
existing service; it never starts a controller, changes WDA settings or enters
a PIN. Enable `HANDKITE_TIMINGS=1` on the normally started service through the
qualified diagnostic route first. The runner enables the same flag on its
MCP subprocess and checks both trace streams before sending input.

The example `tools/benchmarks/notes-settings.json` needs the dedicated Notes
fixture containing exactly `Alpha` and a trailing newline. Its Settings
preparation uses `fresh_launch: true`: an independently qualified driver must
provide `Campaign(settings_reset=...)` to terminate only Settings through the
existing owned WDA session, without retry. The callback returns `True` only
after a valid termination response, including an already-stopped app. A missing
or unconfirmed callback stops before launch. The runner discards the old view,
observes again, then uses the normal guarded launch and strict root-list check.
It never navigates Back or taps a Settings row to repair the fixture. Coordinates
use logical points on a 393×852 viewport.
Use `--calibrate` for one trial per case before the measured campaign; this
report cannot qualify the regression gate.
The phone must be unlocked and exclusively handed to this client. Do not run
another status or observation client during a campaign: traces are correlated
across the two serial request streams.

```sh
python3 /path/to/qualified-benchmark-driver.py run \
  --plan tools/benchmarks/notes-settings.json \
  --output /private/path/before.json \
  --service-trace /private/path/service-timing.jsonl \
  --service-pid SERVICE_PID --service-build FULL_COMMIT \
  --client-build FULL_COMMIT --count 20 -- \
  /path/to/handkite-mcp --service \
  --capabilities observation,navigation,interaction
python3 tools/benchmark_mcp.py compare /private/path/before.json /private/path/after.json
```

Each case runs one warmup and at least twenty measured trials. Select remaining
cases with
`--cases launch observe_image observe_tree` before `-- COMMAND`. This preserves
the canonical plan and each case's preparation and cleanup; a partial report
cannot pass the full regression gate. Preparation and cleanup are timed
separately. The report includes nearest-rank p50/p95,
response sizes, build identities and per-stage timings, without screenshots,
trees, typed text or device identifiers. Failed actions stop the campaign;
input is never retried. One read-only observation can diagnose uncertain input.
Preparation failures retain their case, step, request identity, fixed change
flags and failing predicate names, without predicate values. An unmeasured
setup failure cannot disappear from the report.

Swipe cleanup now uses `settle: 5` observations before and after its inverse
gesture. This accepts two consecutive matching device, state, full-tree and
image frames within five reads, then uses the newest revision. It sends no
input when the view remains unsettled. These reads count toward cleanup time,
not the measured forward gesture. The plan hash changes with this fixture
correction; retain earlier reports and identify it when comparing their
otherwise unchanged measured actions.

On a build with explicit image-only capture, pass `--capture image_only` to
measure that policy for actions and the image observation case. The full-tree
case remains paired. The runner checks acknowledged actions against changed
image bytes, then obtains a separate full observation to verify the fixture's
semantic expectations before cleanup. Reports label the capture policy and
record this extra `fixture_verification_ms` separately; it is not part of the
image-only request latency and must not be hidden when comparing workflows
that need a full actionable revision after every step. Default runs retain
paired capture, including the presentation-only image case.

To prioritize a case without repeating it later, run disjoint case subsets on
the same unchanged service generation, then use
`benchmark_mcp.py merge --output complete.json swipe.json remaining.json`.
The merge requires every case to pass twenty trials, preserves each report's
hash, and rejects overlapping cases, mixed builds or process generations,
failed runs and calibration. It does not concatenate unrelated trace streams.

If a later case stops during preparation, completed cases can be retained with
`benchmark_mcp.py extract stopped.json --cases tap type --output completed.json`.
Extraction requires a globally valid trace and every selected trial's full
verification and cleanup. It preserves the parent hash and failure in the
cohort's provenance; it does not turn the stopped parent into a passing run.
Invalid traces and incomplete cases cannot be extracted. The cohort can then
be merged with disjoint passing cases from the same service generation.

Use `benchmark_mcp.py reference complete.json --output reference.json` to retain
only a complete run's metrics, source hashes and plan identity for later
`compare` calls. This omits process IDs, individual calls and sample context;
it rejects failed, incomplete and calibration runs. Keep the capture policy
the same for a like-for-like regression gate; comparison output names both
policies when they differ.

The first complete real-device reference is
[`reference.json`](tools/benchmarks/reference.json): twenty
successful trials of each case on one service generation, with image-only
action responses. Compare a future complete report against it with the same
plan and capture policy. [Validation results](VALIDATION.md)
record the stage timings, separately measured semantic verification and
remaining budget misses. Passing the relative regression gate does not mean
every product budget is met.

For a bounded follow-up, use `compare before.json after.json --cases type swipe`.
The result names the selected cases and still requires twenty successful samples,
valid traces and the same plan. Omitting `--cases` keeps the full six-case gate.

`span.*.covered` is the union of overlapping intervals in that stage;
`pre_tree_reuses` counts successful pre-action snapshot reuse. Such captures
are marked `reused_tree` and require a successful fresh frame probe; their
missing source timing is intentional. The original snapshot age is retained.
`span.*.work` is their cumulative work. Neither should be added to enclosing
or overlapping stages. Capture stages are reported separately for pre-input
admission and post-input verification. `input_request` measures the WDA input
request, excluding element lookup; it is not a separate unverified-input API.
WDA response time includes its internal idle and snapshot work, which the host
cannot split without WDA instrumentation. `tree_format: "omit"` alone reduces
the MCP payload only; `capture: "image_only"` explicitly skips the tree.

The comparison command fails for missing samples, failed trials, invalid or
incomplete traces, different plans, or a p50/p95 regression above 5% plus 20 ms.
Product budgets remain separate from that relative regression gate: input
dispatch under 1 s, image-only observe under 500 ms, full-tree observe under
1.5 s, launch under 3 s and full act-and-verify under 2 s.

## Owned-session idle settings

The owned daemon's candidate timing policy sets its new WDA session to
`waitForIdleTimeout: 0` and `animationCoolOffTimeout: 0` before reporting
readiness. This removes both implicit waits; Handkite still captures and
verifies the action's effect and retains the final fresh native lock guard
and ownership checks.
The candidate requires matched live validation before promotion. Separate
one-shot PIN settings remain `1.0` and `0.2`. The daemon retains the exact
original values and restores them before closing the session. Failed setup
attempts restoration before returning an error. Setup, restoration and close
writes require the original controller ownership and a live runner; ownership
loss is reported and never authorizes a write. Borrowed sessions are unchanged.

## Read-only idle settings

`handkite-probe idle-settings --config PRIVATE_FILE` reads the active existing
WDA session through the private USB pin. It prints only finite nonnegative
`waitForIdleTimeout` and `animationCoolOffTimeout` values, or a fixed refusal.
It sends no POST, creates no session, and performs no phone input.

`handkite-probe restore-known-idle --config PRIVATE_FILE` is an explicit one-shot
recovery for the known original settings `10` and `2`. Under externally proven
exclusive WDA ownership, it requires two fresh, agreeing reads of stranded
`1` and `0.2` (including `0.20000000000000001`) before one fixed restoration POST.
Already restored `10`/`2` sends no POST; other values refuse. Readback and session
checks run even after an uncertain receipt, which remains unknown. It never
retries or sends phone input. Live recovery remains unqualified by offline tests.

## Clock trial with temporary idle settings

`handkite-probe touch-trial-idle --config PRIVATE_FILE --transport w3c`
(or `coordinate`) explicitly brackets the guarded Clock trial below with
temporary idle settings `1.0` and `0.2`, then restores the exact original values.
It requires externally proven exclusive WDA ownership for the entire invocation;
the command does not acquire a lease. Do not cancel the bracket while it runs.
Both pre-input Clock captures must match the settings transaction's session.
Confirmation requires both the Clock transition and verified settings restoration;
an unproven restoration or worker join reports unknown. There is no retry or
fallback. This command is offline-tested only; live use is not yet qualified.
The default `touch-trial` command does not change idle settings.

`handkite-probe touch-trial-owned-session --config SERVICE_CONFIG --transport w3c`
(or `coordinate`) is the explicit no-PIN qualification route. The config path
must be the fixed protected service config, with a configured runner ID. The
probe acquires Handkite's service lock, starts one XCTest runner, creates one
session, and checks the unlocked World Clock starting state twice on that exact
session before one tap. Runner and service ownership are checked through USB
connect and the tap write; a lost service lock during cleanup cannot confirm the
trial. It requires a confirmed Clock transition and verified
idle-settings restoration, then attempts one bounded close of that session and
joins the runner. Uncertain cleanup is reported as unknown, without replay.
It never reads a PIN or touches the stop marker. This route is offline-tested;
live use still requires a reviewed exclusive handoff from any other WDA owner,
because Handkite's service lock does not exclude an external controller.
Before launching XCTest, the owned runner accepts only an explicit refusal from
the pinned device's WDA port as a point-in-time vacancy observation. A listening
endpoint or uncertain connection refuses startup. Vacancy does not prevent a
different controller from restarting or prove the new endpoint's ownership.

`handkite-probe touch-trial-owned-auto-clock --config SERVICE_CONFIG --transport w3c`
is an additional explicit no-PIN route for the same exclusive handoff. Leave
the phone manually unlocked; **do not prepare World Clock first**. The runner
and its exact session must be ready before the probe sends one guarded Clock
launch request. If iOS asks to enable UI Automation, the operator must approve
that prompt; the probe neither enters a PIN nor infers approval from a generic
startup failure. An uncertain launch receipt stops before any tap. After an
acknowledged launch, fresh captures must prove either World Clock or Stopwatch
selected, then verify that same starting tab twice before one tap of the other
tab and one postcheck. An alarm, timer, overlay, locked phone, session drift, or
unsupported layout refuses the tap. Both routes share settings restoration,
exact-session close, runner join, and uncertain-cleanup behavior. The auto
route has offline fixture proof only and is not a qualified live handoff.

`controller_process::observe` is a separate read-only macOS diagnostic for one
privately pinned PID. It compares effective UID, wall-clock and monotonic process
start times, executable path, and executable Mach-O UUID. Two complete samples
must agree before returning `Match` or `Mismatch`; disappearance during a sample,
permissions, malformed responses, zombies, and unsupported platforms return
`Unknown`. `Absent` requires two initial PID lookups returning ESRCH and means
only that PID was absent at those observations. Each complete sample brackets
the path read with start-time, UID, and executable metadata checks. There are at
most ten native reads, without process enumeration, argv, environment, signals,
subprocesses, or executable-file access.

These are advisory observations, not an atomic process handle. Same-value ABA
changes and changes after the final read remain possible. A UUID/path match is
not code-signature or byte-integrity proof; for an interpreter it identifies the
executable, not a script or its arguments. No result establishes that a controller
is stopped, excludes restart or another PID, identifies a launch supervisor, or
permits selection, activation, or WDA ownership. Independent pins remain private
and are never generated from the process being compared by the production API.

`handkite-controller-process --pinned-stdin` exposes that diagnostic without
service setup. Stdin must already be an effective-user-owned, mode-0600 regular
file with one link, an empty ACL, offset zero, and stable metadata. It accepts at
most 8192 bytes containing one strict JSON object with `schema_version: 1`,
positive `pid`, positive `start_seconds`, `start_microseconds` below one million,
positive `start_abstime` (raw host-local Mach absolute ticks), `effective_uid`,
an exact absolute `executable_path`, and a nonzero `executable_uuid` encoded as
32 lowercase hexadecimal characters. Duplicate, unknown, missing, mistyped, or
invalid fields refuse before any process lookup. Preserve independent host/boot
provenance for these pins; the command does not discover or export them.

Stdout contains only `controller_process=match` (exit 0), `mismatch` or `absent`
(exit 1), or `unknown` (exit 3). Invalid arguments/input also produce `unknown`;
stderr carries no diagnostic details. No PID, path, UID, UUID, or timestamps are
printed. A match establishes only the stated comparison, never handoff admission.

After cleanup, the owned Clock routes emit eleven fixed-field timing receipts to
stderr before their terminal status. Monotonic `elapsed_us` covers runner
startup, the session/settings transaction, an optional Clock launch and tab
detection, both preflights, tap, the sole
postcheck, exact-session close, runner join (including the final ownership
check), and total invocation work. The transaction includes session creation,
settings, the Clock trial, and restoration; its time overlaps the four Clock
steps and must not be added to them. Total excludes receipt formatting/output.
Skipped steps have no duration. Lost worker receipts are `unavailable`; a capture
after an uncertain tap receipt is `observed_unvalidated`, never proof of the
Clock transition. Fixed failure reasons contain no device/session identifiers,
paths, screen content, coordinates, or credentials. Runner startup reports a
fixed lookup, endpoint, timeout, exit, or task failure category instead of
collapsing every failure into `runner_unavailable`. These categories do not
identify a UI Automation prompt or pairing state. `complete` describes only
that step; confirmation still requires the existing final checks. No latency or
live reliability claim follows from the offline fixtures.

## One-shot Clock touch diagnostic

An explicitly supervised trial can run
`handkite-probe touch-trial --config /path/to/config.json --transport w3c`
or select `coordinate` instead. This command sends input when its preconditions
pass. The explicit regular mode-0600 config pins the USB identity through
`host_config::load`; this diagnostic does not use the ambient device environment
variable or establish protected service-config custody.

Manually leave the unlocked phone in the English Clock app with World Clock
selected. The probe requires ready WDA, a validated unchanged existing session,
matching pinned USB identity, native and WDA unlocked state, and two fresh
screenshot/tree captures with stable Stopwatch tab geometry. Its separate
attribute-rich tree retains `visible`, `enabled`, and `selected`; ordinary
controller observations keep their existing projection. Capture windows must
be at most ten seconds, and the paired evidence must be at most two seconds old
after the final identity, session, and native checks. Missing visibility/enabled
attributes, incoherent selection evidence, alerts,
secure fields, lock markers, ambiguous geometry, or an already selected Stopwatch
tab refuse input.

Selection requires reciprocal explicit `selected` booleans on both tabs, or
both `selected` attributes absent with exact `value="1"` on the active tab and
no value on the inactive tab. Explicit booleans can have an absent value or a
corroborating active `1`; inactive values and every other value are refused.
Before input, exactly one visible World Clock NavigationBar must be inside the
Application and outside TabBar, and Start must be absent or explicitly hidden.

The capture also carries an absolute monotonic expiry into the selected input
transport. Discovery and connection time consume that budget. Expiry is checked
immediately before each socket write poll; an expired connection sends no POST
bytes. Expiry after a partial write leaves delivery unknown and never retries.

The selected transport sends one tap at the tab's accessibility-coordinate
center. One post-action paired capture must show Stopwatch selected, World Clock
unselected, no visible World Clock NavigationBar, and exactly one visible enabled
Start button inside the Application and outside TabBar, with the same device/session and
unlocked state. This uses accessibility evidence alongside the screenshot; it
does not perform image recognition. A receipt alone is insufficient. Output is
limited to fixed status words and the selected transport: exit 0 confirms the
observed result, while exit 1 reports invalid arguments/config, refusal before
input, or an unknown result after dispatch. Lost acknowledgment remains unknown
even if the postcheck succeeds. No screen content, identifiers, or captures are
printed or retained.

Coordinate trials retain fixed post-dispatch diagnostics: `http_404` reports only
the HTTP status, `receipt_unproven` covers other unproven receipts, and
`postcondition_unproven` means an accepted receipt lacked a proven postcondition.
All three remain `unknown`; HTTP 404 does not establish a missing route or input
delivery. Exactly one postcapture is attempted even on receipt failure, whose
category takes precedence. W3C output is unchanged. The idle variant exposes
these categories only after verified settings restoration; restoration failure
keeps its existing unknown result.

There is no automatic retry, transport fallback, app launch, session creation,
PIN input, stop-marker change, or service installation. A separate invocation
requires fresh operator judgment. Offline fixtures qualify this command's
control flow only; route-specific live evidence is recorded separately in
[validation](VALIDATION.md).

## PIN stop gate

`handkite-probe assistive-attrs --config PRIVATE_FILE` is read-only. Two distinct
fresh XML captures retain `hittable` and require the same pinned existing session,
native/WDA lock after prior unlock, and an empty six-digit SpringBoard keypad
without alerts. An exact, unique in-app AssistiveTouch Button or Other must have
explicit true visible/enabled/hittable flags and stable in-screen bounds, at least
eight accessibility points from every key, the field, Cancel, Emergency, and other
recognized controls. Missing competitor bounds refuse even for hidden controls.
Output is limited to fixed categories and the target rectangle normalized to
0-1000 screen units. XML is not saved or printed; no screenshots, input, settings,
PIN credential, or stop-marker operations occur. This diagnostic does not prove
pixel appearance or authorize a tap. The Cancel trial guard is unchanged.

`handkite-probe pin-field-compare --config PRIVATE_FILE` is read-only. It compares
the source's empty six-digit field with one direct secure-field lookup and one
value-attribute GET, requiring the same pinned existing session, native/WDA lock,
prior unlock, and a valid empty keypad without alerts before and after. The
`/elements` POST only queries elements; no input or settings endpoint is used.
It prints only source/direct counts, total six, and elapsed milliseconds bounded
to 60,000. Duplicate elements, malformed responses, and session drift refuse.
The sequential observations are not atomic and do not prove input delivery.
There is no PIN credential read, stop-marker access, retry, or saved XML.

`handkite-probe cancel-trial-idle --config PRIVATE_FILE --supervised-hidden-cancel`
is a one-shot Cancel diagnostic, with no PIN or stop-marker access. Before use,
the operator must visually confirm Cancel and six empty dots, and externally
prove exclusive WDA ownership. Two distinct fresh paired captures must agree on
the pinned existing session, locked SpringBoard, empty keypad, and Cancel geometry.
Cancel must be enabled and separate from every digit, the field, and all other
buttons and recognized controls. Competitors require valid bounds even when hidden or
disabled; missing bounds refuse. An explicit false visibility attribute requires
the supervision flag. Missing visibility always refuses. PNG header/aspect checks do not classify screenshot pixels.
The command brackets one W3C tap with temporary idle settings and attempts exact
restoration on every completed path. The second capture's original five-second
deadline bounds the tap write. One postcapture must prove the same session still
locked on the normal lock screen, with no keypad, field, or alert. Only accepted
receipt, proven postcondition, and verified restoration produce
`cancel_trial_idle=confirmed`; post-dispatch failures are `unknown`. There is no
retry or fallback. Refusal sends no tap, but the settings bracket can still run.
Process termination can prevent restoration. Offline fixtures do not qualify live
Cancel delivery or PIN entry.

`handkite-probe cancel-geometry --config PRIVATE_FILE --supervised-hidden-cancel`
is a read-only aid for a Cancel geometry refusal. It reuses the pinned locked
capture and exact trial parser, printing only a fixed classification. A geometry
failure from invalid competing bounds also identifies the first competitor using
only `button`/`other_control` and `emergency`/`delete`/`other` classes.
It also reports that competitor's `enabled` and `visible` categories as
`true`/`false`/`missing`; these flags do not relax the bounds guard.
A failure is reported at its first observation; `stable` requires two fresh,
nonoverlapping captures with the same session and geometry. Other guards still
refuse. It sends no input or settings writes and saves no screenshot or XML.
The classification does not authorize a tap or relax any trial guard.

`handkite-probe pin-geometry --config PRIVATE_FILE` is read-only. After pinned
device, unchanged existing session, and native/WDA lock checks, it reports bounded
screen/PNG dimensions and centers for keys 0-9 in permille (0-1000) of the screen.
It requires a fresh paired capture and a unique empty six-digit keypad, including
the known hidden-visibility schema. No screenshot, XML, field value, or identifier
is printed or saved. PNG header/aspect checks do not classify screenshot pixels.

`handkite-probe normal-lock-attrs --config PRIVATE_FILE` is a read-only aid for
diagnosing a normal-lock-screen refusal. It reports only fixed Application and
proud-lock categories, prohibited-element presence, and geometry validity after
the existing pinned-device/session and lock checks. It sends no input and
prints or saves no screenshot/XML. Its output does not relax the reveal guard.

`handkite-probe reveal-keypad --config PRIVATE_FILE` is a separate one-shot
diagnostic that sends USB keyboard Return only after two distinct fresh captures
whose accessibility trees identify a normal lock screen. Both require the pinned
device, unchanged existing WDA session, matching screen/image dimensions, native
and WDA locked state, prior unlock since boot, and no keypad or field. Overlapping
or reused capture windows refuse input. PNG header/aspect checks do not classify
screenshot content or establish an atomic visual-state proof.
Each capture must be fresh when validated. The first is a consistency baseline;
only the second supplies the unchanged dispatch deadline.
The unique in-app proud-lock marker must carry an explicit visibility boolean,
true or false, which must agree across both captures; missing or invalid evidence
refuses input.
It sends one fixed HID event, then checks once for a valid empty six-digit
keypad while still locked. Output is fixed `confirmed`, `unknown reason=<fixed>`, or refusal
categories. It has no retry, swipe, PIN read, or stop-marker access. This command
sends input; its offline fixtures do not qualify live delivery.

`handkite-probe lock-ui-attrs --config PRIVATE_FILE` is a read-only Cancel-button
diagnostic for an already open locked passcode screen. It requires prior unlock
since boot, an existing WDA session, native and WDA lock confirmation, valid
keypad geometry, and zero-of-six progress. It prints only bounded Cancel counts,
visibility/enabled categories, and an in-screen boolean. It sends no input,
creates no session, and saves or prints no screenshot or XML. Its output does
not qualify a Cancel tap or PIN entry.

`handkite-unlock-once --config /path/to/config.json [--supervised-visibility-fallback]` accepts only the selected device key `phone`. Its stop marker is `Library/Application Support/Handkite/state/iphone/UNLOCK-DISABLED` under the effective user's OS account home, resolved with `getpwuid_r`, independent of `HOME` and the working directory. There is no `--state-dir` override.

The home and each state ancestor must already exist, be owned by that user, and have no group or other write access. Symlinks are rejected, and the final `iphone` directory must be mode 0700. Unlock never creates or migrates these directories. An existing marker blocks execution before PIN input or backend initialization; the bare command cannot clear an earlier attempt's marker. Clear it only after inspecting the device, either yourself or through the [supervised broker](RELEASE.md#pin-unlock).

The explicit `--temporary-idle-settings` variant temporarily uses idle settings
`1.0` and `0.2`, then verifies restoration of the original values. It requires
externally proven exclusive WDA ownership for the complete invocation. Its marker
check precedes stdin and settings mutation. Every backend input revalidates the
transaction session; a completed code submission can clear its own marker only
after verified restoration. Failure never clears a marked gate or retries input.
Do not cancel the bracket. This variant is offline-tested only and does not
qualify live PIN entry; omitting the flag preserves the default unlock path.
Failures report fixed action/restoration categories. An incomplete result can
include only the first unconfirmed position from 1 through 6, never a PIN value
or proof of physical delivery. A marked gate remains disabled on failure.

`--owned-wda-session` additionally requires `--temporary-idle-settings` and
`--supervised-visibility-fallback`. It requires the fixed protected service config
with a runner bundle ID and device key `phone`, and retains `ServiceLock` before
reading stdin or contacting the device. It checks the durable stop gate before
stdin and runner launch, then starts and retains its own XCTest runner on a
continuously driven runtime. Runner, lock, and gate admission run before session
creation, including immediately before its only POST. It accepts only an exact
new receipt ID. Conflicting top-level/nested IDs, reused IDs, uncertain delivery,
or failed status/session validation stop without adoption, renewal, or fallback.

The settings bracket and PIN backend bind to that ID. Every input checks runner
and lock ownership again after asynchronous session validation and through each
socket write and flush poll. The stop gate is not rechecked after this attempt
deliberately marks it. Teardown awaits the input worker and settings restoration,
attempts one identity-checked close for the exact receipt, and aborts and joins
the host runner while retaining the lock. A failed creation supplies no close
target. Only a confirmed code submission with verified restoration, acknowledged
close, retained ownership, and successful runner join may clear this attempt's
marker. Uncertain cleanup preserves it. Close acknowledgement alone does not
prove device-side termination; awaited runner cleanup also requires exact native
process absence and pinned endpoint refusal. External
exclusivity is still required; WDA has no atomic lease, and process termination
can prevent restoration. Existing default modes remain unchanged.

Each live invocation still requires a reviewed handoff from any existing runner
owner and fresh admission checks. Staged-candidate Clock/PIN/lock qualification
does not establish installed-service startup or recovery. Offline fixtures do
not authorize another PIN attempt or clearing an existing marker.

In the session-bound variant, the first snapshot after the first digit tap also
reads the secure field directly once. Both six-digit counts must agree before
another input can proceed. A disagreement reports fixed `progress_mismatch`
action/reason categories, retains the stop marker, and stops without selecting
either count or retrying. Read/session failure also stops. Later snapshots and
default/unbound backends retain their existing behavior; matching sequential
reads do not establish physical delivery or an atomic snapshot.

`--coordinate-pin-taps` is an additional explicit opt-in and requires both
`--temporary-idle-settings` and `--supervised-visibility-fallback`. It selects the
WDA 6+ `/wda/tap` coordinate endpoint for all six digit taps, validating the
exact bound session before each single POST. Key centers and the original
five-second prepared-view write deadline are unchanged. There is no W3C fallback
or retry. Stop-gate ordering, first-digit comparison, and settings restoration
remain in force. The default transport remains W3C. This path is live-unqualified;
deployed endpoint compatibility must first be checked with a separately authorized
harmless Clock coordinate trial. The route follows the
[WDA 6.0.0 source](https://github.com/appium/WebDriverAgent/blob/v6.0.0/WebDriverAgentLib/Commands/FBElementCommands.m#L114);
older routes are not retried. It does not authorize another PIN attempt.

With both `--temporary-idle-settings` and `--supervised-visibility-fallback`,
an explicitly hidden keypad requires fresh locked zero-of-six evidence before one
session-validated wake event. The event retains the prepared view's original
five-second write deadline. After a 1.5-second settle, fresh snapshots must still
prove the pinned session, native/visible/WDA lock, no alert or first-unlock state,
and an empty ready keypad before marking the gate. A genuinely visible empty
keypad needs no wake. There is no swipe or wake/first-digit retry on this path.
This offline change does not establish the cause of the earlier unconfirmed
first digit or qualify live PIN entry; default behavior remains unchanged.

## Optional Apple-account tooling

Apple-account/Anisette tooling is optional and unsupported. It lives in the separate [`crates/handkite-anisette`](crates/handkite-anisette/README.md) crate, which is excluded from the root workspace and is not built by default.

## License

Handkite is licensed under the [MIT License](LICENSE). Third-party components and notices are listed in [THIRD_PARTY.md](THIRD_PARTY.md); Rust dependency licences are checked by `cargo deny check licenses` against [deny.toml](deny.toml).
