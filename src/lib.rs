//! Async client for the [PatSnap Open Platform](https://open.patsnap.com)
//! patent API.
//!
//! PatSnap exposes patent claims, description, bibliography, family, and
//! search data over a REST API at `https://connect.patsnap.com`.
//!
//! # Quick start
//!
//! ```no_run
//! use patsnap::{FamilyFallback, PatsnapClient, PatsnapError};
//!
//! # async fn run() -> Result<(), PatsnapError> {
//! let client = PatsnapClient::new(std::env::var("PATSNAP_API_KEY").unwrap(), None);
//! let claims = client.claims("US11205304B2", FamilyFallback::No).await?;
//! for fragment in &claims.claims {
//!     for claim in fragment.parsed_claims() {
//!         println!("{} (parent {:?}): {}", claim.num, claim.parent, claim.text);
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Credentials
//!
//! Every request authenticates with an API key of the form `sk-…`, sent as
//! an `Authorization: Bearer <key>` header. There is no OAuth flow, no token
//! endpoint and no refresh — the key is used as-is on every call. PatSnap
//! bills per successful result: a 100-patent [`PatsnapClient::claims_batch`]
//! costs 100 credits.
//!
//! # Error envelope
//!
//! Every response body is shaped
//! `{"data": …, "status": true|false, "error_code": 0, "error_msg": "…"}`.
//! Errors arrive on HTTP 200 with `status: false` — the API never signals
//! failure through the HTTP status line alone. Every endpoint method goes
//! through the private `PatsnapClient::request`, which unwraps the envelope,
//! maps `status: false` to a typed [`PatsnapError`], and handles
//! concurrency, pacing, and retry exactly once. Callers never see the
//! envelope.
//!
//! # Endpoints
//!
//! - [`PatsnapClient::claims`] / [`PatsnapClient::claims_batch`] — claim
//!   text, with [`parse_claim_html`] for numbered, dependent structure.
//! - [`PatsnapClient::description`] — description sections.
//! - [`PatsnapClient::bibliography`] / [`PatsnapClient::simple_bibliography`]
//!   — title, abstract, dates, parties, IPC/CPC classification.
//! - [`PatsnapClient::family`] — simple, INPADOC and PatSnap families.
//! - [`PatsnapClient::search`] / [`PatsnapClient::search_count`] — Analytics
//!   query syntax, with [`query_request`] for the common case.
//! - [`PatsnapClient::semantic_search`] — free-text similarity search.
//! - [`PatsnapClient::search_by_number`] — a publication or application
//!   number resolved to PatSnap's standard patent numbers.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::ser::SerializeMap;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::Instant;
use tracing::{Span, field, info, warn};

mod claims;

pub use claims::{ParsedClaim, claim_plain_text, parse_claim_html};

/// PatSnap caps `claim-data` batches at 100 patents per request; each
/// successful result is billed as one credit.
const CLAIMS_BATCH_LIMIT: usize = 100;

/// PatSnap's documented `query_text` ceiling for both
/// `/search/patent/query-search-patent` and
/// `/search/patent/query-search-count` - 1,500 characters per the official
/// reference pages.
const QUERY_TEXT_MAX_LEN: usize = 1_500;

/// PatSnap's shared `limit` ceiling across the search family.
const SEARCH_LIMIT_MAX: u32 = 1_000;

/// PatSnap's `limit + offset` ceiling for `search` and `search_count`.
const SEARCH_LIMIT_OFFSET_MAX: u32 = 20_000;

/// PatSnap's `limit + offset` ceiling for `semantic_search` - narrower than
/// the rest of the search family.
const SEMANTIC_LIMIT_OFFSET_MAX: u32 = 1_000;

/// Configuration for a [`PatsnapClient`].
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// API root, e.g. `https://connect.patsnap.com`.
    pub base_url: String,
    /// Minimum gap enforced between the starts of consecutive requests -
    /// `Duration::ZERO` disables pacing.
    pub request_interval: Duration,
    /// Backoff delays walked in order on a retryable failure. Exhausting
    /// this list makes the failure terminal.
    pub retry_backoff: Vec<Duration>,
    /// Per-request timeout. PatSnap documents a normal response time of
    /// 5000ms and a maximum of 10000ms, so 20s leaves headroom above the
    /// documented worst case.
    pub timeout: Duration,
    /// Maximum requests in flight at once. Clamped to at least 1.
    pub max_concurrent: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            base_url: "https://connect.patsnap.com".to_string(),
            request_interval: Duration::ZERO,
            retry_backoff: vec![
                Duration::from_millis(250),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ],
            timeout: Duration::from_secs(20),
            max_concurrent: 8,
        }
    }
}

/// A typed failure from a PatSnap call.
///
/// PatSnap reports most errors on HTTP 200 with `status: false` in the
/// response body, not via HTTP status codes - see the crate-level notes on
/// the error envelope. An error that came from an HTTP status rather than
/// the envelope carries
/// `"{status}: {body}"`, so callers can classify by the leading digits; an
/// envelope error carries PatSnap's own `error_msg`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PatsnapError {
    /// Bad, missing, or expired credentials, or an exhausted balance. HTTP
    /// 401/403, or envelope codes 67200003/67200005/67200006.
    Auth(String),
    /// HTTP 404, or an endpoint that returned an empty result list for the
    /// requested publication.
    NotFound,
    /// Call quota or rate limit exhausted, after retries. Carries the
    /// server's own wording because PatSnap merges two different problems
    /// into error code 67200004 - "No permission or API package quota has
    /// exceeded the limit" - and only the message says which one it was.
    RateLimit(String),
    /// A business-level API failure (bad parameters, service error), a 5xx
    /// after retries, any other non-success HTTP status, or a body that is
    /// not a PatSnap envelope.
    Api(String),
    /// A transport failure (connect, timeout, TLS), after retries.
    Network(String),
}

impl fmt::Display for PatsnapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatsnapError::Auth(msg) => write!(f, "PatSnap authentication error: {msg}"),
            PatsnapError::NotFound => write!(f, "PatSnap resource not found"),
            PatsnapError::RateLimit(msg) => write!(f, "PatSnap rate limit exceeded: {msg}"),
            PatsnapError::Api(msg) => write!(f, "PatSnap API error: {msg}"),
            PatsnapError::Network(msg) => write!(f, "PatSnap network error: {msg}"),
        }
    }
}

impl std::error::Error for PatsnapError {}

/// The envelope every PatSnap response is wrapped in, success or failure -
/// `{"data": ..., "status": true|false, "error_code": 0, "error_msg": "..."}`.
#[derive(Deserialize)]
struct Envelope<T> {
    data: Option<T>,
    status: bool,
    error_code: i64,
    /// Present only on failures - PatSnap's success responses omit it.
    #[serde(default)]
    error_msg: String,
}

/// Whether a lookup should fall back to a family member's text when the
/// requested publication's own is unavailable - PatSnap's
/// `replace_by_related` query parameter. Applies to claim-data and
/// description-data only; the caller decides, there is no crate-wide
/// default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FamilyFallback {
    No,
    Yes,
}

impl FamilyFallback {
    fn query_value(self) -> &'static str {
        match self {
            FamilyFallback::No => "0",
            FamilyFallback::Yes => "1",
        }
    }
}

/// A localized text fragment - the shared wire shape behind bibliography
/// titles/abstracts and description sections (`{lang, text, data_format}`).
/// The simple-bibliography title/abstract variants omit `data_format` on
/// the wire; that just deserializes to `None`.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct LocalizedText {
    pub lang: Option<String>,
    pub text: Option<String>,
    pub data_format: Option<String>,
}

/// One `claim_text` body plus its claim-level metadata, as returned by
/// `/basic-patent-data/claim-data`.
///
/// `claim_text` comes in two shapes. Modern documents carry PatSnap's
/// `<div class="indep-clm" num="1">` markup, which [`Self::parsed_claims`]
/// turns into numbered claims. Older documents carry OCR'd plain text with
/// no markup (observed live: `DE3106419A1`, 1981), where
/// [`Self::parsed_claims`] is empty and `claim_independent_count` is
/// absent; [`Self::plain_text`] returns the text of either shape.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct ClaimFragment {
    pub lang: Option<String>,
    pub claim_text: String,
    pub data_format: Option<String>,
    pub claim_independent_count: Option<i32>,
}

impl ClaimFragment {
    /// Parses [`Self::claim_text`] into structured claims - see
    /// [`parse_claim_html`]. Empty when the text carries no `<div>` markup.
    pub fn parsed_claims(&self) -> Vec<ParsedClaim> {
        parse_claim_html(&self.claim_text)
    }

    /// The text of [`Self::claim_text`] for either wire shape - see
    /// [`claim_plain_text`].
    pub fn plain_text(&self) -> String {
        claim_plain_text(&self.claim_text)
    }
}

/// One patent's claim data, as returned by `/basic-patent-data/claim-data`.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct PatentClaims {
    pub pn: String,
    pub patent_id: String,
    /// Set only when PatSnap substituted a family member's claims for the
    /// requested publication's own (`replace_by_related`).
    pub pn_related: Option<String>,
    /// Absent for documents whose claims are OCR'd plain text rather than
    /// markup - see [`ClaimFragment`].
    pub claim_count: Option<i32>,
    #[serde(default)]
    pub claims: Vec<ClaimFragment>,
}

/// One patent's description data, as returned by
/// `/basic-patent-data/description-data`.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct PatentDescription {
    pub pn: String,
    pub patent_id: String,
    /// Set only when PatSnap substituted a family member's description for
    /// the requested publication's own (`replace_by_related`).
    pub pn_related: Option<String>,
    #[serde(default)]
    pub description: Vec<LocalizedText>,
}

/// One dated document reference inside bibliography data - publication or
/// application - `{date, kind, year, month, country, doc_number}`.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct DateReference {
    pub date: Option<i32>,
    pub kind: Option<String>,
    pub year: Option<i32>,
    pub month: Option<i32>,
    pub country: Option<String>,
    pub doc_number: Option<String>,
}

/// A named party (applicant, inventor, or assignee) on a patent.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct Person {
    pub lang: Option<String>,
    pub name: Option<String>,
    pub address: Option<String>,
    pub sequence: Option<i32>,
    pub data_format: Option<String>,
}

/// The parties kept from PatSnap's bibliography `parties` object - agency,
/// agents, and examiners are dropped as not worth carrying.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct Parties {
    #[serde(default)]
    pub applicants: Vec<Person>,
    #[serde(default)]
    pub inventors: Vec<Person>,
    #[serde(default)]
    pub assignees: Vec<Person>,
}

/// A JSON value that is either a string or an integer - PatSnap's
/// classification parts come as both.
#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrNumber {
    Str(String),
    Num(i64),
}

/// One classification symbol - `{section, class, sub_class, main_group,
/// sub_group}`. PatSnap emits the numeric-looking parts as JSON numbers
/// (`"class": 60, "main_group": 21` in the official bibliography example)
/// and the rest as strings, so every part accepts either and lands as a
/// `String`.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct ClassificationCode {
    #[serde(default, deserialize_with = "string_or_number")]
    pub section: Option<String>,
    #[serde(default, deserialize_with = "string_or_number")]
    pub class: Option<String>,
    #[serde(default, deserialize_with = "string_or_number")]
    pub sub_class: Option<String>,
    #[serde(default, deserialize_with = "string_or_number")]
    pub main_group: Option<String>,
    #[serde(default, deserialize_with = "string_or_number")]
    pub sub_group: Option<String>,
}

/// A classification scheme's main symbol plus any further symbols.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct ClassificationDoc {
    pub main_classification: Option<ClassificationCode>,
    #[serde(default)]
    pub further_classification: Vec<ClassificationCode>,
}

/// The classification schemes kept from PatSnap's bibliography
/// `classification_data` object - FI, GBC, LOC, UPC, and F-term are dropped
/// as not worth carrying. `classification_ipcr` is PatSnap's IPC scheme.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct ClassificationData {
    pub classification_cpc: Option<ClassificationDoc>,
    pub classification_ipcr: Option<ClassificationDoc>,
}

/// The fields kept from PatSnap's 100+ field `bibliographic_data` object -
/// title, abstract, publication/application dates, parties, and IPC/CPC
/// classification. Everything else falls away via serde's default
/// unknown-field handling; add a field here to keep more.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct BibliographicData {
    #[serde(default)]
    pub invention_title: Vec<LocalizedText>,
    #[serde(default)]
    pub abstracts: Vec<LocalizedText>,
    pub publication_reference: Option<DateReference>,
    pub application_reference: Option<DateReference>,
    pub parties: Option<Parties>,
    pub classification_data: Option<ClassificationData>,
}

/// One patent's bibliography, as returned by
/// `/basic-patent-data/bibliography`.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct PatentBibliography {
    pub pn: String,
    pub patent_id: String,
    pub bibliographic_data: BibliographicData,
}

/// `application_reference` in simple-bibliography data - `{apno, date}`,
/// unlike the full bibliography's doc_number/kind/year/month shape.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct SimpleApplicationReference {
    pub apno: Option<String>,
    pub date: Option<i32>,
}

/// A classification scheme's symbols as plain strings, as returned by
/// simple-bibliography (`{main, further}`).
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct SimpleClassificationDoc {
    pub main: Option<String>,
    #[serde(default)]
    pub further: Vec<String>,
}

/// The classification schemes kept from simple-bibliography's
/// `classification_data` - see [`ClassificationData`] for what's dropped and
/// why.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct SimpleClassificationData {
    pub classification_cpc: Option<SimpleClassificationDoc>,
    pub classification_ipcr: Option<SimpleClassificationDoc>,
}

/// The fields kept from PatSnap's simple-bibliography `bibliographic_data`
/// object - see [`BibliographicData`] for the full-bibliography equivalent.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct SimpleBibliographicData {
    #[serde(default)]
    pub invention_title: Vec<LocalizedText>,
    #[serde(default)]
    pub abstracts: Vec<LocalizedText>,
    pub publication_reference: Option<DateReference>,
    pub application_reference: Option<SimpleApplicationReference>,
    pub parties: Option<Parties>,
    pub classification_data: Option<SimpleClassificationData>,
}

/// One patent's simple bibliography, as returned by
/// `/basic-patent-data/simple-bibliography`.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct SimplePatentBibliography {
    pub pn: String,
    pub patent_id: String,
    pub bibliographic_data: SimpleBibliographicData,
}

/// One member of a patent family - `{kind, country, sequence, doc_number}`.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct FamilyMember {
    pub kind: Option<String>,
    pub country: Option<String>,
    pub sequence: Option<i32>,
    pub doc_number: Option<String>,
}

/// One patent's family data, as returned by
/// `/basic-patent-data/patent-family`. All three families - simple,
/// INPADOC, and PatSnap - are kept side by side; picking one and discarding
/// the others would silently drop data.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PatentFamily {
    pub pn: String,
    pub patent_id: String,
    pub simple_family: Vec<FamilyMember>,
    pub inpadoc_family: Vec<FamilyMember>,
    pub patsnap_family: Vec<FamilyMember>,
    pub simple_family_id: Option<i32>,
    pub inpadoc_family_id: Option<i32>,
    pub patsnap_family_id: Option<i32>,
}

/// Wire shape of one `/basic-patent-data/patent-family` result - `pn` and
/// `patent_id` sit beside `patent_family`, which nests the three family
/// arrays. [`PatsnapClient::family`] flattens this into [`PatentFamily`].
#[derive(Deserialize)]
struct FamilyWire {
    pn: String,
    patent_id: String,
    patent_family: FamilyGroupsWire,
}

#[derive(Deserialize)]
struct FamilyGroupsWire {
    #[serde(default)]
    simple_family: Vec<FamilyMember>,
    #[serde(default)]
    inpadoc_family: Vec<FamilyMember>,
    #[serde(default)]
    patsnap_family: Vec<FamilyMember>,
    simple_family_id: Option<i32>,
    inpadoc_family_id: Option<i32>,
    patsnap_family_id: Option<i32>,
}

impl From<FamilyWire> for PatentFamily {
    fn from(wire: FamilyWire) -> Self {
        let groups = wire.patent_family;
        Self {
            pn: wire.pn,
            patent_id: wire.patent_id,
            simple_family: groups.simple_family,
            inpadoc_family: groups.inpadoc_family,
            patsnap_family: groups.patsnap_family,
            simple_family_id: groups.simple_family_id,
            inpadoc_family_id: groups.inpadoc_family_id,
            patsnap_family_id: groups.patsnap_family_id,
        }
    }
}

/// Field PatSnap search results can be sorted by. The wire values don't
/// case-convert cleanly from the variant names (`PBDT_YEARMONTHDAY` has no
/// underscore between `YEAR`, `MONTH`, and `DAY`), so each is spelled out
/// rather than derived with `rename_all`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SortBy {
    #[serde(rename = "PBDT_YEARMONTHDAY")]
    PbdtYearMonthDay,
    #[serde(rename = "APD_YEARMONTHDAY")]
    ApdYearMonthDay,
    #[serde(rename = "ISD")]
    Isd,
    #[serde(rename = "SCORE")]
    Score,
}

/// Sort direction for a [`SortField`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Asc,
    Desc,
}

/// One entry of [`SearchRequest::sort`] - a field plus its direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SortField {
    pub field: SortBy,
    pub order: SortOrder,
}

/// Date PatSnap collapses (deduplicates) search results by, before
/// [`CollapseType`] decides how broad the grouping is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CollapseBy {
    Apd,
    Pbd,
    Authority,
    Score,
}

/// How broadly PatSnap groups search results before collapsing each group
/// to one result - `All` disables collapsing entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CollapseType {
    All,
    Apno,
    Docdb,
    Inpadoc,
    Extend,
}

/// Which member of a collapsed group PatSnap keeps. Meaningful only when
/// `collapse_type` is [`CollapseType::Apno`] and `collapse_by` is
/// [`CollapseBy::Apd`] or [`CollapseBy::Pbd`] - PatSnap ignores it otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CollapseOrder {
    Oldest,
    Latest,
}

/// Request body for [`PatsnapClient::search`] - PatSnap's Analytics query
/// syntax, documented at <https://analytics.patsnap.com/search_helper>.
/// Build via [`query_request`] for the common case. Request structs are
/// not `#[non_exhaustive]`, unlike the response structs, so callers can
/// build one with a struct literal.
#[derive(Debug, Clone, Serialize)]
pub struct SearchRequest {
    pub query_text: String,
    pub limit: u32,
    pub offset: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<Vec<SortField>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapse_by: Option<CollapseBy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapse_type: Option<CollapseType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapse_order: Option<CollapseOrder>,
}

/// Request body for [`PatsnapClient::search_count`] - the same Analytics
/// query syntax as [`SearchRequest`], without pagination since the endpoint
/// only reports a total.
#[derive(Debug, Clone, Serialize)]
pub struct CountRequest {
    pub query_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapse_by: Option<CollapseBy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapse_type: Option<CollapseType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapse_order: Option<CollapseOrder>,
}

/// Wire response of `/search/patent/query-search-count` -
/// `{total_search_result_count}` is its only field.
#[derive(Deserialize)]
struct CountWire {
    total_search_result_count: u64,
}

/// One patent reference from a search result - just enough to look the
/// patent up through the basic-data endpoints.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PatentRef {
    pub pn: String,
    pub patent_id: String,
}

/// Parsed response of [`PatsnapClient::search`] - PatSnap's
/// `total_search_result_count` renamed to `total`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SearchResponse {
    pub total: u64,
    pub result_count: u32,
    pub results: Vec<PatentRef>,
}

/// Request body for [`PatsnapClient::semantic_search`] - free-text
/// similarity search, distinct in shape from [`SearchRequest`]: no
/// Analytics query syntax and no sort/collapse, but its own geography, date
/// range, and relevancy filters.
#[derive(Debug, Clone, Serialize)]
pub struct SemanticSearchRequest {
    pub text: String,
    pub limit: u32,
    pub offset: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apd_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apd_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pbd_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pbd_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relevancy: Option<String>,
}

/// One patent reference from a semantic-search result - like [`PatentRef`]
/// plus the similarity score PatSnap only returns for semantic search.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[non_exhaustive]
pub struct SemanticPatentRef {
    pub pn: String,
    pub patent_id: String,
    pub relevancy: Option<String>,
}

/// Response of [`PatsnapClient::semantic_search`].
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[non_exhaustive]
pub struct SemanticSearchResponse {
    #[serde(rename = "total_search_result_count")]
    pub total: u64,
    pub result_count: u32,
    #[serde(default)]
    pub results: Vec<SemanticPatentRef>,
}

/// The number to search by for [`PatsnapClient::search_by_number`] - the
/// endpoint takes one or the other, never both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatentNumberQuery {
    /// A publication number, e.g. `US11205304B2`.
    Pn(String),
    /// An application number, e.g. `US16/123456`.
    Apno(String),
}

impl PatentNumberQuery {
    fn value(&self) -> &str {
        match self {
            PatentNumberQuery::Pn(value) | PatentNumberQuery::Apno(value) => value,
        }
    }
}

impl Serialize for PatentNumberQuery {
    /// Serializes as a one-key map - `{"pn": ...}` or `{"apno": ...}` - so
    /// flattening it into the `search_by_number` request body puts exactly
    /// one of the two keys on the wire, never both, never the other as null.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            PatentNumberQuery::Pn(pn) => map.serialize_entry("pn", pn)?,
            PatentNumberQuery::Apno(apno) => map.serialize_entry("apno", apno)?,
        }
        map.end()
    }
}

/// Wire body for [`PatsnapClient::search_by_number`] - `number` flattens
/// into `pn` or `apno` at the top level via [`PatentNumberQuery`]'s own
/// `Serialize` impl.
#[derive(Serialize)]
struct PatentNumberRequestWire<'a> {
    #[serde(flatten)]
    number: &'a PatentNumberQuery,
    #[serde(skip_serializing_if = "Option::is_none")]
    authority: Option<&'a [String]>,
    limit: u32,
    offset: u32,
}

/// One match from [`PatsnapClient::search_by_number`] - PatSnap's standard
/// patent number plus the bibliographic fields the endpoint carries beyond
/// [`PatentRef`].
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[non_exhaustive]
pub struct PatentNumberMatch {
    pub pn: String,
    pub patent_id: String,
    pub apno: Option<String>,
    pub apdt: Option<i32>,
    pub pbdt: Option<i32>,
    pub title: Option<String>,
    pub inventor: Option<String>,
    pub current_assignee: Option<String>,
    pub original_assignee: Option<String>,
}

/// Wire response of `/search/patent/patent-search-pn` - only `results` is
/// kept; `result_count`/`total_search_result_count` aren't exposed since
/// [`PatsnapClient::search_by_number`] returns a plain `Vec`.
#[derive(Deserialize)]
struct PatentSearchPnResultsWire {
    #[serde(default)]
    results: Vec<PatentNumberMatch>,
}

/// Async client for the PatSnap Open Platform.
#[derive(Debug, Clone)]
pub struct PatsnapClient {
    http: reqwest::Client,
    config: ClientConfig,
    semaphore: Arc<Semaphore>,
    last_start: Arc<Mutex<Option<Instant>>>,
}

impl PatsnapClient {
    /// Builds a client with default config, optionally overriding the base
    /// URL (e.g. for a mock server in tests).
    ///
    /// # Panics
    ///
    /// See [`Self::with_config`].
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        let mut config = ClientConfig::default();
        if let Some(base_url) = base_url {
            config.base_url = base_url;
        }
        Self::with_config(api_key, config)
    }

    /// Builds a client with full control over pacing, retry, timeout, and
    /// concurrency.
    ///
    /// # Panics
    ///
    /// If `api_key` is not a valid HTTP header value. In practice that is a
    /// key read from a file with its trailing newline still attached - trim
    /// it first.
    pub fn with_config(api_key: String, config: ClientConfig) -> Self {
        let mut auth_value = HeaderValue::from_str(&format!("Bearer {api_key}"))
            .expect("api key must be a valid HTTP header value");
        auth_value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, auth_value);

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(config.timeout)
            .build()
            .expect("reqwest client builder has no fallible input here");

        let max_concurrent = config.max_concurrent.max(1);

        Self {
            http,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            last_start: Arc::new(Mutex::new(None)),
            config,
        }
    }

    /// Fetches one patent's claims - a thin wrapper over
    /// [`Self::claims_batch`] for a single `pn`. An empty result (the
    /// patent has no claim data) is [`PatsnapError::NotFound`].
    #[tracing::instrument(skip(self))]
    pub async fn claims(
        &self,
        pn: &str,
        fallback: FamilyFallback,
    ) -> Result<PatentClaims, PatsnapError> {
        first_or_not_found(self.claims_batch(&[pn.to_string()], fallback).await?)
    }

    /// Fetches claims for any number of patents, chunking the request at
    /// PatSnap's 100-patents-per-call cap. Chunks are fetched in input
    /// order; within a chunk the order is PatSnap's, and a publication
    /// PatSnap has no claims for is absent rather than an error. An empty
    /// `pns` returns an empty `Vec` without issuing a request.
    #[tracing::instrument(skip(self, pns))]
    pub async fn claims_batch(
        &self,
        pns: &[String],
        fallback: FamilyFallback,
    ) -> Result<Vec<PatentClaims>, PatsnapError> {
        if pns.is_empty() {
            return Ok(Vec::new());
        }

        let mut all = Vec::with_capacity(pns.len());
        for chunk in pns.chunks(CLAIMS_BATCH_LIMIT) {
            let query = [
                ("patent_number", chunk.join(",")),
                ("replace_by_related", fallback.query_value().to_string()),
            ];
            let page: Vec<PatentClaims> = self
                .request(Method::GET, "/basic-patent-data/claim-data", &query, None)
                .await?;
            all.extend(page);
        }
        Ok(all)
    }

    /// Fetches one patent's description. An empty result is
    /// [`PatsnapError::NotFound`].
    #[tracing::instrument(skip(self))]
    pub async fn description(
        &self,
        pn: &str,
        fallback: FamilyFallback,
    ) -> Result<PatentDescription, PatsnapError> {
        let query = [
            ("patent_number", pn.to_string()),
            ("replace_by_related", fallback.query_value().to_string()),
        ];
        let results: Vec<PatentDescription> = self
            .request(
                Method::GET,
                "/basic-patent-data/description-data",
                &query,
                None,
            )
            .await?;
        first_or_not_found(results)
    }

    /// Fetches one patent's full bibliography. An empty result is
    /// [`PatsnapError::NotFound`].
    #[tracing::instrument(skip(self))]
    pub async fn bibliography(&self, pn: &str) -> Result<PatentBibliography, PatsnapError> {
        let query = [("patent_number", pn.to_string())];
        let results: Vec<PatentBibliography> = self
            .request(Method::GET, "/basic-patent-data/bibliography", &query, None)
            .await?;
        first_or_not_found(results)
    }

    /// Fetches one patent's simple bibliography. An empty result is
    /// [`PatsnapError::NotFound`].
    #[tracing::instrument(skip(self))]
    pub async fn simple_bibliography(
        &self,
        pn: &str,
    ) -> Result<SimplePatentBibliography, PatsnapError> {
        let query = [("patent_number", pn.to_string())];
        let results: Vec<SimplePatentBibliography> = self
            .request(
                Method::GET,
                "/basic-patent-data/simple-bibliography",
                &query,
                None,
            )
            .await?;
        first_or_not_found(results)
    }

    /// Fetches one patent's family data - simple, INPADOC, and PatSnap
    /// families all at once. An empty result is [`PatsnapError::NotFound`].
    #[tracing::instrument(skip(self))]
    pub async fn family(&self, pn: &str) -> Result<PatentFamily, PatsnapError> {
        let query = [("patent_number", pn.to_string())];
        let results: Vec<FamilyWire> = self
            .request(
                Method::GET,
                "/basic-patent-data/patent-family",
                &query,
                None,
            )
            .await?;
        first_or_not_found(results).map(PatentFamily::from)
    }

    /// Searches PatSnap's global patent database using an Analytics query -
    /// `/search/patent/query-search-patent`. Query syntax is documented at
    /// <https://analytics.patsnap.com/search_helper>.
    #[tracing::instrument(skip(self, request))]
    pub async fn search(&self, request: &SearchRequest) -> Result<SearchResponse, PatsnapError> {
        check_query_text(&request.query_text, QUERY_TEXT_MAX_LEN)?;
        check_limit_offset(request.limit, request.offset, SEARCH_LIMIT_OFFSET_MAX)?;
        let body = serde_json::to_value(request).expect("SearchRequest always serializes to JSON");
        let data: serde_json::Value = self
            .request(
                Method::POST,
                "/search/patent/query-search-patent",
                &[],
                Some(body),
            )
            .await?;
        Ok(parse_search_response(&data))
    }

    /// Counts how many patents match an Analytics query, without fetching
    /// them - `/search/patent/query-search-count`. Same query syntax as
    /// [`Self::search`].
    #[tracing::instrument(skip(self, request))]
    pub async fn search_count(&self, request: &CountRequest) -> Result<u64, PatsnapError> {
        check_query_text(&request.query_text, QUERY_TEXT_MAX_LEN)?;
        let body = serde_json::to_value(request).expect("CountRequest always serializes to JSON");
        let wire: CountWire = self
            .request(
                Method::POST,
                "/search/patent/query-search-count",
                &[],
                Some(body),
            )
            .await?;
        Ok(wire.total_search_result_count)
    }

    /// Searches by free-text similarity rather than Analytics query syntax -
    /// `/search/patent/semantic-search-patent`. PatSnap recommends >200
    /// words of input (e.g. an abstract) for best results, and caps
    /// `limit + offset` at 1,000 here - narrower than [`Self::search`].
    #[tracing::instrument(skip(self, request))]
    pub async fn semantic_search(
        &self,
        request: &SemanticSearchRequest,
    ) -> Result<SemanticSearchResponse, PatsnapError> {
        if request.text.is_empty() {
            return Err(PatsnapError::Api("text must not be empty".to_string()));
        }
        check_limit_offset(request.limit, request.offset, SEMANTIC_LIMIT_OFFSET_MAX)?;
        let body =
            serde_json::to_value(request).expect("SemanticSearchRequest always serializes to JSON");
        self.request(
            Method::POST,
            "/search/patent/semantic-search-patent",
            &[],
            Some(body),
        )
        .await
    }

    /// Resolves a publication or application number to PatSnap's standard
    /// patent numbers - `/search/patent/patent-search-pn`. One-in, many-out:
    /// a single `apno` can match several jurisdictions' patent numbers, so
    /// this is a search over `authorities` (paginated by `limit`/`offset`),
    /// not a batch lookup by a list of publication numbers. An empty
    /// `authorities` searches every jurisdiction - the `authority` key is
    /// omitted rather than sent as an empty list. PatSnap documents no
    /// `limit + offset` ceiling here, unlike the rest of the search family -
    /// only `limit` itself is capped at 1,000.
    #[tracing::instrument(skip(self))]
    pub async fn search_by_number(
        &self,
        number: &PatentNumberQuery,
        authorities: &[String],
        limit: u32,
        offset: u32,
    ) -> Result<Vec<PatentNumberMatch>, PatsnapError> {
        if number.value().is_empty() {
            return Err(PatsnapError::Api("pn/apno must not be empty".to_string()));
        }
        check_limit_max(limit)?;
        let wire = PatentNumberRequestWire {
            number,
            authority: if authorities.is_empty() {
                None
            } else {
                Some(authorities)
            },
            limit,
            offset,
        };
        let body =
            serde_json::to_value(&wire).expect("PatentNumberRequestWire always serializes to JSON");
        let response: PatentSearchPnResultsWire = self
            .request(
                Method::POST,
                "/search/patent/patent-search-pn",
                &[],
                Some(body),
            )
            .await?;
        Ok(response.results)
    }

    /// Sends one request through the shared envelope, semaphore, pacer, and
    /// retry policy. Every endpoint method goes through here rather than
    /// touching `reqwest` directly, so the in-band error envelope - PatSnap
    /// reports errors as HTTP 200 with `status: false`, not via HTTP status
    /// alone - is handled exactly once.
    #[tracing::instrument(
        skip(self, body),
        fields(method = %method, path, http_status = field::Empty, attempts = field::Empty)
    )]
    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
    ) -> Result<T, PatsnapError> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .expect("semaphore is never closed");

        let span = Span::current();
        let mut attempt = 0;
        loop {
            self.pace().await;

            let mut req = self
                .http
                .request(method.clone(), format!("{}{path}", self.config.base_url));
            if !query.is_empty() {
                req = req.query(query);
            }
            if let Some(ref body) = body {
                req = req.json(body);
            }

            match req.send().await {
                Ok(response) => {
                    let status = response.status();
                    span.record("http_status", status.as_u16());
                    span.record("attempts", attempt as u64 + 1);

                    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                        return Err(PatsnapError::Auth(status_and_body(response).await));
                    }
                    if status == StatusCode::NOT_FOUND {
                        return Err(PatsnapError::NotFound);
                    }
                    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                        if attempt < self.config.retry_backoff.len() {
                            warn!(
                                attempt = attempt + 1,
                                %status,
                                "PatSnap retryable HTTP status, will retry"
                            );
                            tokio::time::sleep(self.config.retry_backoff[attempt]).await;
                            attempt += 1;
                            continue;
                        }
                        let message = status_and_body(response).await;
                        return Err(if status == StatusCode::TOO_MANY_REQUESTS {
                            PatsnapError::RateLimit(message)
                        } else {
                            PatsnapError::Api(message)
                        });
                    }
                    if !status.is_success() {
                        return Err(PatsnapError::Api(status_and_body(response).await));
                    }

                    let value = parse_envelope(response).await?;
                    info!("PatSnap request succeeded");
                    return Ok(value);
                }
                Err(err) => {
                    if is_transport_retryable(&err) && attempt < self.config.retry_backoff.len() {
                        warn!(
                            attempt = attempt + 1,
                            error = %err,
                            "PatSnap transport error, will retry"
                        );
                        tokio::time::sleep(self.config.retry_backoff[attempt]).await;
                        attempt += 1;
                        continue;
                    }
                    span.record("attempts", attempt as u64 + 1);
                    return Err(PatsnapError::Network(err.to_string()));
                }
            }
        }
    }

    /// Blocks the caller until at least `request_interval` has passed since
    /// the previous request's start.
    async fn pace(&self) {
        if self.config.request_interval.is_zero() {
            return;
        }
        let mut last = self.last_start.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < self.config.request_interval {
                tokio::time::sleep(self.config.request_interval - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }
}

/// The single result of a one-`pn` lookup. PatSnap answers an unknown
/// publication with `status: true` and an empty `data` list, not a 404, so
/// the empty list is what maps to [`PatsnapError::NotFound`].
fn first_or_not_found<T>(results: Vec<T>) -> Result<T, PatsnapError> {
    results.into_iter().next().ok_or(PatsnapError::NotFound)
}

/// `"{status}: {body}"` for a response that failed on its HTTP status - the
/// same message shape epo-client and lens-org-client use, so a caller can
/// classify by the leading digits and still read what the server said.
async fn status_and_body(response: reqwest::Response) -> String {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    format!("{status}: {body}")
}

/// Parses the PatSnap envelope out of a response body, mapping a failed
/// envelope (`status: false`) through [`classify_error_code`] and an absent
/// or null `data` on a successful envelope to [`PatsnapError::Api`].
async fn parse_envelope<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, PatsnapError> {
    let bytes = response
        .bytes()
        .await
        .map_err(|err| PatsnapError::Network(err.to_string()))?;

    let envelope: Envelope<T> = serde_json::from_slice(&bytes)
        .map_err(|err| PatsnapError::Api(format!("invalid response body: {err}")))?;

    if !envelope.status {
        return Err(classify_error_code(
            envelope.error_code,
            &envelope.error_msg,
        ));
    }

    envelope.data.ok_or_else(|| {
        PatsnapError::Api("response reported status: true but data was missing or null".to_string())
    })
}

/// Maps a PatSnap in-band `error_code` to a typed, terminal (never retried)
/// error. Codes documented at
/// <https://open.patsnap.com/devportal/api-reference/basic-patent-data/claim-data>:
/// business errors 68300004-68300010 (68300009 is not documented on that
/// page - unrecognized codes in the family still map to `Api`), platform
/// errors 67200001-67200007. The platform family mixes quota, auth, and
/// balance failures under one code range; classification below follows each
/// code's documented description rather than the range as a whole -
/// 67200001/67200002/67200004/67200007 name a quota or call limit and map to
/// `RateLimit`, 67200003/67200005/67200006 (token expiry, insufficient
/// balance, client expiry) map to `Auth`. 67200004 reads "No permission or
/// API package quota has exceeded the limit", which is two different
/// problems under one code - hence `RateLimit` carrying the message.
fn classify_error_code(code: i64, message: &str) -> PatsnapError {
    match code {
        67200001 | 67200002 | 67200004 | 67200007 => PatsnapError::RateLimit(message.to_string()),
        67200003 | 67200005 | 67200006 => PatsnapError::Auth(message.to_string()),
        68300004..=68300010 => PatsnapError::Api(message.to_string()),
        _ => PatsnapError::Api(format!("unrecognized error_code {code}: {message}")),
    }
}

/// Whether a transport-level `reqwest::Error` is worth retrying - connect
/// failures (which cover TLS handshake failures; reqwest has no separate
/// TLS predicate) and timeouts. Anything else (body/decode/builder errors)
/// is a client-side bug, not a transient fault, so it is terminal.
fn is_transport_retryable(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout()
}

/// Deserializes a string-or-integer JSON value into `Option<String>`
/// (`null` → `None`, `60` → `Some("60")`). Used on [`ClassificationCode`].
fn string_or_number<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<StringOrNumber>::deserialize(deserializer)?;
    Ok(value.map(|v| match v {
        StringOrNumber::Str(s) => s,
        StringOrNumber::Num(n) => n.to_string(),
    }))
}

/// Builds a [`SearchRequest`] for the common case - no sort or collapsing,
/// just a query and a page.
pub fn query_request(query: &str, limit: u32, offset: u32) -> SearchRequest {
    SearchRequest {
        query_text: query.to_string(),
        limit,
        offset,
        sort: None,
        collapse_by: None,
        collapse_type: None,
        collapse_order: None,
    }
}

/// Parses a [`PatsnapClient::search`] response body (the `data` object, not
/// the full envelope) into a [`SearchResponse`] - a parsing helper for
/// fixtures and recorded bodies, not a validator. A missing or malformed
/// field degrades to its zero value rather than panicking.
pub fn parse_search_response(json: &serde_json::Value) -> SearchResponse {
    let total = json
        .get("total_search_result_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let result_count = json
        .get("result_count")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(0);
    let results = json
        .get("results")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| PatentRef {
                    pn: item
                        .get("pn")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    patent_id: item
                        .get("patent_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    SearchResponse {
        total,
        result_count,
        results,
    }
}

/// Rejects a `query_text` PatSnap would reject anyway - empty, or over the
/// calling endpoint's documented character cap - before it reaches the
/// network.
fn check_query_text(query_text: &str, max_len: usize) -> Result<(), PatsnapError> {
    if query_text.is_empty() {
        return Err(PatsnapError::Api(
            "query_text must not be empty".to_string(),
        ));
    }
    let len = query_text.chars().count();
    if len > max_len {
        return Err(PatsnapError::Api(format!(
            "query_text is {len} characters, over the {max_len}-character limit"
        )));
    }
    Ok(())
}

/// Rejects a `limit`/`offset` pair PatSnap would reject anyway - `limit`
/// over PatSnap's shared 1,000-result cap, or `limit + offset` over
/// `max_sum` - before it reaches the network. `max_sum` is 20,000 for
/// [`PatsnapClient::search`] and 1,000 for [`PatsnapClient::semantic_search`].
fn check_limit_offset(limit: u32, offset: u32, max_sum: u32) -> Result<(), PatsnapError> {
    check_limit_max(limit)?;
    let sum = limit.saturating_add(offset);
    if sum > max_sum {
        return Err(PatsnapError::Api(format!(
            "limit + offset is {sum}, over the {max_sum} limit"
        )));
    }
    Ok(())
}

/// Rejects a `limit` PatSnap would reject anyway - over the search family's
/// shared 1,000-result cap - before it reaches the network. Shared by
/// [`check_limit_offset`] and [`PatsnapClient::search_by_number`], which has
/// no documented `limit + offset` ceiling to pair it with.
fn check_limit_max(limit: u32) -> Result<(), PatsnapError> {
    if limit > SEARCH_LIMIT_MAX {
        return Err(PatsnapError::Api(format!(
            "limit is {limit}, over the {SEARCH_LIMIT_MAX}-result limit"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn test_client() -> PatsnapClient {
        PatsnapClient::with_config("sk-test".to_string(), ClientConfig::default())
    }

    #[test]
    fn default_config_values() {
        let config = ClientConfig::default();
        assert_eq!(config.base_url, "https://connect.patsnap.com");
        assert_eq!(config.request_interval, Duration::ZERO);
        assert_eq!(
            config.retry_backoff,
            vec![
                Duration::from_millis(250),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ]
        );
        assert_eq!(config.timeout, Duration::from_secs(20));
        assert_eq!(config.max_concurrent, 8);
    }

    #[test]
    fn max_concurrent_zero_is_clamped_to_one() {
        let client = PatsnapClient::with_config(
            "sk-test".to_string(),
            ClientConfig {
                max_concurrent: 0,
                ..ClientConfig::default()
            },
        );
        assert_eq!(client.semaphore.available_permits(), 1);
    }

    #[test]
    fn classifies_business_error_code_as_api() {
        assert_eq!(
            classify_error_code(68300004, "Invalid parameter!"),
            PatsnapError::Api("Invalid parameter!".to_string())
        );
    }

    #[test]
    fn classifies_platform_quota_error_code_as_rate_limit() {
        assert_eq!(
            classify_error_code(67200002, "Quota exceeds the limit!"),
            PatsnapError::RateLimit("Quota exceeds the limit!".to_string())
        );
    }

    /// 67200004 is the reason `RateLimit` carries a message: PatSnap folds a
    /// permission failure and a quota failure into the one code, and the
    /// caller can only tell them apart from the text.
    #[test]
    fn rate_limit_keeps_the_ambiguous_platform_message() {
        let err = classify_error_code(
            67200004,
            "No permission or API package quota has exceeded the limit!",
        );
        assert_eq!(
            err,
            PatsnapError::RateLimit(
                "No permission or API package quota has exceeded the limit!".to_string()
            )
        );
        assert!(err.to_string().contains("No permission"));
    }

    #[test]
    fn classifies_platform_auth_error_code_as_auth() {
        assert_eq!(
            classify_error_code(67200003, "Access token expired or authentication error!"),
            PatsnapError::Auth("Access token expired or authentication error!".to_string())
        );
    }

    #[test]
    fn classification_code_accepts_numbers_and_strings() {
        let code: ClassificationCode = serde_json::from_value(json!({
            "class": 60, "section": "B", "sub_class": "R", "sub_group": "00", "main_group": 21
        }))
        .unwrap();
        assert_eq!(code.class.as_deref(), Some("60"));
        assert_eq!(code.main_group.as_deref(), Some("21"));
        assert_eq!(code.sub_group.as_deref(), Some("00"));
        assert_eq!(code.section.as_deref(), Some("B"));

        let code: ClassificationCode = serde_json::from_value(json!({"class": null})).unwrap();
        assert_eq!(code.class, None);
        assert_eq!(code.section, None);
        assert_eq!(code.sub_class, None);
        assert_eq!(code.main_group, None);
        assert_eq!(code.sub_group, None);
    }

    #[test]
    fn error_display_is_non_empty_and_never_leaks_the_api_key() {
        let variants = [
            PatsnapError::Auth("bad token".to_string()),
            PatsnapError::NotFound,
            PatsnapError::RateLimit("quota exceeded".to_string()),
            PatsnapError::Api("bad request".to_string()),
            PatsnapError::Network("connect refused".to_string()),
        ];
        for variant in variants {
            let rendered = variant.to_string();
            assert!(!rendered.is_empty());
            assert!(!rendered.contains("sk-"));
        }
    }

    #[test]
    fn query_request_sets_expected_defaults() {
        let request = query_request("TACD: virtual reality", 25, 5);
        assert_eq!(request.query_text, "TACD: virtual reality");
        assert_eq!(request.limit, 25);
        assert_eq!(request.offset, 5);
        assert!(request.sort.is_none());
        assert!(request.collapse_by.is_none());
        assert!(request.collapse_type.is_none());
        assert!(request.collapse_order.is_none());
    }

    #[test]
    fn sort_by_serializes_to_wire_strings() {
        assert_eq!(
            serde_json::to_value(SortBy::PbdtYearMonthDay).unwrap(),
            json!("PBDT_YEARMONTHDAY")
        );
        assert_eq!(
            serde_json::to_value(SortBy::ApdYearMonthDay).unwrap(),
            json!("APD_YEARMONTHDAY")
        );
        assert_eq!(serde_json::to_value(SortBy::Isd).unwrap(), json!("ISD"));
        assert_eq!(serde_json::to_value(SortBy::Score).unwrap(), json!("SCORE"));
    }

    #[test]
    fn sort_order_serializes_to_wire_strings() {
        assert_eq!(serde_json::to_value(SortOrder::Asc).unwrap(), json!("asc"));
        assert_eq!(
            serde_json::to_value(SortOrder::Desc).unwrap(),
            json!("desc")
        );
    }

    #[test]
    fn collapse_enums_serialize_to_wire_strings() {
        assert_eq!(serde_json::to_value(CollapseBy::Apd).unwrap(), json!("APD"));
        assert_eq!(serde_json::to_value(CollapseBy::Pbd).unwrap(), json!("PBD"));
        assert_eq!(
            serde_json::to_value(CollapseBy::Authority).unwrap(),
            json!("AUTHORITY")
        );
        assert_eq!(
            serde_json::to_value(CollapseBy::Score).unwrap(),
            json!("SCORE")
        );
        assert_eq!(
            serde_json::to_value(CollapseType::All).unwrap(),
            json!("ALL")
        );
        assert_eq!(
            serde_json::to_value(CollapseType::Apno).unwrap(),
            json!("APNO")
        );
        assert_eq!(
            serde_json::to_value(CollapseType::Docdb).unwrap(),
            json!("DOCDB")
        );
        assert_eq!(
            serde_json::to_value(CollapseType::Inpadoc).unwrap(),
            json!("INPADOC")
        );
        assert_eq!(
            serde_json::to_value(CollapseType::Extend).unwrap(),
            json!("EXTEND")
        );
        assert_eq!(
            serde_json::to_value(CollapseOrder::Oldest).unwrap(),
            json!("OLDEST")
        );
        assert_eq!(
            serde_json::to_value(CollapseOrder::Latest).unwrap(),
            json!("LATEST")
        );
    }

    #[test]
    fn none_knobs_are_omitted_not_null() {
        let request = query_request("virtual reality", 10, 0);
        let value = serde_json::to_value(&request).unwrap();
        let obj = value.as_object().unwrap();
        assert!(!obj.contains_key("sort"));
        assert!(!obj.contains_key("collapse_by"));
        assert!(!obj.contains_key("collapse_type"));
        assert!(!obj.contains_key("collapse_order"));
    }

    #[test]
    fn parses_a_realistic_search_response_body() {
        let body = json!({
            "total_search_result_count": 1042,
            "result_count": 2,
            "results": [
                {"pn": "US11205304B2", "patent_id": "718ead9c-4f3c-4674-8f5a-24e126827269"},
                {"pn": "CN106185468A", "patent_id": "b053642f-3108-4ea9-b629-420b0ab959e3"}
            ]
        });
        let parsed = parse_search_response(&body);
        assert_eq!(parsed.total, 1042);
        assert_eq!(parsed.result_count, 2);
        assert_eq!(
            parsed.results,
            vec![
                PatentRef {
                    pn: "US11205304B2".to_string(),
                    patent_id: "718ead9c-4f3c-4674-8f5a-24e126827269".to_string(),
                },
                PatentRef {
                    pn: "CN106185468A".to_string(),
                    patent_id: "b053642f-3108-4ea9-b629-420b0ab959e3".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parses_empty_object_to_zeros_without_panicking() {
        let parsed = parse_search_response(&json!({}));
        assert_eq!(parsed.total, 0);
        assert_eq!(parsed.result_count, 0);
        assert!(parsed.results.is_empty());
    }

    #[tokio::test]
    async fn search_rejects_empty_query_text() {
        let client = test_client();
        let request = query_request("", 10, 0);
        let err = client.search(&request).await.unwrap_err();
        match err {
            PatsnapError::Api(msg) => assert!(msg.contains("query_text")),
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_rejects_over_long_query_text() {
        let client = test_client();
        let request = query_request(&"a".repeat(QUERY_TEXT_MAX_LEN + 1), 10, 0);
        let err = client.search(&request).await.unwrap_err();
        match err {
            PatsnapError::Api(msg) => assert!(msg.contains("query_text")),
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_rejects_limit_over_one_thousand() {
        let client = test_client();
        let request = query_request("virtual reality", 1001, 0);
        let err = client.search(&request).await.unwrap_err();
        match err {
            PatsnapError::Api(msg) => assert!(msg.contains("limit")),
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_rejects_limit_plus_offset_over_twenty_thousand() {
        let client = test_client();
        let request = query_request("virtual reality", 1000, 19_001);
        let err = client.search(&request).await.unwrap_err();
        match err {
            PatsnapError::Api(msg) => assert!(msg.contains("limit + offset")),
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_count_rejects_over_long_query_text() {
        let client = test_client();
        let request = CountRequest {
            query_text: "a".repeat(QUERY_TEXT_MAX_LEN + 1),
            collapse_by: None,
            collapse_type: None,
            collapse_order: None,
        };
        let err = client.search_count(&request).await.unwrap_err();
        match err {
            PatsnapError::Api(msg) => assert!(msg.contains("query_text")),
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn semantic_search_rejects_limit_plus_offset_over_one_thousand() {
        let client = test_client();
        let request = SemanticSearchRequest {
            text: "a representative abstract".to_string(),
            limit: 600,
            offset: 500,
            country: None,
            apd_from: None,
            apd_to: None,
            pbd_from: None,
            pbd_to: None,
            relevancy: None,
        };
        let err = client.semantic_search(&request).await.unwrap_err();
        match err {
            PatsnapError::Api(msg) => assert!(msg.contains("limit + offset")),
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn semantic_search_rejects_empty_text() {
        let client = test_client();
        let request = SemanticSearchRequest {
            text: String::new(),
            limit: 10,
            offset: 0,
            country: None,
            apd_from: None,
            apd_to: None,
            pbd_from: None,
            pbd_to: None,
            relevancy: None,
        };
        let err = client.semantic_search(&request).await.unwrap_err();
        match err {
            PatsnapError::Api(msg) => assert!(msg.contains("text")),
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn patent_number_query_pn_serializes_to_pn_key_only() {
        let value =
            serde_json::to_value(PatentNumberQuery::Pn("US11205304B2".to_string())).unwrap();
        assert_eq!(value, json!({"pn": "US11205304B2"}));
    }

    #[test]
    fn patent_number_query_apno_serializes_to_apno_key_only() {
        let value =
            serde_json::to_value(PatentNumberQuery::Apno("US16/123456".to_string())).unwrap();
        assert_eq!(value, json!({"apno": "US16/123456"}));
    }

    #[test]
    fn search_by_number_wire_omits_authority_key_when_empty() {
        let number = PatentNumberQuery::Pn("US11205304B2".to_string());
        let wire = PatentNumberRequestWire {
            number: &number,
            authority: None,
            limit: 10,
            offset: 0,
        };
        let value = serde_json::to_value(&wire).unwrap();
        let obj = value.as_object().unwrap();
        assert!(!obj.contains_key("authority"));
        assert_eq!(obj.get("pn").unwrap(), "US11205304B2");
    }

    #[test]
    fn search_by_number_wire_includes_authority_key_when_present() {
        let number = PatentNumberQuery::Apno("US16/123456".to_string());
        let authorities = vec!["US".to_string(), "CN".to_string()];
        let wire = PatentNumberRequestWire {
            number: &number,
            authority: Some(&authorities),
            limit: 10,
            offset: 0,
        };
        let value = serde_json::to_value(&wire).unwrap();
        assert_eq!(value["authority"], json!(["US", "CN"]));
        assert!(value.as_object().unwrap().get("pn").is_none());
    }

    #[tokio::test]
    async fn search_by_number_rejects_empty_number() {
        let client = test_client();
        let number = PatentNumberQuery::Pn(String::new());
        let err = client
            .search_by_number(&number, &[], 10, 0)
            .await
            .unwrap_err();
        match err {
            PatsnapError::Api(msg) => assert!(msg.contains("pn") || msg.contains("apno")),
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_by_number_rejects_limit_over_one_thousand() {
        let client = test_client();
        let number = PatentNumberQuery::Apno("US16/123456".to_string());
        let err = client
            .search_by_number(&number, &[], 1001, 0)
            .await
            .unwrap_err();
        match err {
            PatsnapError::Api(msg) => assert!(msg.contains("limit")),
            other => panic!("expected Api error, got {other:?}"),
        }
    }
}
