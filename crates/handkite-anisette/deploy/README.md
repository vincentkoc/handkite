# Anisette deployment

This bundle prepares a separate Anisette V3 sidecar. It has not been deployed. There is no default hostname: `anisette.example.invalid` in the `Caddyfile` and below is a reserved placeholder. Choose the domain and target host and establish DNS and certificates first, then replace the placeholder in the `Caddyfile` and pass the same origin to every client command with `--origin`.

The pinned image digest currently resolves to both `linux/arm64` and
`linux/amd64` manifests. GHCR's platform attestations associate those manifests
with [upstream source commit `a62f940`](https://github.com/fdkevin0/anisette-server-rs/commit/a62f94094b6240f2dd79225fb1511f248c5ee6fd)
and its [successful build run](https://github.com/fdkevin0/anisette-server-rs/actions/runs/42250742097).
This is registry-bound provenance, not an independently reproduced build or a
verified image signature. The bundled Apple and third-party ADI binary origins
and redistribution terms remain unresolved. `docker compose config` validates
the bundle syntax without pulling or starting the image.

The pinned `linux/amd64` image passed one local `--backend macos --check` run
under amd64 emulation with no network or state mount. That command verifies the
bundled macOS asset and exits before server initialization; it does not qualify
native execution, provisioning, headers, or signing. The exact digests and
receipt are in the repository [`VALIDATION.md`](../../../VALIDATION.md). Repeat the asset check on an admitted native
x86_64 host before the first stateful start, using the same digest and no data
mount; do not treat normal startup as a read-only health check.

## Explicit native image asset check

After admitting a native Linux x86_64 host and its Docker daemon, make the pinned
image available locally through a separately approved pull. From that host, run
one explicit check with Python 3, a Docker CLI supporting platform-qualified
image inspection (Docker 29 or later), and Docker's containerd image store:

```sh
python3 -B crates/handkite-anisette/deploy/check_image.py \
  --context ADMITTED_LOCAL_CONTEXT \
  --expected-daemon-id ADMITTED_DAEMON_ID
```

Supply the independently admitted daemon ID; do not infer the expected identity
from whichever daemon happens to answer this invocation. The named context must
use an existing local Unix socket. Remote contexts and transport/API environment
overrides are refused. The helper resolves that context once, then passes its
fixed Unix endpoint with `--host` to every daemon command. Later context edits
cannot redirect execution or cleanup. Before create, start, and cleanup, it checks
the socket device/inode, expected daemon ID, architecture, and containerd-store
signal; it rechecks the route after cleanup's absence observations. These are
point-in-time checks, not an atomic daemon lease. A socket or daemon replacement
between a check and the following command remains a race; host admission must
exclude concurrent endpoint/daemon replacement. Detected drift stops the operation
and leaves any created container's cleanup `unknown` for explicit recovery.
Both the executing host and daemon must report native Linux amd64. An emulated
amd64 image on an arm64 host does not satisfy this check.

The helper fixes the index and amd64 child digests above, the `anisette-server`
entrypoint, UID/GID `10001:10001`, and `--backend macos --check`. Docker 29's
platform-qualified image `.Id` must equal the child manifest digest. The registry
config digest in the repository [`VALIDATION.md`](../../../VALIDATION.md) remains provenance evidence; it is not substituted
for that local image identity. The created container must report the pinned child
in `ImageManifestDescriptor.digest`; its top-level `.Image` can be a config ID.
The helper requires Docker's `driver-type=io.containerd.snapshotter.v1` signal;
Docker 29 with the legacy image store does not satisfy this contract.

One uniquely named container is created without a pull, network, ports, mounts,
state directory, healthcheck, or restart policy. It has a read-only root,
`no-new-privileges`, all capabilities dropped, one CPU, 256 MiB memory with no
additional swap, and a 32-PID limit. The helper inspects that configuration before
one attached start with a 30-second CLI timeout. The container can continue after
that timeout until exact-owned cleanup removes it. Success requires the exact expected
stdout line, no stderr, exit zero, and `OOMKilled=false`. It then removes only its
verified container ID and verifies the ID and task name are absent. It never
removes an image or prunes Docker resources.

Network isolation applies to the container. The helper forbids implicit image
pulls, but the daemon or a lazy snapshotter may still fetch image layers. It does
not establish host-wide or daemon-wide network isolation.

The single JSON receipt contains fixed status fields, pinned digests, exit/OOM
facts, and generated task identifiers. It suppresses raw Docker output, endpoint,
and daemon identity. Exit zero requires both a successful check and verified
cleanup. An uncertain create or start is never replayed. If cleanup is `unknown`,
use the reported task name and validated ID for independent inspection on the
same admitted daemon; do not rerun the helper or remove other containers by
inference. No receipt file is written. Abrupt process or host loss can still leave
the task container for explicit recovery.

Run the helper's offline regression tests without Docker:

```sh
python3 -B crates/handkite-anisette/deploy/test_check_image.py -fv
```

All subprocess calls in that suite are intercepted by a fake Docker daemon. These
tests qualify command construction, refusal, receipt, and cleanup logic only. The
helper has not yet passed a native image run, and this asset check cannot qualify
ADI execution, Apple requests, provisioning, headers, or signing.

## Boundaries

- `compose.yaml` pins one upstream image digest, selects the macOS ADI backend, binds port 6969 to loopback only, and mounts a persistent private data directory. It refuses implicit pulls and host-directory creation, and it has no automatic restart during initial qualification. Do not put the Apple ID, phone PIN, pairing records, or screenshots in this volume or the sidecar environment.
- `Caddyfile` requires a verified client certificate and admits only query-free `GET /v3/client_info`, `POST /v3/get_headers`, and `GET /v3/provisioning_session` (including WebSocket upgrades). Other methods, paths, and queries receive 404 without reaching the upstream. Place the client CA certificate at `/etc/handkite/anisette-client-ca.pem` on the proxy host. Keep the CA private key and the Mac client's certificate and key outside Git.
- The server's opaque provisioning state and generated headers are sensitive. Container access logs are disabled. Do not enable request or response body logging on the proxy.
- The upstream sidecar is GPL-2.0 and bundles Apple ADI binaries. Its [Dockerfile](https://github.com/fdkevin0/anisette-server-rs/blob/a62f94094b6240f2dd79225fb1511f248c5ee6fd/Dockerfile) fetches Apple assets and a third-party ADI binary; its [license inventory](https://github.com/fdkevin0/anisette-server-rs/blob/a62f94094b6240f2dd79225fb1511f248c5ee6fd/docs/licenses.md) records unresolved compatibility. Keep it as an independently deployed service; resolve asset and license terms before redistributing an image.

The configured macOS backend is only an upstream default: the pinned server accepts
`?backend=android|macos|windows` on each V3 endpoint. The proxy rejects every query,
including a macOS override. [Caddy matcher sets](https://caddyserver.com/docs/caddyfile/matchers)
combine the method, path, empty-query, and exact URI conditions with AND. Caddy's
path matcher normalizes escapes, slashes, and dot segments and ignores case; the
additional URI equality rejects those aliases, a bare `?`, and malformed queries.
The three canonical client operations do not need query parameters.

## Local proxy regression test

With Python 3, OpenSSL, and a verified Caddy binary available, run from the repository root:

```sh
CADDY_BIN=/path/to/caddy python3 -B crates/handkite-anisette/deploy/test_proxy.py -fv
```

The test fails if Caddy is unavailable. It loads the checked-in routing and client-auth
rules into real Caddy, replacing only fixture addresses and certificate paths. It
disables automatic HTTPS and the admin API, binds ephemeral loopback ports, and
generates temporary test identities. A synthetic upstream accepts every method so
an upstream refusal cannot hide a proxy leak. Tests verify allowed requests, blocked
requests with zero upstream receipts, client authentication, and WebSocket frames
in both directions. The fixture stops its own processes and removes its files;
it does not load the Anisette image or contact Apple. See the repository [`VALIDATION.md`](../../../VALIDATION.md) for the
tested Caddy version and binary provenance.

## Host admission

1. Confirm the host owner, CPU architecture, free storage, existing port 6969 and 80/443 listeners, container runtime, and backup destination. Do not replace an existing Caddy site or container.
   Verify the rendered Compose project name is `handkite-anisette` and check existing projects for that identity before starting; command-line or environment overrides can change the default name. Keep its service identity, image, volume, and lifecycle separate from existing applications. Establish a measured CPU, memory, PID, and disk envelope before cohosting; the bundle does not claim resource isolation from a neighboring service.
2. Point the chosen sidecar hostname (shown here as `anisette.example.invalid`) at the admitted host with a **DNS-only** record. [Cloudflare proxying](https://developers.cloudflare.com/dns/proxy-status/use-cases/) terminates the Mac client's TLS connection before Caddy, so the current Caddy origin client-certificate rule requires a direct DNS route. Verify DNS from the device Mac before requesting a publicly trusted origin certificate.
3. Precreate an absolute mode-0700 data directory outside the checkout. The image runs as UID/GID `10001:10001`; verify the directory is writable by that identity under the host's actual user-namespace mapping. Set `HANDKITE_ANISETTE_DATA` to the admitted path. Compose refuses to create a missing bind source. Back up the directory without copying it to Git or an agent workspace.
4. Install the Caddy site only after the client CA exists and the proxy configuration validates. The Mac's client key must be readable only by its device-service owner. Keep the upstream listener on loopback.
   The supplied `reverse_proxy 127.0.0.1:6969` assumes Caddy runs in the host network namespace alongside the published loopback port. If the existing proxy runs in a container or on another host, stop and review the network route instead of using that loopback target. An occupied 80/443 or 6969 listener needs exact owner and interface admission; do not replace it by inference.
5. After reviewing the exact digest and provenance, explicitly pull that digest; the Compose bundle will not pull it. [Upstream startup](https://github.com/fdkevin0/anisette-server-rs/blob/a62f94094b6240f2dd79225fb1511f248c5ee6fd/crates/anisette-server/src/backend.rs) can provision a V1 identity and write `/data` before the V3 listener is ready when state is empty. Inspect the data-directory preimage, then start the container once and observe its state without an automatic restart. Do not treat startup as a passive health check or retry a failed start by inference. On the device Mac, run `handkite-anisette-doctor --origin https://anisette.example.invalid --cert PRIVATE_CERT --key PRIVATE_KEY` with the real origin. It requests only `GET /v3/client_info` at that origin using verified TLS and the private client identity; success prints `anisette_client_info=valid`. Verify separately that a request without a trusted client certificate fails. This is a readiness check.
6. After verifying the target Mac account has no existing Anisette reservation, explicitly run `handkite-anisette-provision-once --origin https://anisette.example.invalid --cert PRIVATE_CERT --key PRIVATE_KEY` once, with the same origin. The command records that origin in a durable reservation before any Apple or sidecar request. A failure or uncertain result preserves it; do not repeat the command, delete the reservation, or provision another identity by inference. Success prints `anisette_provisioning=ready` without exposing state. The client forwards `X-Apple-I-MD-RINFO` from Apple's finish-provisioning `Response` plist dictionary for the macOS backend.
7. Run `handkite-anisette-doctor --origin https://anisette.example.invalid --cert PRIVATE_CERT --key PRIVATE_KEY --headers` to load the ready state and request fresh headers. Success prints `anisette_headers=valid` without displaying the headers. This verifies the client-to-sidecar header route, not an authenticated Apple request or a signed runner. Qualify an actual signing consumer separately before routing production signing to this server.

## Rollback

Stop the proxy route first, then the sidecar. Preserve the data directory and its exact image digest for diagnosis. Restore the previous pinned image and proxy configuration only after verifying their preimages and the persistent identity. Never delete or re-provision the identity as an automatic retry. Enable an automatic restart policy only after the initial identity and restart behavior are independently qualified.
