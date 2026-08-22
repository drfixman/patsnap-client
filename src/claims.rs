//! Parser for PatSnap's `claim_text` markup - HTML `<div>` wrappers that
//! carry claim numbering, independence, and parent linkage
//! (`class="indep-clm"` / `class="dep-clm"`, `num`, `parent`) around plain
//! text and inline formatting tags (`<seg-con>`, `<seg-trans>`,
//! `<seg-comp>`, `<sub>`, `<b>`, ...). EPO gives none of this structure, so
//! it is worth recovering. Hand-rolled scanning, no regex, no HTML crate.

const ENTITIES: [(&str, char); 5] = [
    ("&amp;", '&'),
    ("&lt;", '<'),
    ("&gt;", '>'),
    ("&quot;", '"'),
    ("&#39;", '\''),
];

/// One claim, reassembled from its (possibly multiple) `<div>` fragments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedClaim {
    pub num: u32,
    pub parent: Option<u32>,
    pub independent: bool,
    pub text: String,
}

/// Parses PatSnap `claim_text` HTML into one [`ParsedClaim`] per claim
/// number - consecutive `<div>` fragments sharing a `num` are joined into
/// one claim, inner tags are stripped, and entities are decoded. Never
/// panics - empty or malformed input (an unterminated tag, an unparseable
/// `num`) yields an empty `Vec` or simply skips the offending fragment.
///
/// Older documents come back from PatSnap as OCR'd plain text with no
/// `<div>` markup at all (observed live: `DE3106419A1`, filed 1981, whose
/// `claim_text` starts with the attorney's letterhead). There is no
/// structure to recover from that, so the result is empty; use
/// [`claim_plain_text`] to get the text of either shape.
pub fn parse_claim_html(html: &str) -> Vec<ParsedClaim> {
    let mut claims: Vec<ParsedClaim> = Vec::new();
    let mut rest = html;

    while let Some(div_start) = rest.find("<div") {
        let after_open = &rest[div_start..];
        let Some(tag_end) = after_open.find('>') else {
            break;
        };
        let tag = &after_open[..tag_end];
        let body_start = tag_end + 1;

        let Some(close_offset) = after_open[body_start..].find("</div>") else {
            break;
        };
        let inner = &after_open[body_start..body_start + close_offset];
        rest = &after_open[body_start + close_offset + "</div>".len()..];

        let Some(num) = extract_attr(tag, "num").and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let independent = extract_attr(tag, "class").as_deref() == Some("indep-clm");
        let parent = if independent {
            None
        } else {
            extract_attr(tag, "parent").and_then(|value| value.parse::<u32>().ok())
        };

        let text = strip_tags_and_decode(inner);
        if text.is_empty() {
            continue;
        }

        match claims.last_mut() {
            Some(last) if last.num == num => {
                last.text.push(' ');
                last.text.push_str(&text);
            }
            _ => claims.push(ParsedClaim {
                num,
                parent,
                independent,
                text,
            }),
        }
    }

    claims
}

/// Finds `name="..."` inside a tag's attribute string (the tag text between
/// `<div` and the closing `>`, exclusive) and returns the quoted value.
fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let value = &tag[start..];
    let end = value.find('"')?;
    Some(value[..end].to_string())
}

/// The text of a `claim_text` body, whichever shape PatSnap sent: tags
/// stripped and entities decoded for the `<div>` markup (one line per
/// fragment, since the wire puts a newline between `<div>`s), or the OCR'd
/// plain text of an older document passed through. Lines are trimmed and
/// empty lines dropped; nothing else is normalized, so the letterhead and
/// page markers an OCR'd document carries stay in.
pub fn claim_plain_text(claim_text: &str) -> String {
    decode_entities(&strip_tags(claim_text))
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strips `<...>` tags, decodes the entities PatSnap uses in claim text, and
/// collapses whitespace.
fn strip_tags_and_decode(fragment: &str) -> String {
    collapse_whitespace(&decode_entities(&strip_tags(fragment)))
}

/// Removes every `<...>` tag, keeping the text between them as-is.
fn strip_tags(fragment: &str) -> String {
    let mut without_tags = String::with_capacity(fragment.len());
    let mut in_tag = false;
    for ch in fragment.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            _ => without_tags.push(ch),
        }
    }
    without_tags
}

/// Decodes `&amp; &lt; &gt; &quot; &#39;`, leaving anything else (including
/// an unrecognized or malformed entity) untouched.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        match ENTITIES.iter().find(|(entity, _)| tail.starts_with(entity)) {
            Some((entity, decoded)) => {
                out.push(*decoded);
                rest = &tail[entity.len()..];
            }
            None => {
                out.push('&');
                rest = &tail['&'.len_utf8()..];
            }
        }
    }
    out.push_str(rest);

    out
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_independent_claim() {
        let html = r#"<div class="indep-clm" num="1">1. A device comprising a widget.</div>"#;
        let claims = parse_claim_html(html);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].num, 1);
        assert!(claims[0].independent);
        assert_eq!(claims[0].parent, None);
        assert_eq!(claims[0].text, "1. A device comprising a widget.");
    }

    #[test]
    fn parses_dependent_claim_with_parent() {
        let html = r#"<div class="dep-clm" num="2" parent="1">2. The device of claim 1.</div>"#;
        let claims = parse_claim_html(html);
        assert_eq!(claims.len(), 1);
        assert!(!claims[0].independent);
        assert_eq!(claims[0].parent, Some(1));
    }

    #[test]
    fn joins_consecutive_fragments_of_the_same_claim() {
        let html = concat!(
            r#"<div class="dep-clm" num="2" parent="1"><seg-con>2. The device of claim 1,</seg-con></div>"#,
            r#"<div class="dep-clm" num="2" parent="1"><seg-comp>wherein the sensor is piezoelectric.</seg-comp></div>"#,
        );
        let claims = parse_claim_html(html);
        assert_eq!(claims.len(), 1);
        assert_eq!(
            claims[0].text,
            "2. The device of claim 1, wherein the sensor is piezoelectric."
        );
    }

    #[test]
    fn strips_inner_tags() {
        let html =
            r#"<div class="indep-clm" num="1"><seg-con>1. A </seg-con><sub>b</sub> device.</div>"#;
        let claims = parse_claim_html(html);
        assert_eq!(claims[0].text, "1. A b device.");
    }

    #[test]
    fn decodes_entities() {
        let html = r#"<div class="indep-clm" num="1">A &amp; B &lt;module&gt; &quot;name&quot; &#39;x&#39;</div>"#;
        let claims = parse_claim_html(html);
        assert_eq!(claims[0].text, r#"A & B <module> "name" 'x'"#);
    }

    #[test]
    fn empty_input_returns_empty_vec() {
        assert!(parse_claim_html("").is_empty());
    }

    #[test]
    fn malformed_input_returns_empty_vec() {
        assert!(parse_claim_html(r#"<div class="indep-clm" num="1">unterminated"#).is_empty());
        assert!(parse_claim_html("not html at all").is_empty());
    }

    /// PatSnap's markup: one line per `<div>` fragment, inline tags gone,
    /// entities decoded.
    #[test]
    fn plain_text_of_markup_is_one_line_per_fragment() {
        let html = concat!(
            r#"<div class="indep-clm" num="1"><seg-con>1. A device, </seg-con><seg-trans>comprising:</seg-trans></div>"#,
            "\n",
            r#"<div class="indep-clm" num="1"><seg-refi>a widget &amp; a sprocket.</seg-refi></div>"#,
            "\n",
        );
        assert_eq!(
            claim_plain_text(html),
            "1. A device, comprising:\na widget & a sprocket."
        );
    }

    /// The OCR'd shape: no markup, so the text passes through with its
    /// lines trimmed and blank lines dropped.
    #[test]
    fn plain_text_of_ocr_text_passes_through() {
        let text = "Patentansprüche\n  1. Lösbare Verschlußanordnung.\n\n2. Nach Anspruch 1.\n";
        assert_eq!(
            claim_plain_text(text),
            "Patentansprüche\n1. Lösbare Verschlußanordnung.\n2. Nach Anspruch 1."
        );
        assert!(parse_claim_html(text).is_empty());
        assert_eq!(claim_plain_text(""), "");
    }

    #[test]
    fn unparseable_num_is_skipped_without_panic() {
        let html =
            r#"<div class="indep-clm" num="abc">bad</div><div class="indep-clm" num="1">ok</div>"#;
        let claims = parse_claim_html(html);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].num, 1);
    }
}
