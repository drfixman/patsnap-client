# patsnap-client

[![crates.io](https://img.shields.io/crates/v/patsnap-client.svg)](https://crates.io/crates/patsnap-client)
[![docs.rs](https://docs.rs/patsnap-client/badge.svg)](https://docs.rs/patsnap-client)

Async Rust client for the [PatSnap Open Platform](https://open.patsnap.com)
patent API — claims, description, bibliography, family, and search — with
API-key auth, request pacing, bounded concurrency, and typed errors. v0
exposes the five basic-data and four search endpoints listed below; the
base URL is `https://connect.patsnap.com`.

## Installation

```toml
[dependencies]
patsnap-client = "0.0.1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

## Usage

```rust
use patsnap::{FamilyFallback, PatsnapClient, PatsnapError};

#[tokio::main]
async fn main() -> Result<(), PatsnapError> {
    let client = PatsnapClient::new(std::env::var("PATSNAP_API_KEY").unwrap(), None);
    let claims = client.claims("US11205304B2", FamilyFallback::No).await?;
    for fragment in &claims.claims {
        for claim in fragment.parsed_claims() {
            println!("{} (parent {:?}): {}", claim.num, claim.parent, claim.text);
        }
    }
    Ok(())
}
```

`claim_text` comes in two shapes. Modern documents carry PatSnap's `<div>`
markup, which `parsed_claims()` turns into numbered, dependent claims. Older
documents come back as OCR'd plain text with no markup (seen live on
`DE3106419A1`, 1981); `parsed_claims()` is empty there, and
`fragment.plain_text()` returns the text of either shape.

## Auth

Every request authenticates with an API key of the form `sk-…`, sent as an
`Authorization: Bearer <key>` header. There is no OAuth flow, no token
endpoint, and no refresh — the key is used as-is on every call.

## Error envelope

Every response body is shaped:

```json
{ "data": ..., "status": true, "error_code": 0, "error_msg": "" }
```

Errors arrive on HTTP 200 with `status: false` — the API never signals
failure through the HTTP status line alone. Callers never need to handle
this envelope directly; the client unwraps it and maps `status: false`
responses to typed errors.

## Endpoints (v1)

GET:

- `/basic-patent-data/claim-data`
- `/basic-patent-data/description-data`
- `/basic-patent-data/bibliography`
- `/basic-patent-data/simple-bibliography`
- `/basic-patent-data/patent-family`

POST:

- `/search/patent/query-search-patent`
- `/search/patent/query-search-count`
- `/search/patent/patent-search-pn`
- `/search/patent/semantic-search-patent`

## Credits

PatSnap bills per successful result — a 100-patent batch costs 100 credits.
The free tier grants 10,000 credits.

## Development

Run the gate:

```
just check
```

Run the live-API smoke tests (spends credits):

```
PATSNAP_API_KEY=sk-… just test-live
```

`just publish-dry` verifies the package builds from the tarball without
uploading.

The wire types follow PatSnap's official reference pages at
`https://open.patsnap.com/devportal/api-reference/<family>/<endpoint>`;
the live tests use the example inputs from those pages.

## License

MIT — see LICENSE.
