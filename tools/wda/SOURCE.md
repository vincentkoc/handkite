# WebDriverAgent source pin

Handkite builds its iOS XCTest runner from [Appium WebDriverAgent v16.15.3](https://github.com/appium/WebDriverAgent/tree/v16.15.3), commit `02cb92f0e93c8b021443bcbb8eeb80932c113220`.

The pinned project is `WebDriverAgent.xcodeproj`; the scheme is `WebDriverAgentRunner`. The pinned `project.pbxproj` has SHA-256 `9a9cbda0e2ac34dac53ddb8253ce7b08ef58b25db44720f3281a47fc760aecff`. These values identify source input; they do not prove a successful local build or signature.

The upstream project is [BSD licensed](https://github.com/appium/WebDriverAgent/blob/v16.15.3/LICENSE). Preserve its notices when distributing a built runner. No upstream source is copied into this repository.

The iOS XCTest runner is separate from Handkite's macOS host executables and their release-signing policy.

The archive-only `initial-lock-state-v1` patch corrects cold-start initialization in `XCUIDevice+FBHelpers.m` and exposes unknown state through `FBCustomCommands.m`. It also adds the nullable getter declaration and `FBHandkiteLockState.h`. The source pin remains unchanged; strict preimage hashes and output hashes are recorded by the builder in `source-patches.json`. Upstream notices remain intact in the archived copy.
