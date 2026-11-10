# Anisette deployment

This bundle prepares a separate Anisette V3 sidecar for `anisette.example.invalid`. It has not been deployed. The domain must be registered, the target host chosen, and DNS and certificates established first.

The pinned image digest currently resolves to both `linux/arm64` and
`linux/amd64` manifests. `docker compose config --quiet` accepts this bundle
with an absolute private data-directory value. These checks validate packaging
syntax and registry metadata only; they do not start or qualify the sidecar.

## Boundaries

- `compose.yaml` pins one upstream image digest, selects the macOS ADI backend, binds port 6969 to loopback only, and mounts a persistent private data directory. Do not put the Apple ID, phone PIN, pairing records, or screenshots in this volume or the sidecar environment.
- `Caddyfile` exposes only the three V3 paths over HTTPS and requires a verified client certificate. Place the client CA certificate at `/etc/handkite/anisette-client-ca.pem` on the proxy host. Keep the CA private key and the Mac client's certificate and key outside Git.
- The server's opaque provisioning state and generated headers are sensitive. Container access logs are disabled. Do not enable request or response body logging on the proxy.
- The upstream sidecar is GPL-2.0 and bundles Apple ADI binaries. Keep it as an independently deployed service; review its license and asset terms before redistributing an image.

## Host admission

1. Confirm the host owner, CPU architecture, free storage, existing port 6969 and 80/443 listeners, container runtime, and backup destination. Do not replace an existing Caddy site or container.
2. Choose a domain you control, change the client's fixed origin (`anisette.example.invalid` in this source) to that name, and point it at the admitted host. Verify DNS from the device Mac before requesting a certificate.
3. Create an absolute, service-owned mode-0700 data directory outside the checkout. Set `HANDKITE_ANISETTE_DATA` to that directory for Compose. Back it up without copying it to Git or an agent workspace.
4. Install the Caddy site only after the client CA exists and the proxy configuration validates. The Mac's client key must be readable only by its device-service owner. Keep the upstream listener on loopback.
5. Start the pinned image. On the device Mac, run `handkite-anisette-doctor --cert /path/to/client.pem --key /path/to/client-key.pem`. It requests only `GET /v3/client_info` at the fixed hostname using verified TLS and the private client identity; success prints `anisette_client_info=valid`. Verify separately that a request without a trusted client certificate fails. This is a readiness check, not a signing qualification.
6. Qualify a fresh V3 provisioning and signing flow through the final hostname. The client must forward `X-Apple-I-MD-RINFO` from Apple's finish-provisioning response for the macOS backend. Do not route production signing to this server until that flow succeeds.

## Rollback

Stop the proxy route first, then the sidecar. Preserve the data directory and its exact image digest for diagnosis. Restore the previous pinned image and proxy configuration only after verifying their preimages and the persistent identity. Never delete or re-provision the identity as an automatic retry.
