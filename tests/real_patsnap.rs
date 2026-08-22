//! Live-API smoke suite against the real PatSnap Open Platform.
//!
//! These spend real API credits and need a real key, so every test is
//! `#[ignore]` - they run only via `just test-live`
//! (`cargo test --test real_patsnap -- --ignored --test-threads=1
//! --nocapture`), never as part of `just check`. They confirm the wire
//! format PatSnap actually returns still matches this crate's types; they
//! are not correctness tests of PatSnap's own data, so assertions stay
//! loose on content and strict on shape.

use patsnap::{
    CountRequest, FamilyFallback, PatentNumberQuery, PatsnapClient, PatsnapError,
    SemanticSearchRequest, query_request,
};

/// PatSnap's real API root - duplicated from [`patsnap::ClientConfig::default`]
/// rather than read off it, since [`get_json`] deliberately bypasses the
/// typed client to fetch the raw wire body.
const BASE_URL: &str = "https://connect.patsnap.com";

/// The example inputs below are the ones PatSnap's own reference pages use,
/// at `https://open.patsnap.com/devportal/api-reference/<family>/<endpoint>`.
///
/// A US publication - the `patent_number` example for
/// `basic-patent-data/bibliography` and `search/patent/patent-search-pn`.
const US_PN: &str = "US11205304B2";

/// A CN publication - the `patent_number` example for
/// `basic-patent-data/claim-data`.
const CN_PN: &str = "CN108977442B";

/// A DE publication - the `patent_number` example for
/// `basic-patent-data/backward-citation`. Germany is the third authority
/// EPO OPS serves no full text for.
const DE_PN: &str = "DE3106419A1";

/// A JP publication - the `patent_number` example for
/// `basic-patent-data/patent-family` and `basic-patent-data/description-data`.
const JP_PN: &str = "JP1999068462A";

/// A publication number no authority has issued - well past the roughly
/// twelve million US utility patents granted to date, but still shaped like
/// a real one so PatSnap treats it as a lookup rather than a malformed
/// parameter.
const BOGUS_PN: &str = "US99999999B2";

/// PatSnap's own Analytics query example, shared by
/// `search/patent/query-search-patent` and `search/patent/query-search-count`.
const QUERY_TEXT: &str = "TACD: virtual reality";

/// PatSnap's own `text` example for `search/patent/semantic-search-patent`.
const SEMANTIC_SEARCH_TEXT: &str = concat!(
    "The invention discloses an automobile front-view based wireless video ",
    "transmission system and method. The system comprises a front-view camera, ",
    "a wireless video transmitting module, a wireless video receiving module, ",
    "a display screen, a display triggering device, a first controller, a ",
    "wireless command transmitting module, a wireless command receiving module, ",
    "a second controller and an automobile starting detecting module, wherein ",
    "the display screen is connected with the wireless video receiving module; ",
    "the front-view camera is connected with the wireless video transmitting ",
    "module; the wireless video transmitting module is wirelessly connected ",
    "with the wireless video receiving module and wirelessly transmits a video ",
    "shot by the front-view camera; and the wireless video receiving module ",
    "receives and sends the video and displays the video on the display ",
    "screen, so that the mounting time of the front-view camera is shortened ",
    "greatly, no damage can be caused to an original automobile, the ",
    "front-view camera can be mounted without a threading manner, and great ",
    "convenience is brought to the owner of the automobile.",
);

/// Confirms `/basic-patent-data/claim-data` parses for the three
/// authorities EPO OPS doesn't serve full text for - US, CN, and DE - which
/// is the whole reason this client exists. Runs all three before panicking
/// once with the full list of failures, mirroring epo-client's
/// `biblio_variants_real`.
///
/// US and CN come back as PatSnap's `<div>` markup and must yield numbered
/// claims. DE3106419A1 (1981) comes back as OCR'd plain text with no
/// markup, so only `plain_text()` is asserted there; `parsed_claims()` is
/// asserted wherever markup is present, so a document that switches shape
/// is reported either way.
#[tokio::test]
#[ignore]
async fn claims_real() {
    init_tracing();
    let client = client();
    let mut failures = Vec::new();

    for pn in [US_PN, CN_PN, DE_PN] {
        let raw = get_json(
            reqwest::Method::GET,
            "/basic-patent-data/claim-data",
            &[
                ("patent_number", pn.to_string()),
                ("replace_by_related", "0".to_string()),
            ],
            None,
        )
        .await;
        println!("claim-data raw for {pn}: {raw:#?}");

        match client.claims(pn, FamilyFallback::No).await {
            Ok(claims) => {
                dump_parsed(&format!("claims({pn})"), &claims);
                let Some(fragment) = claims.claims.first() else {
                    failures.push(format!("{pn}: claims array was empty"));
                    continue;
                };
                if fragment.claim_text.is_empty() {
                    failures.push(format!("{pn}: claim_text was empty"));
                }
                if fragment.plain_text().is_empty() {
                    failures.push(format!("{pn}: plain_text() was empty"));
                }
                if fragment.claim_text.contains("<div") {
                    if fragment.parsed_claims().is_empty() {
                        failures.push(format!(
                            "{pn}: claim_text has <div> markup but parsed_claims() produced no numbered claim"
                        ));
                    }
                } else {
                    println!(
                        "{pn}: claim_text is plain text without <div> markup ({} chars); parsed_claims() is empty for this shape",
                        fragment.claim_text.chars().count()
                    );
                }
            }
            Err(err) => failures.push(format!("{pn}: {err}")),
        }
    }

    assert!(
        failures.is_empty(),
        "claims_real failed for {} of 3 publications:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Confirms `claims_batch` fetches all three publications in one call and
/// returns one `PatentClaims` per requested `pn`, not just the single-`pn`
/// path [`claims_real`] already covers.
#[tokio::test]
#[ignore]
async fn claims_batch_real() {
    init_tracing();
    let client = client();
    let pns = vec![US_PN.to_string(), CN_PN.to_string(), DE_PN.to_string()];

    let results = client
        .claims_batch(&pns, FamilyFallback::No)
        .await
        .expect("claims_batch should succeed for three known-good publications");
    dump_parsed("claims_batch", &results);

    assert_eq!(
        results.len(),
        pns.len(),
        "expected one result per requested pn"
    );
    for claims in &results {
        assert!(!claims.pn.is_empty(), "a result had an empty pn");
        assert!(
            !claims.claims.is_empty(),
            "{}: claims array was empty",
            claims.pn
        );
    }
}

/// Confirms `/basic-patent-data/description-data` parses, using PatSnap's
/// own `patent_number` example for the endpoint.
#[tokio::test]
#[ignore]
async fn description_real() {
    init_tracing();
    let client = client();

    let description = client
        .description(JP_PN, FamilyFallback::No)
        .await
        .expect("description should succeed for a known-good publication");
    dump_parsed("description", &description);

    assert!(!description.pn.is_empty());
    assert!(
        !description.description.is_empty(),
        "description sections were empty"
    );
    assert!(
        description
            .description
            .iter()
            .any(|section| section.text.as_deref().is_some_and(|text| !text.is_empty())),
        "no description section had non-empty text"
    );
}

/// Confirms `/basic-patent-data/bibliography` parses, using PatSnap's own
/// `patent_number` example for the endpoint.
#[tokio::test]
#[ignore]
async fn biblio_real() {
    init_tracing();
    let client = client();

    let biblio = client
        .bibliography(US_PN)
        .await
        .expect("bibliography should succeed for a known-good publication");
    dump_parsed("bibliography", &biblio);

    assert!(!biblio.pn.is_empty());
    assert!(
        !biblio.bibliographic_data.invention_title.is_empty(),
        "invention_title was empty"
    );
    assert!(
        biblio.bibliographic_data.publication_reference.is_some(),
        "publication_reference was missing"
    );
}

/// Confirms `/basic-patent-data/patent-family` parses and flattens
/// correctly out of [`patsnap`]'s private `FamilyWire`/`FamilyGroupsWire`
/// shape, using PatSnap's own `patent_number` example for the endpoint.
#[tokio::test]
#[ignore]
async fn family_real() {
    init_tracing();
    let client = client();

    let family = client
        .family(JP_PN)
        .await
        .expect("family should succeed for a known-good publication");
    dump_parsed("family", &family);

    assert!(!family.pn.is_empty());
    assert!(
        !family.simple_family.is_empty()
            || !family.inpadoc_family.is_empty()
            || !family.patsnap_family.is_empty(),
        "all three family lists were empty"
    );
}

/// Confirms `/search/patent/query-search-patent` parses, using PatSnap's
/// own Analytics query example.
#[tokio::test]
#[ignore]
async fn search_real() {
    init_tracing();
    let client = client();
    let request = query_request(QUERY_TEXT, 10, 0);

    let response = client
        .search(&request)
        .await
        .expect("search should succeed for PatSnap's own example query");
    dump_parsed("search", &response);

    assert!(response.total > 0, "total_search_result_count was zero");
    assert!(!response.results.is_empty(), "results was empty");
    for result in &response.results {
        assert!(!result.pn.is_empty(), "a result had an empty pn");
        assert!(
            !result.patent_id.is_empty(),
            "a result had an empty patent_id"
        );
    }
}

/// Confirms `/search/patent/query-search-count` parses, using the same
/// Analytics query example as [`search_real`].
#[tokio::test]
#[ignore]
async fn search_count_real() {
    init_tracing();
    let client = client();
    let request = CountRequest {
        query_text: QUERY_TEXT.to_string(),
        collapse_by: None,
        collapse_type: None,
        collapse_order: None,
    };

    let total = client
        .search_count(&request)
        .await
        .expect("search_count should succeed for PatSnap's own example query");
    println!("search_count total: {total}");

    assert!(total > 0, "total_search_result_count was zero");
}

/// Confirms `/search/patent/semantic-search-patent` parses, using PatSnap's
/// own example abstract as free-text input.
#[tokio::test]
#[ignore]
async fn semantic_search_real() {
    init_tracing();
    let client = client();
    let request = SemanticSearchRequest {
        text: SEMANTIC_SEARCH_TEXT.to_string(),
        limit: 10,
        offset: 0,
        country: None,
        apd_from: None,
        apd_to: None,
        pbd_from: None,
        pbd_to: None,
        relevancy: None,
    };

    let response = client
        .semantic_search(&request)
        .await
        .expect("semantic_search should succeed for PatSnap's own example abstract");
    dump_parsed("semantic_search", &response);

    assert!(!response.results.is_empty(), "results was empty");
    assert_eq!(
        response.results.len() as u32,
        response.result_count,
        "result_count should match the number of results returned"
    );
}

/// Confirms `/search/patent/patent-search-pn` parses, using PatSnap's own
/// `pn` example for the endpoint.
#[tokio::test]
#[ignore]
async fn search_by_number_real() {
    init_tracing();
    let client = client();
    let number = PatentNumberQuery::Pn(US_PN.to_string());

    let matches = client
        .search_by_number(&number, &[], 10, 0)
        .await
        .expect("search_by_number should succeed for a known-good publication number");
    dump_parsed("search_by_number", &matches);

    assert!(!matches.is_empty(), "no matches for a known-good pn");
    assert!(
        matches.iter().any(|m| !m.pn.is_empty()),
        "every match had an empty pn"
    );
}

/// Confirms an unrecognized publication number maps to
/// `PatsnapError::NotFound`. PatSnap may report this as an empty `data`
/// array on HTTP 200 rather than an HTTP 404 - `claims()` maps both to
/// `NotFound`. The raw dump printed here tells a human running
/// `just test-live` which one PatSnap actually did.
#[tokio::test]
#[ignore]
async fn not_found_real() {
    init_tracing();
    let client = client();

    let raw = get_json(
        reqwest::Method::GET,
        "/basic-patent-data/claim-data",
        &[
            ("patent_number", BOGUS_PN.to_string()),
            ("replace_by_related", "0".to_string()),
        ],
        None,
    )
    .await;
    println!("claim-data raw for bogus pn {BOGUS_PN}: {raw:#?}");

    let err = client
        .claims(BOGUS_PN, FamilyFallback::No)
        .await
        .expect_err("a nonexistent publication number should not resolve to Ok");
    assert!(
        matches!(err, PatsnapError::NotFound),
        "expected NotFound for {BOGUS_PN}, got {err:?}"
    );
}

/// Initializes a `tracing` subscriber once per process, so the client's
/// `#[tracing::instrument]`ed requests and retries show up alongside this
/// suite's own `println!`s under `--nocapture`. Safe to call from every
/// test - a second `try_init` after the first just no-ops.
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

/// Pretty-prints a parsed value under `label`, so its shape can be
/// eyeballed against PatSnap's reference pages in `--nocapture` output.
fn dump_parsed<T: std::fmt::Debug>(label: &str, value: &T) {
    println!("{label}:\n{value:#?}");
}

/// Builds a client against the real PatSnap API using the live key.
fn client() -> PatsnapClient {
    PatsnapClient::new(api_key(), None)
}

/// Fetches `path` directly via a fresh HTTP call - bypassing the typed
/// client entirely - and logs the redacted raw JSON body before parsing, so
/// a wire-format surprise is visible in `--nocapture` output rather than
/// hidden behind the typed client's own `serde` error.
async fn get_json(
    method: reqwest::Method,
    path: &str,
    query: &[(&str, String)],
    body: Option<serde_json::Value>,
) -> serde_json::Value {
    let key = api_key();
    let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
        .expect("api key must be a valid HTTP header value");
    auth.set_sensitive(true);

    let mut req = reqwest::Client::new()
        .request(method.clone(), format!("{BASE_URL}{path}"))
        .header(reqwest::header::AUTHORIZATION, auth);
    if !query.is_empty() {
        req = req.query(query);
    }
    if let Some(ref body) = body {
        req = req.json(body);
    }

    let response = req
        .send()
        .await
        .unwrap_or_else(|err| panic!("{method} {path} failed: {}", redact(&err.to_string(), &key)));
    let status = response.status();
    let text = response.text().await.unwrap_or_else(|err| {
        panic!(
            "{method} {path} failed to read body: {}",
            redact(&err.to_string(), &key)
        )
    });
    println!("{method} {path} [{status}] -> {}", redact(&text, &key));

    serde_json::from_str(&text).unwrap_or_else(|err| {
        panic!(
            "{method} {path} returned invalid JSON: {err}\nbody: {}",
            redact(&text, &key)
        )
    })
}

/// Reads the live API key, panicking with a clear message if it's absent -
/// these tests need real credentials and spend real credits, so a missing
/// key should fail loud rather than silently skip.
fn api_key() -> String {
    std::env::var("PATSNAP_API_KEY").unwrap_or_else(|_| {
        panic!(
            "PATSNAP_API_KEY must be set to run the live PatSnap smoke suite (see `just test-live`)"
        )
    })
}

/// Replaces any occurrence of the live API key with a placeholder, so
/// nothing this suite prints - a raw response body, a panic message - can
/// ever leak it, even if a server error happened to echo request details
/// back.
fn redact(text: &str, key: &str) -> String {
    if key.is_empty() {
        text.to_string()
    } else {
        text.replace(key, "<redacted>")
    }
}
