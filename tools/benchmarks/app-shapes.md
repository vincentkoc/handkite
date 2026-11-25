# App-shape coverage

Run on the selected release with one device owner and one persistent MCP client.
Enable `HANDKITE_TIMINGS=1` in both the service and client. Record the installed
source and manifest before a cohort. Setup is separate from measured calls.
Do not restart the service between samples.

For each screen, collect at least eight observations with each of these options:

```json
{"tree_format":"outline","image":"omit","targets":false}
{"tree_format":"outline","image":"omit","targets":true}
```

Alternate the variants, without `since_revision`, so each response includes its
complete requested presentation. Record returned outline bytes, rendered node
count, omitted node counts, target count and tree scope. An outline is lossy:
do not label its byte count as original XML size. When capture timing exposes
source bytes, report those separately. Record XML request time from
`source_request` or `target_source`; distinguish a reused source from a missing
measurement. Keep raw trees, screenshots, target handles and page contents out
of the summary and Git history.

Measure inputs and their following observations separately and as a complete
step. A pending delivery effect is not a verified success: correlate the next
observation's effect update. Never resend input after a refusal or uncertain
result. Record failed attempts separately; do not silently replace them with
successful samples. A bounded additional read can diagnose an unresolved
effect but cannot turn the original attempt into a new input trial.

Report the arithmetic median and nearest-rank p95, sample count, failures and
per-stage timing. The p95 of eight samples is their maximum. Nested or concurrent
stage durations are not additive wall time. Compare against Clock with the same
release, client and response options. A variant that exceeds twice the matching
Clock time or fails needs diagnosis before changing implementation. Keep the
fixture unchanged for its before/after measurement.

| App or shape | Fixture | Allowed interaction |
| --- | --- | --- |
| Clock reference | World Clock and ordinary tabs | Observe; switch tabs without starting a timer or changing entries |
| Safari, short page | `https://example.com` | Observe; follow the public information link and return |
| Safari, long page | `https://en.wikipedia.org/wiki/IPhone` | Observe; follow a public article link; three scrolls, each followed by observation |
| Photos | Existing library or album grid | Browse only; no changes, sharing or deletion |
| App Store | Existing public browse pages | Browse only; no Get, Buy, account or subscription action |
| Notes compose | New disposable note with a unique synthetic title | Type, verify and discard only the note created by this run |
| Maps | Existing map or public place results | Browse and pan only; no saved places or route changes |
| Long native list | A read-only picker or list | Scroll without selecting or changing an entry |

Repeat a three-scroll sequence eight times when the page has enough content.
Record the initial position and scroll geometry, and distinguish scrolling from
a boundary bounce. Address-bar Go is URL navigation; do not submit a web form.
Never sign in, download, answer a permission prompt, change Settings or enter a
PIN. If a prompt appears, capture its category without private text, press Home
through normal guarded navigation, and mark that app blocked. Uncertain delivery
stops its input sequence; it is not permission to retry or bypass admission.

Run summaries and qualified changes belong in `VALIDATION.md`. Keep detailed
private evidence only while the active task needs it.
