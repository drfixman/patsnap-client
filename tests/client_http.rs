//! Offline HTTP suite for [`patsnap::PatsnapClient`] against a `wiremock`
//! server - the envelope, retry, batching, and concurrency behavior that
//! `parse_claim_html`'s own inline tests (in `src/claims.rs`) don't reach.
//! No fixture files - every body is an inline `serde_json::json!` literal.

use std::time::Duration;

use patsnap::{
    ClientConfig, CountRequest, FamilyFallback, PatentNumberQuery, PatsnapClient, PatsnapError,
    SemanticSearchRequest, query_request,
};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Pins that `abstract_image` reads the nested `abstract_drawing` object
/// (`num` and `path`), the shape of PatSnap's own example.
#[tokio::test]
async fn abstract_image_happy_path_parses_the_drawing() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/abstract-image"))
        .and(query_param("patent_number", "CN104169186A"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ok_envelope(json!([{
                "pn": "CN104169186A",
                "patent_id": "fdbca633-63df-4c0b-8597-3f6c5e3a2748",
                "abstract_drawing": {
                    "num": "0001",
                    "path": "https://open.patsnap.com/abstract-image/CN/A/10/41/69/18/6/00000001_0001.png?sign=x"
                }
            }]))),
        )
        .mount(&mock)
        .await;

    let image = client_for(&mock)
        .abstract_image("CN104169186A")
        .await
        .expect("mock returns one abstract image");

    let drawing = image.abstract_drawing.expect("drawing present");
    assert_eq!(drawing.num.as_deref(), Some("0001"));
    assert!(drawing.path.unwrap().ends_with(".png?sign=x"));
}

/// Pins that `abstract_image_batch` chunks at PatSnap's 100-per-call cap.
#[tokio::test]
async fn abstract_image_batch_chunks_at_one_hundred() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/abstract-image"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_envelope(json!([]))))
        .mount(&mock)
        .await;

    let pns: Vec<String> = (0..150).map(|n| format!("US{n:07}B2")).collect();
    client_for(&mock)
        .abstract_image_batch(&pns)
        .await
        .expect("mock returns an empty page for each chunk");

    let requests = mock.received_requests().await.unwrap();
    let counts: Vec<usize> = requests.iter().map(patent_number_count).collect();
    assert_eq!(counts, vec![100, 50]);
}

/// Pins that `pdf` sends `replace_by_related` and reads `pdf.path` and
/// `pn_related`.
#[tokio::test]
async fn pdf_happy_path_parses_link_and_substitute() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/pdf-data"))
        .and(query_param("replace_by_related", "1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ok_envelope(json!([{
                "pn": "US11205304B2",
                "pdf": {"path": "https://open.patsnap.com/pdf/watermark/a.pdf?sign=x"},
                "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269",
                "pn_related": "US11205304B1"
            }]))),
        )
        .mount(&mock)
        .await;

    let pdf = client_for(&mock)
        .pdf("US11205304B2", FamilyFallback::Yes)
        .await
        .expect("mock only matches replace_by_related=1");

    assert_eq!(pdf.pn_related.as_deref(), Some("US11205304B1"));
    assert!(pdf.pdf.unwrap().path.unwrap().ends_with(".pdf?sign=x"));
}

/// Pins that `fulltext_images` pages at 100 until PatSnap's total is
/// reached: 150 drawings come back from two requests, offsets 0 and 100.
#[tokio::test]
async fn fulltext_images_pages_until_the_total() {
    let mock = MockServer::start().await;
    for (offset, count) in [(0, 100), (100, 50)] {
        let images: Vec<_> = (offset..offset + count)
            .map(|n| json!({"path": format!("https://data-fulltext-image.patsnap.com/{n}.png"), "image_type": "drawing"}))
            .collect();
        Mock::given(method("POST"))
            .and(path("/basic-patent-data/fulltext-image"))
            .and(body_partial_json(json!({
                "patent_number": "CN116420141A", "limit": 100, "offset": offset
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_envelope(json!({
                "pn": "CN116420141A",
                "patent_id": "fdbca633-63df-4c0b-8597-3f6c5e3a2748",
                "result_count": count,
                "total_search_result_count": 150,
                "fulltext_image": images
            }))))
            .expect(1)
            .mount(&mock)
            .await;
    }

    let drawings = client_for(&mock)
        .fulltext_images("CN116420141A")
        .await
        .expect("mock returns two pages");

    assert_eq!(drawings.images.len(), 150);
    assert_eq!(drawings.images[149].image_type.as_deref(), Some("drawing"));
    assert!(drawings.images[149].path.ends_with("/149.png"));
    mock.verify().await;
}

/// Pins that a missing total does not stop paging after a full page: the
/// short second page is the stop.
#[tokio::test]
async fn fulltext_images_without_a_total_pages_until_a_short_page() {
    let mock = MockServer::start().await;
    for (offset, count) in [(0, 100), (100, 3)] {
        let images: Vec<_> = (offset..offset + count)
            .map(|n| json!({"path": format!("https://data-fulltext-image.patsnap.com/{n}.png"), "image_type": "drawing"}))
            .collect();
        Mock::given(method("POST"))
            .and(path("/basic-patent-data/fulltext-image"))
            .and(body_partial_json(json!({"offset": offset})))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_envelope(json!({
                "pn": "CN116420141A",
                "patent_id": "fdbca633-63df-4c0b-8597-3f6c5e3a2748",
                "fulltext_image": images
            }))))
            .expect(1)
            .mount(&mock)
            .await;
    }

    let drawings = client_for(&mock)
        .fulltext_images("CN116420141A")
        .await
        .expect("mock returns two pages");

    assert_eq!(drawings.images.len(), 103);
    mock.verify().await;
}

/// Pins that a patent without drawings is an empty list, not an error.
#[tokio::test]
async fn fulltext_images_without_drawings_is_empty() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/basic-patent-data/fulltext-image"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_envelope(json!({
            "pn": "CN116420141A",
            "patent_id": "fdbca633-63df-4c0b-8597-3f6c5e3a2748",
            "result_count": 0,
            "total_search_result_count": 0,
            "fulltext_image": []
        }))))
        .expect(1)
        .mount(&mock)
        .await;

    let drawings = client_for(&mock)
        .fulltext_images("CN116420141A")
        .await
        .expect("an empty list is not an error");

    assert!(drawings.images.is_empty());
    mock.verify().await;
}

/// Pins that `download` returns the body and content type, and never sends
/// the API key to the file host.
#[tokio::test]
async fn download_returns_bytes_without_the_api_key() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/abstract-image/0001.png"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"\x89PNG\r\n\x1a\n".to_vec(), "image/png"),
        )
        .mount(&mock)
        .await;

    let file = file_client_for(&mock, vec![])
        .download(&format!("{}/abstract-image/0001.png?sign=x", mock.uri()))
        .await
        .expect("mock serves the file");

    assert_eq!(file.content_type, "image/png");
    assert_eq!(file.bytes, b"\x89PNG\r\n\x1a\n");
    let requests = mock.received_requests().await.unwrap();
    assert!(requests[0].headers.get("authorization").is_none());
}

/// Pins that a link on a host outside `file_hosts` is refused before any
/// request is sent.
#[tokio::test]
async fn download_refuses_a_foreign_host_without_a_request() {
    let mock = MockServer::start().await;

    let err = client_for(&mock)
        .download(&format!("{}/a.png", mock.uri()))
        .await
        .unwrap_err();

    assert!(matches!(err, PatsnapError::Api(_)), "got {err:?}");
    assert_eq!(mock.received_requests().await.unwrap().len(), 0);
}

/// Pins that a redirect is not followed, so only a checked host is ever
/// contacted.
#[tokio::test]
async fn download_does_not_follow_a_redirect() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a.png"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "https://evil.example/a.png"),
        )
        .mount(&mock)
        .await;

    let err = file_client_for(&mock, vec![])
        .download(&format!("{}/a.png", mock.uri()))
        .await
        .unwrap_err();

    assert!(
        matches!(&err, PatsnapError::Api(msg) if msg.starts_with("302")),
        "got {err:?}"
    );
}

/// Pins that a 404 from the file host is `NotFound` and a 500 is retried.
#[tokio::test]
async fn download_maps_404_and_retries_500() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/gone.png"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/flaky.png"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/flaky.png"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"ok".to_vec(), "image/png"))
        .mount(&mock)
        .await;

    let client = file_client_for(&mock, vec![Duration::from_millis(1)]);
    let err = client
        .download(&format!("{}/gone.png", mock.uri()))
        .await
        .unwrap_err();
    assert!(matches!(err, PatsnapError::NotFound), "got {err:?}");

    let file = client
        .download(&format!("{}/flaky.png", mock.uri()))
        .await
        .expect("second attempt succeeds");
    assert_eq!(file.bytes, b"ok");
}

/// Pins that a body over `MAX_FILE_BYTES` fails instead of filling memory.
#[tokio::test]
async fn download_refuses_a_body_over_the_cap() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/huge.pdf"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(vec![0u8; patsnap::MAX_FILE_BYTES + 1], "application/pdf"),
        )
        .mount(&mock)
        .await;

    let err = file_client_for(&mock, vec![])
        .download(&format!("{}/huge.pdf", mock.uri()))
        .await
        .unwrap_err();

    assert!(
        matches!(&err, PatsnapError::Api(msg) if msg.contains("over")),
        "got {err:?}"
    );
}

/// Builds a client whose `file_hosts` accepts the mock's loopback host.
fn file_client_for(mock: &MockServer, retry_backoff: Vec<Duration>) -> PatsnapClient {
    PatsnapClient::with_config(
        "sk-test-key".to_string(),
        ClientConfig {
            base_url: mock.uri(),
            retry_backoff,
            file_hosts: vec!["127.0.0.1".to_string()],
            ..ClientConfig::default()
        },
    )
}

/// Pins that `claims` deserializes every `PatentClaims` field, including a
/// nested `ClaimFragment`, from a realistic `claim-data` body.
#[tokio::test]
async fn claims_happy_path_parses_full_struct() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ok_envelope(json!([{
                "pn": "US11205304B2",
                "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269",
                "pn_related": null,
                "claim_count": 1,
                "claims": [{
                    "lang": "EN",
                    "claim_text": "<div class=\"indep-clm\" num=\"00001\">A widget.</div>",
                    "data_format": "html",
                    "claim_independent_count": 1
                }]
            }]))),
        )
        .mount(&mock)
        .await;

    let claims = client_for(&mock)
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .expect("mock returns one claim record");

    assert_eq!(claims.pn, "US11205304B2");
    assert_eq!(claims.patent_id, "718ead9c-4f3c-4674-8f5a-24e126827269");
    assert_eq!(claims.pn_related, None);
    assert_eq!(claims.claim_count, Some(1));
    assert_eq!(claims.claims.len(), 1);
    assert_eq!(claims.claims[0].lang.as_deref(), Some("EN"));
    assert_eq!(claims.claims[0].claim_independent_count, Some(1));
}

/// Pins that `description` deserializes `PatentDescription`'s localized-text
/// list.
#[tokio::test]
async fn description_happy_path_parses_full_struct() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/description-data"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ok_envelope(json!([{
                "pn": "US11205304B2",
                "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269",
                "pn_related": null,
                "description": [{"lang": "EN", "text": "A gadget.", "data_format": "html"}]
            }]))),
        )
        .mount(&mock)
        .await;

    let description = client_for(&mock)
        .description("US11205304B2", FamilyFallback::No)
        .await
        .expect("mock returns one description record");

    assert_eq!(description.pn, "US11205304B2");
    assert_eq!(description.description.len(), 1);
    assert_eq!(
        description.description[0].text.as_deref(),
        Some("A gadget.")
    );
}

/// Pins that `bibliography` deserializes `BibliographicData`'s parties and
/// classification schemes, not just the top-level fields.
#[tokio::test]
async fn bibliography_happy_path_parses_full_struct() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/bibliography"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ok_envelope(json!([{
                "pn": "US11205304B2",
                "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269",
                "bibliographic_data": {
                    "invention_title": [{"lang": "EN", "text": "Widget", "data_format": null}],
                    "abstracts": [{"lang": "EN", "text": "An abstract.", "data_format": null}],
                    "publication_reference": {
                        "date": 20220101, "kind": "B2", "year": 2022, "month": 1,
                        "country": "US", "doc_number": "11205304"
                    },
                    "application_reference": {
                        "date": 20200101, "kind": "A", "year": 2020, "month": 1,
                        "country": "US", "doc_number": "16123456"
                    },
                    "parties": {
                        "applicants": [{
                            "lang": "EN", "name": "Acme Corp", "address": "1 Main St",
                            "sequence": 1, "data_format": null
                        }],
                        "inventors": [{
                            "lang": "EN", "name": "Jane Doe", "address": null,
                            "sequence": 1, "data_format": null
                        }],
                        "assignees": []
                    },
                    "classification_data": {
                        "classification_cpc": {
                            "main_classification": {
                                "section": "H", "class": 4, "sub_class": "L",
                                "main_group": 12, "sub_group": "00"
                            },
                            "further_classification": []
                        },
                        "classification_ipcr": null
                    }
                }
            }]))),
        )
        .mount(&mock)
        .await;

    let bibliography = client_for(&mock)
        .bibliography("US11205304B2")
        .await
        .expect("mock returns one bibliography record");

    let parties = bibliography
        .bibliographic_data
        .parties
        .as_ref()
        .expect("parties present");
    assert_eq!(parties.applicants[0].name.as_deref(), Some("Acme Corp"));
    assert_eq!(parties.inventors[0].name.as_deref(), Some("Jane Doe"));
    let cpc = bibliography
        .bibliographic_data
        .classification_data
        .as_ref()
        .expect("classification_data present")
        .classification_cpc
        .as_ref()
        .expect("classification_cpc present");
    assert_eq!(
        cpc.main_classification.as_ref().unwrap().section.as_deref(),
        Some("H")
    );
    assert_eq!(
        cpc.main_classification.as_ref().unwrap().class.as_deref(),
        Some("4")
    );
}

/// Pins that `simple_bibliography` deserializes its flatter apno/date and
/// main/further shapes, distinct from full bibliography's.
#[tokio::test]
async fn simple_bibliography_happy_path_parses_full_struct() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/simple-bibliography"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ok_envelope(json!([{
                "pn": "US11205304B2",
                "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269",
                "bibliographic_data": {
                    "invention_title": [{"lang": "EN", "text": "Widget", "data_format": null}],
                    "abstracts": [{"lang": "EN", "text": "An abstract.", "data_format": null}],
                    "publication_reference": {
                        "date": 20220101, "kind": "B2", "year": 2022, "month": 1,
                        "country": "US", "doc_number": "11205304"
                    },
                    "application_reference": {"apno": "US16/123456", "date": 20200101},
                    "parties": null,
                    "classification_data": {
                        "classification_cpc": {"main": "H04L12/00", "further": ["H04L12/24"]},
                        "classification_ipcr": null
                    }
                }
            }]))),
        )
        .mount(&mock)
        .await;

    let bibliography = client_for(&mock)
        .simple_bibliography("US11205304B2")
        .await
        .expect("mock returns one simple bibliography record");

    let application_reference = bibliography
        .bibliographic_data
        .application_reference
        .as_ref()
        .expect("application_reference present");
    assert_eq!(application_reference.apno.as_deref(), Some("US16/123456"));
    let cpc = bibliography
        .bibliographic_data
        .classification_data
        .as_ref()
        .expect("classification_data present")
        .classification_cpc
        .as_ref()
        .expect("classification_cpc present");
    assert_eq!(cpc.main.as_deref(), Some("H04L12/00"));
    assert_eq!(cpc.further, vec!["H04L12/24".to_string()]);
}

/// Pins that `family` flattens `FamilyWire`'s nested `patent_family` object
/// and keeps all three family lists, not just one.
#[tokio::test]
async fn family_happy_path_keeps_all_three_family_lists() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/patent-family"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ok_envelope(json!([{
                "pn": "US11205304B2",
                "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269",
                "patent_family": {
                    "simple_family": [
                        {"kind": "B2", "country": "US", "sequence": 1, "doc_number": "11205304"}
                    ],
                    "inpadoc_family": [
                        {"kind": "A1", "country": "WO", "sequence": 1, "doc_number": "2021123456"}
                    ],
                    "patsnap_family": [
                        {"kind": "B2", "country": "US", "sequence": 1, "doc_number": "11205304"},
                        {"kind": "A1", "country": "CN", "sequence": 2, "doc_number": "106185468"}
                    ],
                    "simple_family_id": 111,
                    "inpadoc_family_id": 222,
                    "patsnap_family_id": 333
                }
            }]))),
        )
        .mount(&mock)
        .await;

    let family = client_for(&mock)
        .family("US11205304B2")
        .await
        .expect("mock returns one family record");

    assert_eq!(family.simple_family.len(), 1);
    assert_eq!(family.inpadoc_family.len(), 1);
    assert_eq!(family.patsnap_family.len(), 2);
    assert_eq!(
        family.simple_family[0].doc_number.as_deref(),
        Some("11205304")
    );
    assert_eq!(family.inpadoc_family[0].country.as_deref(), Some("WO"));
    assert_eq!(
        family.patsnap_family[1].doc_number.as_deref(),
        Some("106185468")
    );
    assert_eq!(family.simple_family_id, Some(111));
    assert_eq!(family.inpadoc_family_id, Some(222));
    assert_eq!(family.patsnap_family_id, Some(333));
}

/// Pins that `search` parses `total_search_result_count`/`result_count`/
/// `results` out of the raw `data` object via `parse_search_response`.
#[tokio::test]
async fn search_happy_path_parses_results() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search/patent/query-search-patent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_envelope(json!({
            "total_search_result_count": 1042,
            "result_count": 2,
            "results": [
                {"pn": "US11205304B2", "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269"},
                {"pn": "CN106185468A", "patent_id": "b053642f-3108-4ea9-b629-420b0ab959e3"}
            ]
        }))))
        .mount(&mock)
        .await;

    let response = client_for(&mock)
        .search(&query_request("TACD: gadget", 10, 0))
        .await
        .expect("mock returns a search page");

    assert_eq!(response.total, 1042);
    assert_eq!(response.result_count, 2);
    assert_eq!(response.results.len(), 2);
    assert_eq!(response.results[0].pn, "US11205304B2");
}

/// Pins that `search_count` reads `total_search_result_count` off
/// `CountWire` and returns the bare `u64`.
#[tokio::test]
async fn search_count_happy_path_returns_total() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search/patent/query-search-count"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ok_envelope(json!({"total_search_result_count": 87}))),
        )
        .mount(&mock)
        .await;

    let request = CountRequest {
        query_text: "TACD: gadget".to_string(),
        collapse_by: None,
        collapse_type: None,
        collapse_order: None,
    };
    let total = client_for(&mock)
        .search_count(&request)
        .await
        .expect("mock returns a count");

    assert_eq!(total, 87);
}

/// Pins that `semantic_search` deserializes the renamed `total` field and
/// the per-result `relevancy` score - the field `search`'s results don't
/// carry.
#[tokio::test]
async fn semantic_search_happy_path_parses_relevancy() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search/patent/semantic-search-patent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_envelope(json!({
            "total_search_result_count": 5,
            "result_count": 1,
            "results": [{
                "pn": "US11205304B2",
                "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269",
                "relevancy": "0.92"
            }]
        }))))
        .mount(&mock)
        .await;

    let request = SemanticSearchRequest {
        text: "a representative abstract".to_string(),
        limit: 10,
        offset: 0,
        country: None,
        apd_from: None,
        apd_to: None,
        pbd_from: None,
        pbd_to: None,
        relevancy: None,
    };
    let response = client_for(&mock)
        .semantic_search(&request)
        .await
        .expect("mock returns a semantic search page");

    assert_eq!(response.total, 5);
    assert_eq!(response.results[0].relevancy.as_deref(), Some("0.92"));
}

/// Pins that `search_by_number` unwraps `PatentSearchPnResultsWire` down to
/// its bare `Vec<PatentNumberMatch>`.
#[tokio::test]
async fn search_by_number_happy_path_parses_matches() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search/patent/patent-search-pn"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_envelope(json!({
            "results": [{
                "pn": "US11205304B2",
                "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269",
                "apno": "US16/123456",
                "apdt": 20200101,
                "pbdt": 20220101,
                "title": "Widget",
                "inventor": "Jane Doe",
                "current_assignee": "Acme Corp",
                "original_assignee": "Acme Corp"
            }]
        }))))
        .mount(&mock)
        .await;

    let matches = client_for(&mock)
        .search_by_number(
            &PatentNumberQuery::Pn("US11205304B2".to_string()),
            &[],
            10,
            0,
        )
        .await
        .expect("mock returns one match");

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].title.as_deref(), Some("Widget"));
    assert_eq!(matches[0].current_assignee.as_deref(), Some("Acme Corp"));
}

/// Pins that a business-level in-band failure (`status: false`, a
/// `68300xxx` code) maps to `PatsnapError::Api` carrying PatSnap's own
/// message, even though the HTTP status is 200.
#[tokio::test]
async fn in_band_business_error_maps_to_api() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(err_envelope(68300004, "Invalid parameter!")),
        )
        .mount(&mock)
        .await;

    let err = client_for(&mock)
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .unwrap_err();

    match err {
        PatsnapError::Api(msg) => assert_eq!(msg, "Invalid parameter!"),
        other => panic!("expected Api, got {other:?}"),
    }
}

/// Pins that an in-band quota failure maps to `PatsnapError::RateLimit` and,
/// critically, is never retried - `expect(1)` fails the test if the client
/// treats an in-band `status: false` like a retryable HTTP failure.
#[tokio::test]
async fn in_band_quota_error_maps_to_rate_limit_and_is_never_retried() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(err_envelope(67200002, "Quota exceeds the limit!")),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let err = client_for(&mock)
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .unwrap_err();

    match err {
        PatsnapError::RateLimit(msg) => assert_eq!(msg, "Quota exceeds the limit!"),
        other => panic!("expected RateLimit, got {other:?}"),
    }
    mock.verify().await;
}

/// Pins that `status: true` with a `null` `data` is treated as a failure
/// (`Api`), not as an empty success.
#[tokio::test]
async fn status_true_with_null_data_maps_to_api() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": null, "status": true, "error_code": 0, "error_msg": ""
        })))
        .mount(&mock)
        .await;

    let err = client_for(&mock)
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .unwrap_err();

    assert!(matches!(err, PatsnapError::Api(_)));
}

/// Pins that `status: true` with an empty `data` list - PatSnap's answer for
/// a publication it does not have, as `not_found_real` shows on the live
/// API - maps to `NotFound`, not to an `Ok` with nothing in it.
#[tokio::test]
async fn empty_result_list_maps_to_not_found() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_envelope(json!([]))))
        .mount(&mock)
        .await;

    let err = client_for(&mock)
        .claims("US99999999B2", FamilyFallback::No)
        .await
        .unwrap_err();

    assert!(matches!(err, PatsnapError::NotFound), "got {err:?}");
}

/// Pins that an in-band failure with no `error_msg` key at all (not just an
/// empty string) still classifies correctly - `error_msg` deserializes via
/// `#[serde(default)]` to `String::new()` when the key is absent.
#[tokio::test]
async fn in_band_error_without_error_msg_still_classifies() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": null, "status": false, "error_code": 67200002
        })))
        .mount(&mock)
        .await;

    let err = client_for(&mock)
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .unwrap_err();

    assert!(matches!(err, PatsnapError::RateLimit(msg) if msg.is_empty()));
}

/// Pins that HTTP 401 maps to `Auth` carrying `"{status}: {body}"` - the
/// leading digits are what a caller classifies on.
#[tokio::test]
async fn http_401_maps_to_auth() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid token"))
        .mount(&mock)
        .await;

    let err = client_for(&mock)
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .unwrap_err();

    assert!(
        matches!(&err, PatsnapError::Auth(msg) if msg.starts_with("401") && msg.contains("invalid token")),
        "got {err:?}"
    );
}

/// Pins that HTTP 403 maps to `Auth`, alongside 401.
#[tokio::test]
async fn http_403_maps_to_auth() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&mock)
        .await;

    let err = client_for(&mock)
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .unwrap_err();

    assert!(
        matches!(&err, PatsnapError::Auth(msg) if msg.starts_with("403")),
        "got {err:?}"
    );
}

/// Pins that a 4xx outside 401/403/404/429 maps to `Api` with the status
/// and body, rather than being fed to the envelope parser and surfacing as
/// "invalid response body" with no status at all.
#[tokio::test]
async fn http_400_maps_to_api_with_leading_status() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&mock)
        .await;

    let err = client_for(&mock)
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .unwrap_err();

    assert!(
        matches!(&err, PatsnapError::Api(msg) if msg.starts_with("400") && msg.contains("bad request")),
        "got {err:?}"
    );
}

/// Pins that HTTP 404 maps to `NotFound` directly, without going through
/// the envelope.
#[tokio::test]
async fn http_404_maps_to_not_found() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock)
        .await;

    let err = client_for(&mock)
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .unwrap_err();

    assert!(matches!(err, PatsnapError::NotFound));
}

/// Pins that a persistent HTTP 500 becomes terminal `Api` once the backoff
/// list is exhausted, after retrying exactly that many times.
#[tokio::test]
async fn http_500_after_backoff_exhausted_maps_to_api() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock)
        .await;

    let client = client_with_backoff(&mock, vec![Duration::from_millis(1)]);
    let err = client
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .unwrap_err();

    assert!(
        matches!(&err, PatsnapError::Api(msg) if msg.starts_with("500")),
        "got {err:?}"
    );
    assert_eq!(mock.received_requests().await.unwrap().len(), 2);
}

/// Pins that a persistent HTTP 429 becomes terminal `RateLimit` once the
/// backoff list is exhausted, after retrying exactly that many times.
#[tokio::test]
async fn http_429_after_backoff_exhausted_maps_to_rate_limit() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&mock)
        .await;

    let client = client_with_backoff(&mock, vec![Duration::from_millis(1)]);
    let err = client
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .unwrap_err();

    assert!(
        matches!(&err, PatsnapError::RateLimit(msg) if msg.starts_with("429")),
        "got {err:?}"
    );
    assert_eq!(mock.received_requests().await.unwrap().len(), 2);
}

/// Pins that a transient HTTP 500 followed by a 200 is recovered by retry -
/// the call succeeds and the mock saw exactly two requests.
#[tokio::test]
async fn retries_a_transient_500_then_succeeds() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ok_envelope(json!([{
                "pn": "US11205304B2",
                "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269",
                "pn_related": null,
                "claim_count": 0,
                "claims": []
            }]))),
        )
        .mount(&mock)
        .await;

    let client = client_with_backoff(&mock, vec![Duration::from_millis(1)]);
    let claims = client
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .expect("second attempt succeeds");

    assert_eq!(claims.pn, "US11205304B2");
    assert_eq!(mock.received_requests().await.unwrap().len(), 2);
}

/// Pins that every request carries `Authorization: Bearer <api_key>` - a
/// mock that only matches with the header present fails closed (404) rather
/// than passing silently if the client stops sending it.
#[tokio::test]
async fn sends_bearer_auth_header() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .and(header("authorization", "Bearer sk-test-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ok_envelope(json!([{
                "pn": "US11205304B2",
                "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269",
                "pn_related": null,
                "claim_count": 0,
                "claims": []
            }]))),
        )
        .mount(&mock)
        .await;

    client_for(&mock)
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .expect("mock only matches with the expected bearer header present");
}

/// Pins that `claims_batch` chunks a 250-patent request at PatSnap's
/// 100-per-call cap into 100/100/50, in three separate requests.
#[tokio::test]
async fn claims_batch_chunks_at_one_hundred() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_envelope(json!([]))))
        .mount(&mock)
        .await;

    let pns: Vec<String> = (0..250).map(|n| format!("US{n:07}B2")).collect();
    client_for(&mock)
        .claims_batch(&pns, FamilyFallback::No)
        .await
        .expect("mock returns an empty page for each chunk");

    let requests = mock.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let counts: Vec<usize> = requests.iter().map(patent_number_count).collect();
    assert_eq!(counts, vec![100, 100, 50]);
}

/// Pins that `claims_batch` issues no request at all for an empty slice,
/// rather than a zero-patent call.
#[tokio::test]
async fn claims_batch_empty_slice_issues_no_request() {
    let mock = MockServer::start().await;

    let claims = client_for(&mock)
        .claims_batch(&[], FamilyFallback::No)
        .await
        .expect("empty input is not an error");

    assert!(claims.is_empty());
    assert_eq!(mock.received_requests().await.unwrap().len(), 0);
}

/// Pins that `FamilyFallback::Yes` sends `replace_by_related=1` and that a
/// response carrying `pn_related` (PatSnap substituted a family member's
/// text) survives onto the parsed struct.
#[tokio::test]
async fn family_fallback_yes_sends_replace_by_related_one() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .and(query_param("replace_by_related", "1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ok_envelope(json!([{
                "pn": "US11205304B2",
                "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269",
                "pn_related": "US11205304B1",
                "claim_count": 0,
                "claims": []
            }]))),
        )
        .mount(&mock)
        .await;

    let claims = client_for(&mock)
        .claims("US11205304B2", FamilyFallback::Yes)
        .await
        .expect("mock only matches replace_by_related=1");

    assert_eq!(claims.pn_related.as_deref(), Some("US11205304B1"));
}

/// Pins that `FamilyFallback::No` sends `replace_by_related=0`, the other
/// half of the parameter pairing with [`family_fallback_yes_sends_replace_by_related_one`].
#[tokio::test]
async fn family_fallback_no_sends_replace_by_related_zero() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .and(query_param("replace_by_related", "0"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ok_envelope(json!([{
                "pn": "US11205304B2",
                "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269",
                "pn_related": null,
                "claim_count": 0,
                "claims": []
            }]))),
        )
        .mount(&mock)
        .await;

    client_for(&mock)
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .expect("mock only matches replace_by_related=0");
}

/// Pins that a client built with a custom `retry_backoff` retries exactly
/// that many times and no more - a longer list than the minimal
/// [`http_500_after_backoff_exhausted_maps_to_api`] case, so the count
/// itself (not just the terminal error type) is under test.
#[tokio::test]
async fn custom_retry_backoff_length_bounds_the_retry_count() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock)
        .await;

    let backoff = vec![
        Duration::from_millis(1),
        Duration::from_millis(1),
        Duration::from_millis(1),
    ];
    let client = client_with_backoff(&mock, backoff);
    client
        .claims("US11205304B2", FamilyFallback::No)
        .await
        .unwrap_err();

    assert_eq!(mock.received_requests().await.unwrap().len(), 4);
}

/// Pins that `request_interval` spaces consecutive requests out. Measured by
/// wall clock, lower bound only, like
/// [`max_concurrent_caps_requests_in_flight`]: a 250-patent batch issues
/// three requests, so two 50ms gaps must separate them (>= 100ms total). An
/// unpaced client finishes the three in a few milliseconds.
#[tokio::test]
async fn request_interval_spaces_consecutive_requests() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_envelope(json!([]))))
        .mount(&mock)
        .await;

    let client = PatsnapClient::with_config(
        "sk-test-key".to_string(),
        ClientConfig {
            base_url: mock.uri(),
            request_interval: Duration::from_millis(50),
            ..ClientConfig::default()
        },
    );

    let pns: Vec<String> = (0..250).map(|n| format!("US{n:07}B2")).collect();
    let started = tokio::time::Instant::now();
    client
        .claims_batch(&pns, FamilyFallback::No)
        .await
        .expect("mock returns an empty page for each chunk");
    let elapsed = started.elapsed();

    assert_eq!(mock.received_requests().await.unwrap().len(), 3);
    assert!(
        elapsed >= Duration::from_millis(100),
        "elapsed {elapsed:?} - three requests at a 50ms interval need two 50ms gaps"
    );
}

/// Pins that `max_concurrent` bounds how many requests are in flight at
/// once. Measured by wall clock rather than a task-entry counter: with a
/// 2-permit cap and four 40ms-delayed calls launched together, the two
/// pairs must serialize into two rounds (~80ms), not run as one (~40ms,
/// uncapped) or four (~160ms, capped at 1). A counter incremented before
/// the client call would count semaphore-queued tasks as "in flight" too
/// and over-report even against a correctly-capped client, since queuing
/// for the 2 permits is exactly what four concurrent callers are expected
/// to do.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn max_concurrent_caps_requests_in_flight() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic-patent-data/claim-data"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ok_envelope(json!([{
                    "pn": "US11205304B2",
                    "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269",
                    "pn_related": null,
                    "claim_count": 0,
                    "claims": []
                }])))
                .set_delay(Duration::from_millis(40)),
        )
        .mount(&mock)
        .await;

    let client = PatsnapClient::with_config(
        "sk-test-key".to_string(),
        ClientConfig {
            base_url: mock.uri(),
            max_concurrent: 2,
            ..ClientConfig::default()
        },
    );

    let started = tokio::time::Instant::now();
    let mut handles = Vec::new();
    for n in 0..4 {
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            let pn = format!("US{n:07}B2");
            client.claims(&pn, FamilyFallback::No).await
        }));
    }
    for handle in handles {
        handle.await.unwrap().unwrap();
    }
    let elapsed = started.elapsed();

    // Only the lower bound is asserted. Without the cap all four calls would
    // overlap and finish in roughly one delay; two rounds means the semaphore
    // held them back. An upper bound would also catch a cap stuck at 1, but a
    // loaded machine can exceed any such ceiling without the client being
    // wrong, which makes it a flake rather than a check.
    assert!(
        elapsed >= Duration::from_millis(70),
        "elapsed {elapsed:?} - four calls at max_concurrent=2 should serialize into two rounds"
    );
}

/// Builds a client pointed at `mock` with default pacing/retry/concurrency.
fn client_for(mock: &MockServer) -> PatsnapClient {
    PatsnapClient::new("sk-test-key".to_string(), Some(mock.uri()))
}

/// Builds a client pointed at `mock` with a caller-chosen `retry_backoff`,
/// so retry-exhaustion tests don't have to wait out the multi-second
/// production defaults.
fn client_with_backoff(mock: &MockServer, retry_backoff: Vec<Duration>) -> PatsnapClient {
    PatsnapClient::with_config(
        "sk-test-key".to_string(),
        ClientConfig {
            base_url: mock.uri(),
            retry_backoff,
            ..ClientConfig::default()
        },
    )
}

/// Wraps a successful envelope around `data` - no `error_msg`, exactly as
/// PatSnap's documented success responses omit it.
fn ok_envelope(data: serde_json::Value) -> serde_json::Value {
    json!({"data": data, "status": true, "error_code": 0})
}

/// Wraps a failed envelope carrying `error_code`/`error_msg`.
fn err_envelope(error_code: i64, error_msg: &str) -> serde_json::Value {
    json!({"data": null, "status": false, "error_code": error_code, "error_msg": error_msg})
}

/// Counts the comma-separated entries in one recorded request's
/// `patent_number` query parameter.
fn patent_number_count(request: &wiremock::Request) -> usize {
    request
        .url
        .query_pairs()
        .find(|(key, _)| key == "patent_number")
        .map(|(_, value)| value.split(',').count())
        .expect("request carries a patent_number query parameter")
}
