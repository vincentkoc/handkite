# Build and verify an iOS WDA runner

This is an explicit Xcode-managed route for a Handkite-owned iOS XCTest runner. It is separate from the macOS host release. The [source pin](SOURCE.md) identifies the one fork revision accepted by the build command.

Use a trusted Mac with Xcode, an exact device UDID, an independently chosen Apple development team, and a clean checkout of the pinned source. The build and verification tools support Python 3.9 or later, including Xcode's native `/usr/bin/python3`. Select a new output directory outside both repositories. The command creates that directory once and preserves it on failure.

```sh
python3 tools/wda/build.py \
  --source /absolute/pinned/handkite-wda \
  --output /absolute/private/wda-build \
  --team ABCDE12345 \
  --udid 00008120-00123456789A1234 \
  --bundle-id com.example.HandkiteRunner
```

When the signing Mac does not have the target phone connected, add `--generic-ios-destination`. This explicit opt-in uses the literal `generic/platform=iOS` destination for both the settings probe and `build-for-testing`, following [Appium's generic-device build route](https://github.com/appium/appium-xcuitest-driver/blob/master/docs/getting-started/provisioning-profile/generic-device-config.md). `--udid` remains required and validated as the intended device for subsequent profile/package verification; the generic build does not require that device to be connected. It does not enable provisioning updates, device registration, installation or launch. Without the flag, both commands still target the supplied UDID.

The tool checks the exact source commit, clean worktree, and pinned project hash. It archives that commit into the new output directory and builds only its pinned `packages/webdriveragent` subtree. The `HandkiteRunner` scheme and product come from the fork; the selected bundle ID is supplied through the runner-only `HANDKITE_RUNNER_BUNDLE_ID` setting. No archived source is patched. It checks Xcode's resolved target settings before `clean build-for-testing`. It never installs, launches, or runs tests on the phone. The original checkout is not patched. Build output stays in the private directory; `build.log` is mode 0600 and may contain device or account metadata.

The builder flushes fixed phase messages for source checks, source preparation, settings checks, and build start, each with elapsed seconds. Its final `wda_build` status also includes total elapsed seconds. Xcode still runs synchronously; there are no periodic updates during a phase. Progress messages contain no log contents, paths, or device/account identities. A `completed_unverified` status still requires the separate verifier below.

The tool removes inherited `CPATH` from its Xcode child processes. Host include paths such as `/usr/local/include` are unsafe for an iOS cross-compile; the caller's environment is unchanged.

For compilation qualification on a Mac without signing access, add `--unsigned`. Both Xcode signing flags are disabled, the source receipt records `signing: unsigned`, and the result still reports `completed_unverified`. It cannot pass signed verification or be installed through the reviewed route. Combining this flag with `--allow-provisioning-updates` is rejected before output is created. Build again through the existing qualified Xcode signer to produce an installable candidate.

Add `--allow-provisioning-updates` only when intentionally allowing Xcode to contact Apple and manage signing assets. The command does not enable automatic device registration. Team and UDID are passed to Xcode as process arguments on the trusted Mac; no PIN, Apple password, or token is accepted by this tool.

If Xcode reports `errSecInternalComponent` while signing, inspect keychain access in the same macOS security session that will run the build. A read-only SSH keychain status cannot establish whether the active GUI session can use the private key. Check the GUI session before changing keychain access or retrying signing. An identity appearing in `security find-identity` does not prove its private key is usable.

From a native Terminal in the intended Mac account, run `python3 -B tools/wda/keychain_status.py`. It disables keychain prompts only in that process and reports `locked`, `unlocked`, or `unavailable` without displaying the account or certificate. An `unlocked` result still does not prove that codesign may use a particular private key. A locked result from SSH describes only the SSH security session.

For renewal, reuse the exact retained signer and source route with its independently pinned identity and a fresh output directory. The security session that performs the build must be able to use that exact private key; readiness in another SSH or GUI session does not establish this, and a completed owner wrapper must not be replayed against its existing output.

Rebuilding or re-signing with an existing profile does not extend its validity or the signing certificate's expiry. Extending validity requires a newly issued profile and, when the certificate expires, a newly issued certificate; independently verify the new assets, identity pins, device coverage, entitlements and signed output before a separately guarded installation. This does not authorize Apple account changes or provisioning updates.

After the build, obtain the exact expected runner bundle ID and an independently trusted signing-certificate SHA-1 fingerprint. The verifier requires those pins; it does not infer them from the generated files:

```sh
python3 tools/wda/verify.py \
  --output /absolute/private/wda-build \
  --team ABCDE12345 \
  --udid 00008120-00123456789A1234 \
  --runner-id com.example.HandkiteRunner.xctrunner \
  --certificate-sha1 0000000000000000000000000000000000000000
```

The verifier requires one runner app and one `.xctestrun`, checks the app identifier, verifies the app, test bundle, and nested signatures individually with `codesign`, compares the extracted leaf certificate, and checks the decoded development profile's team, target device, expiry, application ID, certificate, and signed entitlements. Signed claims may be a subset of the profile's allowances; a terminal `.*` allowance can expand to a nonempty qualified name. Other wildcard shapes and nested allowance values fail closed. It emits only SHA-256 hashes of the runner tree and `.xctestrun`. A decoded profile is a diagnostic view; [Apple TN3125](https://developer.apple.com/documentation/technotes/tn3125-inside-code-signing-provisioning-profiles) notes that the DER profile is authoritative on modern iOS. Verification is a point-in-time filesystem check, not a held custody capability.

Neither command installs a runner or proves Xcode account access, profile issuance, on-device signature acceptance, XCTest launch, WDA endpoint ownership, or Handkite input. The existing installed runner and PIN stop marker remain untouched. After independent review, install alongside the retained runner on the pinned test phone. Keep the old runner and protected configuration ready for rollback, then qualify the requested no-PIN action cohorts.

## Fork-owned lock state

The pinned fork owns the initial lock-state correction. Registration success
triggers an immediate read; reads and callbacks serialize the complete
read-and-publish operation. Unknown state produces `lock_state_unavailable`
from `/wda/locked`, rather than a default Boolean.

The builder validates the commit, clean source checkout, project hash and
`HandkiteRunner` scheme hash. `source-provenance.json` records those source
inputs. There are no copied lock-state helpers or source patch strings in the
Handkite builder. The fork's `packages/webdriveragent/HANDKITE.md` describes its
patches and upstream base.

Focused offline checks are `python3 -m unittest discover -s tools/wda -p test_build.py`
and `python3 -m unittest discover -s tools/wda -p test_verify.py`. Run them with
`/usr/bin/python3 -B` to cover native Python 3.9 compatibility. Passing host
fixtures does not qualify Xcode compilation, signing, installation or input.
