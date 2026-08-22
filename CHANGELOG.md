# Changelog

All notable changes to this crate are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows
[Semantic Versioning](https://semver.org/).

## [0.0.1] - 2026-08-22

Initial public release.

- Async client for the PatSnap Open Platform (`https://connect.patsnap.com`).
  Basic data: `claims` / `claims_batch` (chunked at PatSnap's 100-per-call
  cap, `FamilyFallback` maps to `replace_by_related`), `description`,
  `bibliography`, `simple_bibliography`, `family` (simple, INPADOC and
  PatSnap families kept side by side). Search: `search` (Analytics query
  syntax, sort + collapse), `search_count`, `semantic_search`,
  `search_by_number` (one `pn` or `apno`, many matches).
- Static API-key auth, sent as `Authorization: Bearer <key>`; no token
  endpoint, no refresh.
- The PatSnap envelope is unwrapped once, in the client: errors arrive on
  HTTP 200 with `status: false`, and the documented platform codes
  (67200001-67200007) map to `RateLimit` / `Auth`, business codes
  (68300004-68300010) to `Api`, all terminal. HTTP 401/403 → `Auth`, 404 →
  `NotFound`, 429 and 5xx retried on a backoff list, connect/timeout faults
  retried the same way, then `Network`. Errors from an HTTP status carry
  `"{status}: {body}"`; every retry is logged at `warn` via `tracing`.
- Typed `PatsnapError`: `Auth` / `NotFound` / `RateLimit` / `Api` / `Network`.
  The error enum and every response struct are `#[non_exhaustive]`; request
  structs are not, so they can be built with struct literals.
- `ClientConfig`: request pacing (`request_interval`), a concurrency
  semaphore (`max_concurrent`), retry backoff, per-request timeout.
- Pre-flight checks on the documented search limits: `query_text` ≤ 1,500
  characters, `limit` ≤ 1,000, `limit + offset` ≤ 20,000 (1,000 for semantic
  search).
- `parse_claim_html` — structured claims (number, parent, independence)
  recovered from PatSnap's `claim_text` markup, no HTML crate; fuzzed by
  `fuzz_claim_parse`. `claim_plain_text` / `ClaimFragment::plain_text` —
  the text of either `claim_text` shape, since older documents (observed:
  `DE3106419A1`, 1981) arrive as OCR'd plain text with no markup and no
  `claim_count`. `query_request` / `parse_search_response` so recorded
  bodies parse without a live client.

[0.0.1]: https://github.com/drfixman/patsnap-client/releases/tag/v0.0.1
