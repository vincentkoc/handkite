# handkite-anisette

Optional, unsupported Apple-account/Anisette tooling for Handkite.

- This crate is its own Cargo workspace with its own `Cargo.lock`. The root
  workspace excludes it, so `cargo build` at the repository root never compiles
  it, and nothing in `handkite-core` or `handkite-host` depends on it.
- No Anisette server is deployed. Nothing in the Handkite daemon, CLI, or MCP
  bridge uses this code.
- There is no default sidecar host. Every command requires
  `--origin https://HOST[:PORT]`.

Build and test it explicitly from this directory:

```sh
cargo test --offline --locked
```

## Anisette V3 client

The [deployment bundle](deploy/README.md) pins a separate V3 sidecar behind a loopback listener and a TLS client-authenticated proxy. It is staged only. The final domain, host, client certificate, provisioning flow, and signing result must be verified before use.

The library provides a single-origin V3 client for HTTPS client information and signing headers, plus a WSS provisioning conversation. The sidecar origin is required configuration with no default host: every command takes `--origin https://HOST[:PORT]`, and `AnisetteOrigin::parse` accepts only an HTTPS origin with a lowercase DNS hostname and optional port, rejecting IP literals, credentials, paths, queries, and fragments. A missing or invalid origin fails before any identity, state, or network access. Both transports use one explicit rustls configuration with public WebPKI roots, client-certificate authentication, and HTTP/1.1. There are no per-endpoint overrides, environment proxies, redirects, automatic retries, or provider fallbacks.

Load `AnisetteIdentity` from absolute paths to a PEM certificate chain and one matching PEM private key. Each file must be mode 0600, service-owned, and inside a service-owned mode-0700 directory. Symlinks, hard links, empty files, oversized files, and mismatched keys are rejected. The library accepts at most 64 KiB of certificate PEM and 16 KiB of key PEM. Keep this identity outside the checkout.

`AnisetteClient::provision_apple` consumes a durable `PendingProvisioning` reservation. It derives the only identifier from that reservation, fetches one validated sidecar client-info response, and constructs the Apple context internally. Unreserved callback provisioning exists only in synthetic test builds; Apple provider/context construction is private. Each server challenge has a three-second read budget, followed by a fresh three-second budget for the callback and reply. An absolute thirty-second deadline bounds the whole conversation, including connection setup. The callback must not block the async runtime or start detached work. Validated success synchronously publishes the Mac state before polling websocket close. Close failure or cancellation preserves ready state; a restart may load that state, but cannot retry provisioning. `get_headers` binds state to that same identity. Network errors expose only fixed messages. Owned sensitive buffers are cleared where possible; third-party transport and serialization buffers are not a guaranteed memory-erasure boundary.

The Mac-side state store reserves only the effective account's fixed `state/anisette-v3` service directory. An exclusive directory creation is the replay boundary: any existing entry, including empty, malformed, pending, or staging residue, blocks a second reservation. The non-cloneable `PendingProvisioning` handle is returned only after its configured-origin/backend/version attempt record and random 16-byte identifier are synced with the directory and parent. Root-anchored nofollow walks verify ownership, mode, ACL policy, and retained path identity. Files are created exclusively at mode 0600, changed to 0400, and read back before use. Ready state preserves the opaque bytes, uses one fixed exclusive stage and `RENAME_EXCL`, syncs, and revalidates. `load_ready` accepts only a complete valid attempt/ready pair. No cleanup, reset, retry, alternate root, or pending recovery is exposed. Ready state reserved for one origin is rejected when loaded for another, and provisioning refuses a reservation recorded for a different origin. This store is separate from sidecar `/data`; filesystem tests do not prove power-loss recovery. The production provisioning route consumes this reservation even when client-info fetching, Apple discovery, or the WSS exchange fails or is cancelled. It rechecks retained custody before client-info fetching, after that response, after Apple discovery, before WSS connection, and before conversation effects, including after provider awaits. These checks are not atomic with network operations and do not prevent same-account replacement/restoration between checks.

Local TLS fixtures qualify the transport behavior only. No live domain, Apple provisioning, signing result, daemon integration, or deployment is qualified by these tests. See [validation](../../VALIDATION.md).

`handkite-anisette-doctor --origin https://HOST --cert FILE --key FILE` checks only the configured origin's client-info route through verified mutual TLS. `handkite-anisette-doctor --origin https://HOST --cert FILE --key FILE --headers` loads the fixed service account's ready state for that origin, requests fresh headers, discards their values, and reports only `anisette_headers=valid`. It does not reserve or provision. Both modes report no certificate, state, client-info, or header content.

`handkite-anisette-provision-once --origin https://HOST --cert FILE --key FILE` is the explicit provisioning route. It validates the protected client identity before reserving the fixed Mac state location, then consumes that reservation through the Apple and sidecar exchange. Success reports only `anisette_provisioning=ready`; failure reports a fixed category. A failed or uncertain attempt remains reserved and cannot be retried or reset by this command. Do not run it until the final hostname, sidecar, proxy, client identity, and state custody are admitted. A successful header check is not proof of an authenticated Apple request or a signed runner; those consumers remain to be implemented and qualified.

The separate `anisette_v3::apple` module parses supplied lookup XML into unapproved URL candidates. It checks HTTPS, DNS host syntax, port 443, and bounded XML structure without opening a connection. `ReviewedEndpointPolicy::production()` approves only the complete GrandSlam pair `https://gsa.apple.com/grandslam/MidService/startMachineProvisioning` and `https://gsa.apple.com/grandslam/MidService/finishMachineProvisioning`, using the protocol provenance in [validation](../../VALIDATION.md). No hostname suffix grants approval.

The same module encodes start/finish plist requests and parses supplied responses into the existing `StartProvisioningData` and `EndProvisioningData` types. Parsing requires HTTP 200 and signed integer `Response.Status.ec` equal to zero. Finish parsing requires the exact `Response.X-Apple-I-MD-RINFO` plist field containing a positive `u64` integer or a strict decimal string of at most twenty digits. Duplicate dictionary keys are rejected. HTTP response headers do not supply routing information. Responses are capped at 256 KiB and decoded binary fields at 64 KiB each. These pure helpers do not send requests or handle Apple credentials.

The private `AppleProvisioningProvider` implements the callback with a separate rustls/WebPKI HTTPS client that sends no client certificate. It owns exactly approved endpoints and the internally constructed context containing one typed 16-byte identifier and validated sidecar client information. Requests derive lowercase SHA-256 MD-LU and UUID headers from that identifier and include UTC whole-second time, timezone, locale, client-info, and user-agent headers. The content type is `application/x-www-form-urlencoded`, while POST bodies remain unchanged XML plists without form encoding. Start and finish are each consumed before their returned futures run; failure, cancellation, or dropping a future cannot replay a stage. Finish parsing reads RINFO from the response plist and rejects duplicate dictionary keys.

`AnisetteClient::provision_apple` performs fixed-URL lookup and exact endpoint approval before opening WSS, with the same identifier throughout. An explicit call now performs the lookup over HTTPS and rejects any returned pair outside the reviewed policy. Only test builds inject local endpoint routing and trust roots. The Apple client uses HTTPS and HTTP/1.1 only, disables redirects, proxies, retries, connection pooling, and decompression, and has no cookie store or credential input. Disabling the pool also prevents Hyper's independent retry of cancelled requests on reused connections. It accepts at most 64 KiB of lookup XML or 256 KiB of provisioning XML while streaming. The 16 KiB header limit applies after Hyper receives and parses headers; it is not a 16 KiB receive-buffer limit. The callback retains the existing three-second WSS reply budget; real Apple latency and compatibility are unqualified.

## Developer probe (experimental)

The developer probe is optional and experimental. It exercises an undocumented
Apple endpoint, is not part of any supported Handkite workflow, and may be
removed without notice.

`handkite-developer-probe --origin https://HOST --cert PRIVATE_CERT --key PRIVATE_KEY < PRIVATE_TOKEN_FILE`
is an explicit read-only compatibility probe. The input is a service-owned,
mode-0600 regular single-link file with two LF-terminated lines: numeric ADSID
and Xcode GS token. The program uses the fixed ready Anisette identity to fetch
fresh headers, then makes one bounded HTTPS team-listing request to an
undocumented Apple developer endpoint. It prints only a fixed status and never
stores tokens or team data. This endpoint and flow have loopback tests only;
success would establish request compatibility, not signing, profile creation,
or a signed runner. The request must not be run before the final sidecar and
credential custody are qualified.
