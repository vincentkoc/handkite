# Anisette deployment

This bundle prepares a separate Anisette V3 sidecar for `anisette.example.invalid`. It has not been deployed. The domain must be registered, the target host chosen, and DNS and certificates established first.

The pinned image digest currently resolves to both `linux/arm64` and
`linux/amd64` manifests. GHCR's platform attestations associate those manifests
with [upstream source commit `a62f940`](https://github.com/fdkevin0/anisette-server-rs/commit/a62f94094b6240f2dd79225fb1511f248c5ee6fd)
and its [successful build run](https://github.com/fdkevin0/anisette-server-rs/actions/runs/42250742097).
This is registry-bound provenance, not an independently reproduced build or a
verified image signature. The bundled Apple and third-party ADI binary origins
and redistribution terms remain unresolved. `docker compose config` validates
the bundle syntax without pulling or starting the image.

## Boundaries

- `compose.yaml` pins one upstream image digest, selects the macOS ADI backend, binds port 6969 to loopback only, and mounts a persistent private data directory. It refuses implicit pulls and host-directory creation, and it has no automatic restart during initial qualification. Do not put the Apple ID, phone PIN, pairing records, or screenshots in this volume or the sidecar environment.
- `Caddyfile` exposes only the three V3 paths over HTTPS and requires a verified client certificate. Place the client CA certificate at `/etc/handkite/anisette-client-ca.pem` on the proxy host. Keep the CA private key and the Mac client's certificate and key outside Git.
- The server's opaque provisioning state and generated headers are sensitive. Container access logs are disabled. Do not enable request or response body logging on the proxy.
- The upstream sidecar is GPL-2.0 and bundles Apple ADI binaries. Its [Dockerfile](https://github.com/fdkevin0/anisette-server-rs/blob/a62f94094b6240f2dd79225fb1511f248c5ee6fd/Dockerfile) fetches Apple assets and a third-party ADI binary; its [license inventory](https://github.com/fdkevin0/anisette-server-rs/blob/a62f94094b6240f2dd79225fb1511f248c5ee6fd/docs/licenses.md) records unresolved compatibility. Keep it as an independently deployed service; resolve asset and license terms before redistributing an image.

## Host admission

1. Confirm the host owner, CPU architecture, free storage, existing port 6969 and 80/443 listeners, container runtime, and backup destination. Do not replace an existing Caddy site or container.
2. Choose a domain you control, change the client's fixed origin (`anisette.example.invalid` in this source) to that name, and point it at the admitted host with a **DNS-only** record. [Cloudflare proxying](https://developers.cloudflare.com/dns/proxy-status/use-cases/) terminates the Mac client's TLS connection before Caddy, so the current Caddy origin client-certificate rule requires a direct DNS route. Verify DNS from the device Mac before requesting a publicly trusted origin certificate.
3. Precreate an absolute mode-0700 data directory outside the checkout. The image runs as UID/GID `10001:10001`; verify the directory is writable by that identity under the host's actual user-namespace mapping. Set `HANDKITE_ANISETTE_DATA` to the admitted path. Compose refuses to create a missing bind source. Back up the directory without copying it to Git or an agent workspace.
4. Install the Caddy site only after the client CA exists and the proxy configuration validates. The Mac's client key must be readable only by its device-service owner. Keep the upstream listener on loopback.
5. After reviewing the exact digest and provenance, explicitly pull that digest; the Compose bundle will not pull it. [Upstream startup](https://github.com/fdkevin0/anisette-server-rs/blob/a62f94094b6240f2dd79225fb1511f248c5ee6fd/crates/anisette-server/src/backend.rs) can provision a V1 identity and write `/data` before the V3 listener is ready when state is empty. Inspect the data-directory preimage, then start the container once and observe its state without an automatic restart. Do not treat startup as a passive health check or retry a failed start by inference. On the device Mac, run `handkite-anisette-doctor --cert PRIVATE_CERT --key PRIVATE_KEY`. It requests only `GET /v3/client_info` at the fixed hostname using verified TLS and the private client identity; success prints `anisette_client_info=valid`. Verify separately that a request without a trusted client certificate fails. This is a readiness check.
6. After verifying the target Mac account has no existing Anisette reservation, explicitly run `handkite-anisette-provision-once --cert PRIVATE_CERT --key PRIVATE_KEY` once. The command creates a durable reservation before any Apple or sidecar request. A failure or uncertain result preserves it; do not repeat the command, delete the reservation, or provision another identity by inference. Success prints `anisette_provisioning=ready` without exposing state. The client forwards `X-Apple-I-MD-RINFO` from Apple's finish-provisioning `Response` plist dictionary for the macOS backend.
7. Run `handkite-anisette-doctor --cert PRIVATE_CERT --key PRIVATE_KEY --headers` to load the ready state and request fresh headers. Success prints `anisette_headers=valid` without displaying the headers. This verifies the client-to-sidecar header route, not an authenticated Apple request or a signed runner. Qualify an actual signing consumer separately before routing production signing to this server.

## Rollback

Stop the proxy route first, then the sidecar. Preserve the data directory and its exact image digest for diagnosis. Restore the previous pinned image and proxy configuration only after verifying their preimages and the persistent identity. Never delete or re-provision the identity as an automatic retry. Enable an automatic restart policy only after the initial identity and restart behavior are independently qualified.
