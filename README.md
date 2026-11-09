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

The CLI's `request` subcommand reads one JSON request from stdin and returns the complete JSON response. An `observe` response contains a screenshot and accessibility tree, so keep its raw output inside the protected local boundary. While holding the fixed process lock, the daemon can reclaim an owned mode-0600 socket left by a crash. It first checks endpoint liveness with a nonblocking datagram connect, then rechecks the socket inode, owner, mode, link count, size, and modification/change timestamps before removing it through the anchored runtime directory. Only `ECONNREFUSED` permits reclamation; a live endpoint, symlink, non-socket, unsafe owner/mode, changed metadata, or uncertain probe result stops startup. A datagram probe creates no stream connection and sends no request. This trial service currently requires a ready WDA on the pinned phone. It does not yet own pairing, WDA startup, signing, PIN entry, or remote-agent authentication.

Before each action, the daemon checks the existing WDA session on the pinned USB route and validates its session endpoint. It adopts that session on the first action and rejects absent, invalid, stale, or replacement sessions before sending input. It never creates or renews a WDA session. WDA ownership and session renewal remain external and unqualified; a replaced session requires an explicit daemon restart after owner inspection.

Run `handkited --check --config /path/to/config.json` for a read-only preflight. It prints the same bounded JSON health flags and native lock state as `handkite-doctor`, with no USB identifier or screen contents. Exit codes are 0 for observation readiness, 1 for failed readiness, and 2 for invalid arguments, configuration, or reporting errors. The check does not bind or remove a socket, start WDA, create a WDA session, or send input. This is a candidate-only readiness check, not signed-install or independent WDA startup proof.

For `act`, `receipt.accepted` is `true` or `false` for a known acknowledgment, or `null` when delivery is unknown. A lock action sends the named Shortcut once, then queries native lock state once even if the WDA acknowledgment was lost. Its `native_lock` field is `locked`, `unlocked`, or `uncertain`; non-lock actions return `null`. Only native `locked` produces `effect: "lock_confirmed"`. This confirms the postcondition, not that the request caused a transition. Opening Shortcuts or a changed screenshot cannot confirm a lock. The optional `after` capture can be absent even when native lock is confirmed.

Lock retains the existing lease, pinned-identity, revision, and unlocked-screen preconditions. Native querying has a five-second command timeout and an eight-second process budget. The complete action has no shared deadline across its WDA steps, so the local client's 65-second read timeout can expire before a result arrives. A timeout leaves delivery and lock state unknown to that caller; inspect fresh native state before deciding what to do. The controller never automatically resends a lock action. This receipt behavior is covered offline; a live Handkite lock cycle remains unqualified.

## Staged Mac service layout

The library's `ServiceLayout::current_account()` derives fixed paths from the effective user's OS account home, independent of `HOME` and the working directory. It accepts no alternate root. All paths below are relative to `Library/Application Support/Handkite` under that home:

| Purpose | Path |
| --- | --- |
| Host configuration | `config/host.json` |
| Versioned signed releases | `releases/` |
| Private runtime socket | `runtime/handkite.sock` |
| Process ownership lock | `runtime/handkite.lock` |
| Durable state | `state/` |
| Staged launchd descriptor | `launchd/com.handkite.host.plist` |

`validate_existing_directories()` requires every listed parent directory to exist. The home, Library, Application Support, Handkite root, and state directory must belong to the effective user and reject group/other write access. Config, releases, runtime, and launchd directories must additionally be mode 0700. Ancestors above the home must belong to root or that user and reject group/other writes. Symlinks and non-directory components fail closed. This metadata preflight is a point-in-time check; a future installer must use anchored directory descriptors to prevent path replacement races and validate each file separately.

`ServiceLock::acquire` validates this layout, opens the runtime directory without following symlinks, and holds a nonblocking exclusive OS lock on a regular, single-link, owner-only mode-0600 file. The lock has one fixed name for the account's pinned-device service; device aliases cannot create separate ownership domains. The file is never truncated or removed, and process exit releases ownership without deleting state. Metadata and inode checks reject replaced locks or runtime directories. This is cooperative process ownership within the protected service account, not isolation from code already running as that account.

Daemon startup now uses the layout and process lock, but no live installation is qualified. Layout validation creates no files; lock acquisition creates only the process lock in an existing runtime directory, and daemon startup binds its fixed socket. No startup step creates directories, config, releases, descriptors, or stop markers. The descriptor is outside launchd's automatically loaded directories. Signed builds, install/rollback commands, and launchd supervision remain pending. The existing `state/iphone/UNLOCK-DISABLED` gate keeps its current path, permission rules, and behavior; layout checks never open or migrate it.

## MCP bridge

`handkite-mcp --socket /path/to/private-dir/handkite.sock --device phone` speaks MCP over stdio. Its default observation capability advertises only `handkite_status` and `handkite_observe`; the latter returns a paired screenshot and accessibility tree from one capture. The device key and socket are fixed when the process starts. Run it as the socket owner.

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

`handkite-doctor --config /path/to/config.json` checks the pinned USB device, WDA status, the existing WDA session, and native lock state. It reports only readiness flags and lock state as JSON; it does not create a WDA session or send device input. Exit code 0 means the locked or unlocked phone can be observed. It does not mean app actions or PIN entry are qualified.

## PIN stop gate

`handkite-unlock-once --config /path/to/config.json [--supervised-visibility-fallback]` accepts only the fixed device key `iphone`. Its stop marker is `Library/Application Support/Handkite/state/iphone/UNLOCK-DISABLED` under the effective user's OS account home, resolved with `getpwuid_r`, independent of `HOME` and the working directory. There is no `--state-dir` override.

The home and each state ancestor must already exist, be owned by that user, and have no group or other write access. Symlinks are rejected, and the final `iphone` directory must be mode 0700. Unlock never creates or migrates these directories. An existing marker blocks execution before PIN input or backend initialization; it requires owner inspection, and this command cannot clear an earlier attempt's marker. The failed supervised trial remains blocked, and Handkite PIN entry is still unqualified.

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
