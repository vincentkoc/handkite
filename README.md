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

## Anisette

The [deployment bundle](deploy/anisette/README.md) pins a separate V3 sidecar behind a loopback listener and a TLS client-authenticated proxy. It is staged only. The final domain, host, client certificate, provisioning flow, and signing result must be verified before use.

The host library now validates the V3 provisioning message order, forwards Apple's nonzero routing information in the finish message, binds macOS state to its provider and 16-byte identity, and parses the signing-header response without exposing server error text. This is protocol preparation: there is no connected Anisette client or signing flow yet.
