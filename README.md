# Handkite

Handkite is a local-first control service for an explicitly selected iPhone. The Mac that owns the USB connection runs the device controller; an authenticated agent bridge will expose observation and guarded actions.

The design specification is maintained outside this repository.

## Local trial

Build `handkited` and `handkitectl` with `cargo build --release -p handkite-host --bins`. Place a JSON config such as `{"device_key":"phone","usb_id":"<PINNED_USB_ID>"}` in a regular mode-0600 file. Create a service-owned mode-0700 socket directory, then run:

```sh
handkited --config /path/to/config.json --socket-dir /path/to/private-dir
handkitectl --socket /path/to/private-dir/handkite.sock observe phone
```

The CLI's `request` subcommand reads one JSON request from stdin and returns the complete JSON response. An `observe` response contains a screenshot and accessibility tree, so keep its raw output inside the protected local boundary. The daemon refuses an existing socket path and does not clear a stale socket automatically; inspect the owner before removing one. This trial service currently requires a ready WDA on the pinned phone. It does not yet own pairing, WDA startup, signing, PIN entry, or remote-agent authentication.

## Read-only MCP bridge

`handkite-mcp --socket /path/to/private-dir/handkite.sock --device phone` speaks MCP over stdio. It advertises only `handkite_status` and `handkite_observe`; the latter returns a paired screenshot and accessibility tree from one capture. The device key is fixed when the process starts, and tool arguments cannot select another phone or request input. Run it as the socket owner. For a remote agent, launch it through an authenticated SSH command to that owner rather than exposing the Unix socket or WDA on a network port.

The MCP bridge has passed local protocol tests and a live read-only capture through the device host's protected socket. It has not yet been connected to the intended OpenClaw agent. Its observation tool returns private screen content to the calling agent. There is no MCP action or PIN tool yet.

## Device doctor

`handkite-doctor --config /path/to/config.json` checks the pinned USB device, WDA status, the existing WDA session, and native lock state. It reports only readiness flags and lock state as JSON; it does not create a WDA session or send device input. Exit code 0 means the locked or unlocked phone can be observed. It does not mean app actions or PIN entry are qualified.

## PIN stop gate

`handkite-unlock-once --config /path/to/config.json [--supervised-visibility-fallback]` accepts only the fixed device key `iphone`. Its stop marker is `Library/Application Support/Handkite/state/iphone/UNLOCK-DISABLED` under the effective user's OS account home, resolved with `getpwuid_r`, independent of `HOME` and the working directory. There is no `--state-dir` override.

The home and each state ancestor must already exist, be owned by that user, and have no group or other write access. Symlinks are rejected, and the final `iphone` directory must be mode 0700. Unlock never creates or migrates these directories. An existing marker blocks execution before PIN input or backend initialization; it requires owner inspection, and this command cannot clear an earlier attempt's marker. The failed supervised trial remains blocked, and Handkite PIN entry is still unqualified.

## Anisette

The [deployment bundle](deploy/anisette/README.md) pins a separate V3 sidecar behind a loopback listener and a TLS client-authenticated proxy. It is staged only. The final domain, host, client certificate, provisioning flow, and signing result must be verified before use.

The host library provides a fixed-origin V3 client for HTTPS client information and signing headers, plus a WSS provisioning conversation. Both transports use one explicit rustls configuration with public WebPKI roots, client-certificate authentication, and HTTP/1.1. There are no endpoint overrides, environment proxies, redirects, automatic retries, or provider fallbacks.

Load `AnisetteIdentity` from absolute paths to a PEM certificate chain and one matching PEM private key. Each file must be mode 0600, service-owned, and inside a service-owned mode-0700 directory. Symlinks, hard links, empty files, oversized files, and mismatched keys are rejected. The library accepts at most 64 KiB of certificate PEM and 16 KiB of key PEM. Keep this identity outside the checkout.

`AnisetteClient::provision` accepts only a 16-byte identity and a typed `ProvisioningDataProvider` for the bounded `spim`, `ptm`, `tk`, and Apple routing-header fields. The callback must finish within the remaining three-second stage deadline; it must not block the async runtime or start detached work. This library does not implement Apple requests or credential handling. A successful conversation returns validated macOS state, which `get_headers` binds to that same identity. Network errors expose only fixed messages. Owned sensitive buffers are cleared where possible; third-party transport and serialization buffers are not a guaranteed memory-erasure boundary.

Local TLS fixtures qualify the transport behavior only. No live domain, Apple provisioning, signing result, daemon integration, or deployment is qualified by these tests. See [validation](VALIDATION.md).
