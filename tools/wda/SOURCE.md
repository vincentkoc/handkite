# WebDriverAgent source pin

Handkite builds its iOS XCTest runner from the private
[Handkite WDA fork](https://github.com/vincentkoc/handkite-wda/tree/1262085ed9a13ef4e0d0e222cf0a918ecfe4b565/packages/webdriveragent),
commit `1262085ed9a13ef4e0d0e222cf0a918ecfe4b565`, on
`handkite/appium-webdriveragent-17.0.6`. Its upstream base is Appium
`appium-webdriveragent@17.0.6` in the
[appium-ios monorepo](https://github.com/appium/appium-ios/tree/main/packages/webdriveragent).

The builder archives only `packages/webdriveragent`. The project remains
`WebDriverAgent.xcodeproj`; the scheme and product are `HandkiteRunner`.
The pinned project SHA-256 is
`bbc76ffc83962e55e6a0f941e86e71ee978d501b0e29c388579788a48056c3f3`;
the shared scheme SHA-256 is
`73d246bbc783189cc0400a3f93a6f4174a86396d522f92c7c525598292f9daba`.
These identify source inputs, not build or signature qualification.

The fork contains the initial lock-state correction and runner product rename.
The builder makes no source changes: it supplies the owner's bundle ID through
`HANDKITE_RUNNER_BUNDLE_ID` and writes `source-provenance.json`.
See the fork's `packages/webdriveragent/HANDKITE.md` for patch ownership.
Target names and upstream classes remain unchanged.

WebDriverAgent is BSD-3-Clause licensed. Preserve the fork's `LICENSE` and
upstream notices with the built runner; the full terms are reproduced in
[THIRD_PARTY.md](../../THIRD_PARTY.md). The upstream source is not vendored in
this repository.

The iOS XCTest runner is separate from Handkite's macOS host executables and
their release-signing policy. A source re-pin does not install or qualify a
new runner. Retain the previously qualified runner and configuration for the
reviewed upgrade and matched device cohorts.
