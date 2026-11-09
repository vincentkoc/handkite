# Building and releasing Handkite

This guide covers building the Mac host yourself, signing your own iOS runner
and host binaries, and installing, upgrading or rolling back a release on your
own Mac, and setting up optional PIN unlock. Each step runs on the Mac that owns
the USB connection, with a phone and signing identity you control.

The release contains exactly three host executables: `handkited` (the device
service), `handkitectl` (local CLI) and `handkite-mcp` (MCP bridge). Every other
binary in the workspace is a separately built operator or diagnostic tool and is
not part of the release inventory.

## Requirements

- An Apple Silicon Mac with Apple's command-line tools selected.
- The Rust toolchain pinned in `rust-toolchain.toml`.
- Your own Apple development team and code-signing certificate.
- A test iPhone with Developer Mode enabled, trusted by the Mac over USB.

## Build the host

Build from a clean, committed checkout with frozen dependencies. Fetch
dependencies once in a separate, reviewed step; the release build itself runs
offline.

Run `python3 tools/check_source_text.py` for the English source/prose check.
Unicode and IME fixtures remain in their original scripts; the Notes benchmark
exception covers expected text values only, not keys or metadata.

```sh
cargo fetch --locked --target aarch64-apple-darwin
export SDKROOT="$(xcrun --sdk macosx --show-sdk-path)"
export MACOSX_DEPLOYMENT_TARGET=11.0
cargo build --frozen --release --target aarch64-apple-darwin \
  --target-dir "$build_dir" --package handkite-host \
  --bin handkited --bin handkitectl --bin handkite-mcp
shasum -a 256 "$build_dir"/aarch64-apple-darwin/release/handkite{d,ctl,-mcp}
```

Use an account-owned, mode-0700 build directory rather than a shared temporary
root. Record the source commit, toolchain and SDK versions and the binary hashes
together; they are the inputs to packaging.

## Sign your own runner

Handkite drives the phone through a WebDriverAgent XCTest runner that you build
and sign with your own team. See [the runner build guide](tools/wda/README.md)
and the [pinned upstream source](tools/wda/SOURCE.md). Configure the installed
runner's bundle ID as `wda_runner_bundle_id` in the protected host config.

Run `tools/wda/verify.py` before installation. It checks the pinned identity,
device coverage, entitlements and the expiration of both the provisioning
profile and signing certificate. Expired assets are refused. Its
`--min-validity-hours` option also rejects assets that will expire inside the
selected window; the result reports hours remaining without account identifiers.
Rebuilding with the same profile does not renew it. A replacement needs its own
verification before installation. A running service alone does not prove that
the next runner start will still have valid signing assets.

Before XCTest starts, the host reads the installed profiles that cover the
runner. Missing, unreadable or incomplete inventory fails with
`ProfileUnavailable`; expired evidence fails with `ProfileExpired`. The host
does not continue after either error. An owner authorization prompt is a
separate startup gate and does not authorize a PIN retry or a Settings change.

## Package and sign the host

`handkite-release-package` signs an existing frozen build with one certificate
you pin by SHA1:

```sh
handkite-release-package --source "$build_dir/aarch64-apple-darwin/release" \
  --output "$package_root" < "$package_input"
```

`package_input` is a mode-0600 file: the certificate SHA1 on the first line,
then JSON describing the build (source commit, target, Rust toolchain, SDK and
the pre-signing size and SHA-256 of each executable). The tool copies the three
executables, signs each with `/usr/bin/codesign`, verifies the result and
writes a canonical `manifest.json` over the signed bytes. It prints
`release_package=inventory` on success. Review the manifest yourself before
trusting its digest; the tool never treats its own output as approval.

An Apple Development signature is not Developer ID distribution or
notarization. If signing fails with `errSecInternalComponent`, check keychain
access in the same login session that runs the signer; an SSH session can see a
different keychain state from the desktop session.

## Release verification diagnostic

`handkite-release-verify --stage ABSOLUTE_PATH` checks a staged release without
installing it. Build it separately:

```sh
cargo build --frozen --package handkite-host --bin handkite-release-verify
```

Redirect stdin from a mode-0600, single-link file containing:

1. the 64-character lowercase hex SHA-256 of the manifest you approved, then LF;
2. the 40-character lowercase hex SHA1 of the signing certificate, then LF;
3. the exact canonical manifest bytes.

| Standard output | Exit | Meaning |
| --- | --- | --- |
| `release_verify=pass` | 0 | The stage matches the manifest and signer. |
| `release_verify=reject` | 1 | The stage or inputs were rejected. |
| `release_verify=invalid` | 2 | Arguments or the input envelope were invalid. |

No paths, pins or manifest contents are printed.

## Release publication command

`handkite-release-publish --stage ABSOLUTE_PATH` takes the same stdin envelope.
Under the installer lock it copies the verified artifacts into a new
digest-named directory under `releases/`, writes a receipt and re-verifies the
pair. It never changes `releases/current` or starts the service.

| Standard output | Exit | Meaning |
| --- | --- | --- |
| `release_publish=published` | 0 | Publication and re-verification succeeded. |
| `release_publish=invalid` | 2 | Arguments or stdin were invalid. |
| `release_publish=reject stage=<stage> ...` | 1 | Nothing was published; residues may remain. |
| `release_publish=unknown stage=<stage> ...` | 3 | The release directory may have changed; inspect it. |

Failures leave files in place for inspection. Do not rerun the command as a
repair step.

## Device agreement diagnostic

`handkite-device-agreement` confirms that the attached phone matches the device
binding in your protected host config. Build it separately:

```sh
cargo build --frozen --package handkite-host --bin handkite-device-agreement
```

`--service` reads the binding from the fixed host config. `--pinned-stdin`
reads an independently recorded binding from a mode-0600 file:

```json
{"schema_version":1,"device_key":"phone","usb_id":"test-device"}
```

It prints `device_agreement=pass` (exit 0) or
`device_agreement=reject stage=<fixed-stage>` (exit 1). Stages name the failed
check (config, installer lock, USB inventory, Lockdown connection or identity
agreement, deadline) and never print device identifiers.

## Install, upgrade and roll back

Releases live side by side under
`~/Library/Application Support/Handkite/releases/`, and `releases/current`
selects one. Selection changes are atomic and journaled, and always run with
the service stopped:

- **First install.** `handkite-select-initial` selects a published release when
  no current selection exists.
- **Upgrade.** `handkite-select-update --operation upgrade` moves `current` from
  the release it names as previous to a different published release.
- **Roll back.** `handkite-select-update --operation rollback` names the older
  release explicitly; it never infers one from history.

Both commands verify the published releases and the attached device before
committing, preserve files on every failure, and report whether the selection
may have been applied. Previous releases are retained, so rolling back is
another explicit selection, not a rebuild.

## Service registration

`handkite-service-stage` renders the launchd descriptor for the fixed
`releases/current/handkited` path, and `handkite-service-register` installs it
as `~/Library/LaunchAgents/com.handkite.host.plist`. Registration does not
start the job; bootstrap it with `launchctl` once you have checked the selected
release and runner.

## PIN unlock

Handkite can enter the device passcode for you, once, under strict guards. It
is off unless you set it up: nothing in the service or the MCP bridge reads,
stores or types a PIN, and no MCP tool can request one.

### What it does

`handkite-unlock-once --config FILE` reads one PIN from stdin, checks that the
phone is the pinned device on its locked passcode screen with an empty keypad,
then types each digit once while re-verifying the session, lock state and
keypad progress between digits. It reports `unlock=code_submitted` or
`unlock=already_unlocked`, or a fixed failure category. Output never contains
the PIN, and an incomplete result reports at most the first unconfirmed digit
position.

`tools/supervised_device.py` is the supervised broker that wraps this one-shot
operator for unattended use on the Mac that owns the phone (Python 3.9 or
newer). It has three fixed modes: `pin-once`, `clock-once` (a harmless
coordinate trial) and `lock-once`. It hands the device over from the stopped
service, runs exactly one operator dispatch, verifies native lock state before
and after, and cleans up the runner.

### Guards

- **No PIN through MCP or the service.** PIN entry is a separate local binary
  that the daemon and MCP bridge never invoke.
- **One-shot entry.** One attempt per invocation; failed or uncertain input is
  never retried or resent.
- **Stop markers.** A marker file blocks every attempt before stdin is read or
  the device is contacted. An attempt marks the gate itself while it runs;
  only a confirmed submission with verified cleanup clears it. Any failure or
  uncertainty leaves PIN entry disabled until you inspect the phone and clear
  the marker yourself.
- **Pinned identity.** The PIN goes only to the device in the protected host
  config, on the expected locked keypad; changed sessions, alerts, unknown lock
  state or mismatched progress stop input.
- **Exclusive ownership.** The broker refuses while the service, another
  controller or the runtime lock is active.
- **Reviewed inputs.** Every executable, config, descriptor and stop marker the
  broker relies on is pinned by hash and file metadata in a profile whose
  digest you review; any change refuses.

### Enable, disable and configure

1. Create the state directory once, owned by you and mode 0700:
   `~/Library/Application Support/Handkite/state/iphone`. Handkite never
   creates or migrates it, and PIN entry refuses without it.
2. To disable PIN entry, create `UNLOCK-DISABLED` in that directory. To
   re-enable it after inspecting the device, remove the marker.
3. Supply the PIN from your own secret source through an anonymous pipe on
   stdin, for example your password manager's CLI:

   ```sh
   your-secret-tool read phone-pin | handkite-unlock-once --config "$config"
   ```

   Never put the PIN in arguments, environment variables, files, the broker
   profile or command output. The broker accepts only an unlinked,
   account-owned pipe as PIN input.
4. For the broker, capture the stop-state receipt into a new private file and
   review it:

   ```sh
   (umask 077; set -C; handkite-unlock-once --stop-state-snapshot > stop-state.json)
   ```

   Then write a version-1 profile (mode 0600) listing the fixed mode,
   `service_state: "stopped"`, the current release selection and the path,
   SHA-256 and metadata fingerprint of each pinned file, and run:

   ```sh
   your-secret-tool read phone-pin | /usr/bin/python3 -B tools/supervised_device.py pin-once \
     --profile-fd 3 --profile-sha256 "$reviewed_profile_sha256" 3< "$profile"
   ```

Exit 0 means confirmed completion, 2 invalid arguments, and 3 that the result
needs review. A lost response, crash or timeout never authorizes running it
again; check the phone's actual state first.

Run the offline broker tests with
`/usr/bin/python3 -B -m unittest discover -s tools -p test_supervised_device.py`.
They use temporary state and fake device outcomes.

## Compact observation presentation

`handkite_observe` accepts optional `{"tree_format":"compact"}`. The capture is
unchanged; only the MCP presentation differs. The returned
`accessibility_tree` is the same XML with the `XCUIElementType` prefix removed
from known role tags and `type` values (`XCUIElementTypeButton` becomes
`Button`); restore the prefix to recover the original roles. Attributes, text,
hierarchy, state and bounds are retained, and metadata and image are identical
to full output.

On success the payload adds `tree_format:"compact"`, `tree_format_version:1`
and `tree_format_reason:"requested"`. Unsupported, malformed or oversized XML
(over 2 MiB, 4096 elements, depth 32 or 64 KiB per value) returns the original
full tree with `tree_format:"full"` and a `tree_format_reason` of
`malformed_xml`, `unsupported_xml` or `limit_exceeded`. There is no partial tree
or silent truncation, and compact output is not guaranteed to be smaller.

## Text injection rate

Text is sent as one string at 240 characters per second by default.
`HANDKITE_TYPING_FREQUENCY` accepts a plain decimal from 60 through 600; missing
or invalid values use 240.

## Emoji keyboard text admission

Prepared text input recognizes the observed Emoji search field in its separate
keyboard container. The complete baseline and rich context must agree on both
fields and the keyboard; the native active element must still be the primary
editor immediately before typing. Search-field focus, changed or extra fields,
unknown layouts, secure fields and dialogs still refuse. Generic key and
current-selection admission are unchanged.
