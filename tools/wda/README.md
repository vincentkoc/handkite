# Build and verify an iOS WDA runner

This is an explicit Xcode-managed route for a Handkite-owned iOS XCTest runner. It is separate from the macOS host release. The [source pin](SOURCE.md) identifies the one upstream revision accepted by the build command.

Use a trusted Mac with Xcode, an exact device UDID, an independently chosen Apple development team, and a clean checkout of the pinned source. Select a new output directory outside both repositories. The command creates that directory once and preserves it on failure.

```sh
python3 tools/wda/build.py \
  --source /absolute/pinned/WebDriverAgent \
  --output /absolute/private/wda-build \
  --team ABCDE12345 \
  --udid 00008120-00123456789A1234 \
  --bundle-id com.example.HandkiteRunner
```

The tool checks the exact source commit, clean worktree, and pinned project hash. It archives that commit into the new output directory and changes only the iOS Runner target's two bundle-ID configurations in the copy. It checks Xcode's resolved target settings before `clean build-for-testing`. It never installs, launches, or runs tests on the phone. The original checkout is not patched. Build output stays in the private directory; `build.log` is mode 0600 and may contain device or account metadata.

Add `--allow-provisioning-updates` only when intentionally allowing Xcode to contact Apple and manage signing assets. The command does not enable automatic device registration. Team and UDID are passed to Xcode as process arguments on the trusted Mac; no PIN, Apple password, or token is accepted by this tool.

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

Neither command installs a runner or proves Xcode account access, profile issuance, on-device signature acceptance, XCTest launch, WDA endpoint ownership, or Handkite input. The existing installed runner and PIN stop marker remain untouched. After independent review, install and launch only on the pinned test phone, then prove the owned no-PIN Clock action before any guarded unlock attempt.
