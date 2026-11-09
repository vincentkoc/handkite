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
