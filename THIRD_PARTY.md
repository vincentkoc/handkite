# Third-party components

Handkite itself is MIT licensed; see [LICENSE](LICENSE). This file lists
third-party material that is in, or used by, this repository.

## Rust dependencies

Rust crates are fetched by Cargo and are not vendored. Their licences are
checked with `cargo deny check licenses` against [deny.toml](deny.toml) for the
root workspace and [crates/handkite-anisette/deny.toml](crates/handkite-anisette/deny.toml)
for the optional Anisette workspace. Allowed licences are MIT, Apache-2.0,
Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, ISC, Zlib,
Unicode-3.0, CDLA-Permissive-2.0, Unlicense and 0BSD.

`idevice` (MIT) is taken from <https://github.com/jkcoxson/idevice> at the
revision pinned in the root `Cargo.toml`; it is the only git dependency.

## Appium WebDriverAgent

`tools/wda/build.py` builds an iOS XCTest runner from a pinned upstream
checkout of [Appium WebDriverAgent](https://github.com/appium/WebDriverAgent)
(see [tools/wda/SOURCE.md](tools/wda/SOURCE.md)). The upstream tree is not
vendored, but `build.py` contains short excerpts of upstream source as patch
match strings and replacements. Those excerpts, and any built runner, are
subject to the upstream licence:

```text
BSD License

For WebDriverAgent software

Copyright (c) 2015-present, Facebook, Inc. All rights reserved.

Redistribution and use in source and binary forms, with or without modification,
are permitted provided that the following conditions are met:

 * Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

 * Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

 * Neither the name Facebook nor the names of its contributors may be used to
   endorse or promote products derived from this software without specific
   prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR
ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON
ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

## Optional Anisette tooling

These items belong to the optional, unsupported `crates/handkite-anisette`
crate, which is not built by default.

- `src/anisette_v3/apple/client/apple-root-ca.der` is Apple's public
  "Apple Root CA" certificate (SHA-256
  `B0B1730ECBC7FF4505142C49F1295E6EDA6BCAED7E2C68C5BE91B5A11001F024`),
  embedded as a trust anchor. It is Apple's material, not covered by the
  Handkite licence.
- `deploy/compose.yaml` references, by digest, a container image of
  [anisette-server-rs](https://github.com/fdkevin0/anisette-server-rs), which is
  GPL-2.0 and bundles Apple ADI binaries with unresolved redistribution terms.
  The image is not included in or distributed with this repository; it is run
  only as a separately deployed service. See
  [crates/handkite-anisette/deploy/README.md](crates/handkite-anisette/deploy/README.md).
