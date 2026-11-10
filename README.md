# Handkite

Handkite is a local-first control service for an explicitly selected iPhone. The Mac that owns the USB connection runs the device controller; an authenticated agent bridge will expose observation and guarded actions.

The design specification is maintained outside this repository.

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

Build the service candidates using the pinned [Mac release inputs](RELEASE.md). Stage the existing directory layout described below and place a JSON config such as `{"device_key":"phone","usb_id":"<PINNED_USB_ID>"}` in its regular mode-0600 `config/host.json`. With a normal account `HOME`, run:

```sh
handkited
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
Omit it for existing read-only service use; `null` and malformed IDs are rejected.
Keep the actual ID only in the protected host config. The library API
`install_config::load_protected_config` returns the binding and optional ID
through the same anchored reader. `RetainedConfig::wda_runner_bundle_id` exposes
the original retained value; revalidation still requires the exact original
bytes. Neither config API starts WDA or verifies runner installation. The
default daemon mode uses an externally ready WDA; explicit `--owned-wda` mode
requires this runner ID and starts the configured XCTest task and a new session.

The CLI's `request` subcommand reads one JSON request from stdin and returns the complete JSON response. An `observe` response contains a screenshot and accessibility tree, so keep its raw output inside the protected local boundary. While holding the fixed process lock, the daemon can reclaim an owned mode-0600 socket left by a crash. It first checks endpoint liveness with a nonblocking datagram connect, then rechecks the socket inode, owner, mode, link count, size, and modification/change timestamps before removing it through the anchored runtime directory. Only `ECONNREFUSED` permits reclamation; a live endpoint, symlink, non-socket, unsafe owner/mode, changed metadata, or uncertain probe result stops startup. A datagram probe creates no stream connection and sends no request. Default mode requires a ready external WDA. Explicit `--owned-wda` starts a runner and session after protected config and socket checks. Neither mode yet owns pairing, signing, PIN entry, or remote-agent authentication.

Before each action, default mode checks the existing WDA session on the pinned USB route and validates its session endpoint. It adopts that session on the first action and rejects absent, invalid, stale, or replacement sessions. Owned mode instead retains the exact session it created, checks runner liveness and session identity around observation and input, and refuses drift. Neither mode renews a session automatically. A replaced session requires owner inspection.

The separate `wda_runner::OwnedWdaRunner` library can start one exact installed
`.xctrunner` bundle through pinned `idevice` 0.1.68. It requires a callback from
that launch's test plan, a still-live task and plan, and HTTP 200 with boolean
`value.ready: true`. One absolute startup deadline covers discovery, a separately
time-bounded InstallationProxy lookup, and readiness. Startup cancellation and
handle Drop abort the owned host task; `abort_and_join` also observes completion.
Callers must retain the handle inside an active Tokio runtime and establish
exclusive device/WDA ownership before calling it.

Owned daemon mode now uses this module, then creates a fresh exact session.
On adapter Drop it attempts one bounded, identity-checked session DELETE before
aborting the host task. Acknowledgement does not prove endpoint ownership or
device-process termination; abrupt process termination may skip Drop entirely.
The module does not install a runner. It returns fixed error categories, discards
callback logs, and configures no logging subscriber; upstream `idevice` tracing
must stay inside the caller's private boundary. Live startup and shutdown remain
unqualified.

The daemon installs SIGTERM/SIGINT handlers before device startup. Each handler
only sets a shutdown flag. The socket listener stops accepting new work within
its 100 ms idle poll; partial requests have a ten-second absolute read budget
and check the flag between reads. Response writes share one 15-second deadline
and stop if shutdown is requested; a client may therefore receive an incomplete
reply after an action. An already dispatched action is never replayed. Owned
runner startup can be cancelled before session creation. The staged launchd
descriptor allows 180 seconds between SIGTERM and
SIGKILL. These are offline-tested shutdown paths, not proof of a live device exit.

Run `handkited --check --config /path/to/config.json` for a read-only preflight. It prints the same bounded JSON health flags and native lock state as `handkite-doctor`, with no USB identifier or screen contents. Exit codes are 0 for observation readiness, 1 for failed readiness, and 2 for invalid arguments, configuration, or reporting errors. The check does not bind or remove a socket, start WDA, create a WDA session, or send input. This is a candidate-only readiness check, not signed-install or independent WDA startup proof.

`handkited --check-service-layout` checks the effective account's fixed directory
layout and protected host config, including a configured runner ID. Success
prints only `service_layout=ready`. It does not check that the runner is
installed, inspect the phone, acquire the process lock, bind a socket, or start
WDA. This is the read-only preflight for preparing the owned service layout.

The `--check` diagnostic retains its explicit arbitrary-file config route and
existing file/parser checks. Its success does not establish that the fixed
service configuration satisfies the protected startup checks.

For `act`, `receipt.accepted` is `true` or `false` for a known acknowledgment, or `null` when delivery is unknown. A lock action sends the named Shortcut once, then queries native lock state once even if the WDA acknowledgment was lost. Its `native_lock` field is `locked`, `unlocked`, or `uncertain`; non-lock actions return `null`. Only native `locked` produces `effect: "lock_confirmed"`. This confirms the postcondition, not that the request caused a transition. Opening Shortcuts or a changed screenshot cannot confirm a lock. The optional `after` capture can be absent even when native lock is confirmed.

Lock retains the existing lease, pinned-identity, revision, and unlocked-screen preconditions. The action POST cannot send bytes after the earlier of observation freshness and lease expiry. Native querying has a five-second command timeout and an eight-second process budget. The complete result, including post-action capture, has no shared deadline, so the local client's 65-second read timeout can still expire before a result arrives. A timeout leaves delivery and lock state unknown to that caller; inspect fresh native state before deciding what to do. The controller never automatically resends a lock action. This behavior is covered offline; a live Handkite lock cycle remains unqualified.

## Staged Mac service layout

The library's `ServiceLayout::current_account()` derives fixed paths from the effective user's OS account home, independent of `HOME` and the working directory. It accepts no alternate root. All paths below are relative to `Library/Application Support/Handkite` under that home:

| Purpose | Path |
| --- | --- |
| Host configuration | `config/host.json` |
| Versioned signed releases | `releases/` |
| Current release selection | `releases/current` |
| Selected daemon executable | `releases/current/handkited` |
| Private runtime socket | `runtime/handkite.sock` |
| Process ownership lock | `runtime/handkite.lock` |
| Durable state | `state/` |
| Staged launchd descriptor | `launchd/com.handkite.host.plist` |

The pure `launchd_descriptor::render` helper emits the intended supervised
descriptor for this fixed selection path. Its sole daemon argument is
`--owned-wda`, so activation requires a protected installed runner ID. It writes or loads nothing; current
release selection and activation remain unimplemented. See the descriptor
contract and activation preconditions in [Mac release inputs](RELEASE.md).

`validate_existing_directories()` requires every listed parent directory to exist. The home, Library, Application Support, Handkite root, and state directory must belong to the effective user and reject group/other write access. Config, releases, runtime, and launchd directories must additionally be mode 0700. Ancestors above the home must belong to root or that user and reject group/other writes. Symlinks and non-directory components fail closed. This metadata preflight is a point-in-time check; a future installer must use anchored directory descriptors to prevent path replacement races and validate each file separately.

`ServiceLock::acquire` validates this layout, opens the runtime directory without following symlinks, and holds a nonblocking exclusive OS lock on a regular, single-link, owner-only mode-0600 file. The lock has one fixed name for the account's pinned-device service; device aliases cannot create separate ownership domains. The file is never truncated or removed, and process exit releases ownership without deleting state. Metadata and inode checks reject replaced locks or runtime directories. This is cooperative process ownership within the protected service account, not isolation from code already running as that account.

Daemon startup now uses the layout and process lock, but no live installation is qualified. Layout validation creates no files; lock acquisition creates only the process lock in an existing runtime directory, and daemon startup binds its fixed socket. No startup step creates directories, config, releases, descriptors, or stop markers. The descriptor is outside launchd's automatically loaded directories. Signed builds, install/rollback commands, and launchd supervision remain pending. The existing `state/iphone/UNLOCK-DISABLED` gate keeps its current path, permission rules, and behavior; layout checks never open or migrate it.

## Release verification diagnostic

`handkite-release-verify --stage /absolute/private-stage` reads independently
supplied pins and exact canonical manifest bytes from a bounded private regular
stdin file with an empty ACL, then calls the production stage verifier once.
It reports only `release_verify=pass`, `release_verify=reject`, or
`release_verify=invalid`.
See the [input envelope and exit codes](RELEASE.md#release-verification-diagnostic).
This diagnostic is excluded from the three-artifact release inventory. Its tests
do not qualify a real signed stage, installation, or service activation.

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

`handkite-mcp --socket /path/to/private-dir/handkite.sock --device phone` speaks MCP over stdio. Its default observation capability advertises only `handkite_status` and `handkite_observe`; the latter returns a paired screenshot and accessibility tree from one capture. The device key and socket are fixed when the process starts. Run it as the socket owner.

`handkite-mcp --service` derives the device key from the protected fixed host
config and the socket from the effective account's fixed service layout. It does
not accept a caller-selected device or socket in this mode. This is the preferred
command for a restricted SSH principal once the service is installed. It does
not authenticate the caller: bind the exact invocation and capabilities to an
SSH forced command, with forwarding and PTY allocation disabled. The default
capability remains observation only.

The host can explicitly add `--capabilities navigation` or `--capabilities observation,navigation`. Navigation permits only `handkite_acquire_control`, `handkite_release_control`, `handkite_home`, and `handkite_launch_clock`. Home and Clock require an unsigned integer `expected_revision` and reject every other argument. Clock always means `com.apple.mobiletimer`. Navigation alone does not grant either observation tool. There are no generic input, lock, unlock, or PIN tools.

Acquire control explicitly before an action. The process holds the controller's random lease token internally for a fixed two-minute TTL; neither the token nor an owner override is exposed through MCP. Acquire never renews an existing lease. A competing session receives `Busy`. Release is explicit, with best-effort release again on EOF or input/output failure. A killed process relies on expiry. If the acquire response is lost, that process refuses further acquisition: close it and wait two minutes before starting a new session.

Home and Clock return delivery receipt, observed effect, and optional post-action metadata. They do not return screenshots or accessibility content. A lost acknowledgment, unexpected reply, post-input identity mismatch, or oversized response reports unknown delivery with `receipt.accepted: null` and `effect: "unconfirmed"`. No action is automatically replayed. A stale revision requires a fresh authorized observation; an expired lease requires another explicit acquisition.

For remote use, an authenticated SSH key must be bound to a restricted forced command that fixes the socket, device, and capabilities, disables forwarding and PTY allocation, and ignores client-supplied commands. Do not grant that principal an unrestricted shell or direct socket access. A CLI capability flag is host policy, not authentication; SSH access that lets a caller select another command or flags does not enforce capability separation. Keep the Unix socket and WDA off network listeners. Host identity, key enrollment, forced-command restrictions, and the intended OpenClaw connection remain unqualified and require separate setup and acceptance.

The bridge has passed a previous live read-only capture through the device host's protected socket. Navigation is qualified only by offline fixtures; no live navigation or OpenClaw acceptance is claimed. Observation returns private screen content to the calling agent.

An authorized client with both capabilities follows this sequence after MCP initialization:

1. Call `handkite_acquire_control` with `{}`.
2. Call `handkite_status` with `{}` and read its `revision`.
3. Call `handkite_launch_clock` with only `expected_revision` set to that integer.
4. Inspect the receipt and effect. Obtain a fresh status before calling `handkite_home` with its new revision.
5. Call `handkite_release_control` with `{}` when finished.

Use `handkite_observe` only when the agent needs private screen content. A navigation-only client needs a current revision supplied through a separately authorized observation path. A stale or missing revision cannot authorize input; do not guess revisions. After unknown delivery, inspect a fresh observation before deciding on a new action; repeating the earlier request is not recovery proof. The bridge never switches to another device or fallback route.

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

`handkite-probe touch-trial-owned-session --config PRIVATE_FILE --transport coordinate`
(or `w3c`) is the explicit no-PIN qualification route for a newly owned session.
It first proves the unlocked World Clock starting state, then uses the same
creation and settings helper as the opt-in unlock route below. It repeats the
guarded Clock trial on that exact new session and requires verified restoration.
It never reads a PIN or touches the stop marker. This route is offline-tested;
live use is blocked until the baseline daemon that owns the WDA runner is
quiesced through a reviewed handoff with exclusive ownership for the whole call.

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
control flow only; live touch delivery and PIN entry remain unqualified.

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

`handkite-unlock-once --config /path/to/config.json [--supervised-visibility-fallback]` accepts only the fixed device key `iphone`. Its stop marker is `Library/Application Support/Handkite/state/iphone/UNLOCK-DISABLED` under the effective user's OS account home, resolved with `getpwuid_r`, independent of `HOME` and the working directory. There is no `--state-dir` override.

The home and each state ancestor must already exist, be owned by that user, and have no group or other write access. Symlinks are rejected, and the final `iphone` directory must be mode 0700. Unlock never creates or migrates these directories. An existing marker blocks execution before PIN input or backend initialization; it requires owner inspection, and this command cannot clear an earlier attempt's marker. The failed supervised trial remains blocked, and Handkite PIN entry is still unqualified.

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
`--supervised-visibility-fallback`. After gate admission and again checking the
gate immediately before its only session-creation POST, it accepts only an exact
new receipt ID. Conflicting top-level/nested IDs, reused IDs, uncertain delivery,
or failed status/session validation stop without adoption, renewal, or fallback.
The settings bracket and PIN backend bind to that ID; observed session drift
stops further settings writes. The new session is retained for explicit owner
handoff, never automatically deleted. Session ownership still requires external
exclusivity; WDA has no atomic lease, and process termination can prevent settings
restoration. Existing defaults and stop-marker behavior are unchanged.

Live admission remains blocked until the baseline daemon that owns the WDA runner
is quiesced through a reviewed handoff, then the no-PIN Clock route above is
qualified. Offline fixtures do not authorize another PIN attempt or clearing an
existing marker.

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

## Anisette

The [deployment bundle](deploy/anisette/README.md) pins a separate V3 sidecar behind a loopback listener and a TLS client-authenticated proxy. It is staged only. The final domain, host, client certificate, provisioning flow, and signing result must be verified before use.

The host library provides a fixed-origin V3 client for HTTPS client information and signing headers, plus a WSS provisioning conversation. Both transports use one explicit rustls configuration with public WebPKI roots, client-certificate authentication, and HTTP/1.1. There are no endpoint overrides, environment proxies, redirects, automatic retries, or provider fallbacks.

Load `AnisetteIdentity` from absolute paths to a PEM certificate chain and one matching PEM private key. Each file must be mode 0600, service-owned, and inside a service-owned mode-0700 directory. Symlinks, hard links, empty files, oversized files, and mismatched keys are rejected. The library accepts at most 64 KiB of certificate PEM and 16 KiB of key PEM. Keep this identity outside the checkout.

`AnisetteClient::provision` accepts only a 16-byte identity and a typed `ProvisioningDataProvider` for the bounded `spim`, `ptm`, `tk`, and Apple routing-header fields. Each server challenge has a three-second read budget, followed by a fresh three-second budget for the callback and reply. An absolute thirty-second deadline bounds the whole conversation, including connection setup. The callback must not block the async runtime or start detached work. A successful conversation returns validated macOS state, which `get_headers` binds to that same identity. Network errors expose only fixed messages. Owned sensitive buffers are cleared where possible; third-party transport and serialization buffers are not a guaranteed memory-erasure boundary.

Local TLS fixtures qualify the transport behavior only. No live domain, Apple provisioning, signing result, daemon integration, or deployment is qualified by these tests. See [validation](VALIDATION.md).

`handkite-anisette-doctor --cert FILE --key FILE` checks only the fixed hostname's client-info route through verified mutual TLS. It reports no certificate or client-info content. It cannot prove provisioning or signing.

The separate `anisette_v3::apple` module parses supplied lookup XML into unapproved URL candidates. It checks HTTPS, DNS host syntax, port 443, and bounded XML structure without opening a connection. `ReviewedEndpointPolicy::production()` contains no approved destinations: exact Apple start and finish URLs remain unknown. No hostname suffix grants approval.

The same module encodes start/finish plist requests and parses supplied responses into the existing `StartProvisioningData` and `EndProvisioningData` types. Parsing requires HTTP 200 and signed integer `Response.Status.ec` equal to zero. Finish parsing requires exactly one case-insensitive `X-Apple-I-MD-RINFO` header containing a positive decimal `u64`; supply the original header list before duplicate joining or replacement. Responses are capped at 256 KiB, decoded binary fields at 64 KiB each, and the supplied header list at 128 entries and 16 KiB total name/value bytes. These pure helpers do not send requests or handle Apple credentials.

`AppleProvisioningProvider` implements the callback with a separate rustls/WebPKI HTTPS client that sends no client certificate. It accepts only `ApprovedEndpoints` and a context containing one typed 16-byte identifier and validated sidecar client information. Requests derive lowercase SHA-256 MD-LU and UUID headers from that identifier and include UTC whole-second time, timezone, locale, client-info, and user-agent headers. Start and finish are each consumed before their returned futures run; failure, cancellation, or dropping a future cannot replay a stage. Finish parsing preserves all response header values to reject duplicate RINFO.

`AnisetteClient::provision_apple` performs fixed-URL lookup and exact endpoint approval before opening WSS, with the same identifier throughout. Its empty production policy rejects the operation before any network request. Only test builds inject local endpoint routing and trust roots. The Apple client uses HTTPS and HTTP/1.1 only, disables redirects, proxies, retries, connection pooling, and decompression, and has no cookie store or credential input. Disabling the pool also prevents Hyper's independent retry of cancelled requests on reused connections. It accepts at most 64 KiB of lookup XML or 256 KiB of provisioning XML while streaming. The 16 KiB header limit applies after Hyper receives and parses headers; it is not a 16 KiB receive-buffer limit. The callback retains the existing three-second WSS reply budget; real Apple latency and compatibility are unqualified.
