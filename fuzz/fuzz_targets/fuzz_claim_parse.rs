#![no_main]

//! `parse_claim_html` and `claim_plain_text` scan vendor-supplied text by
//! hand, with no HTML crate behind them - unbalanced tags, truncated
//! attributes, multi-byte boundaries and markup-free OCR'd text all reach
//! them from the wire. Both must always return, so byte-slicing mistakes
//! surface here rather than in a caller.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(html) = std::str::from_utf8(data) else {
        return;
    };
    let _ = patsnap::claim_plain_text(html);
    for claim in patsnap::parse_claim_html(html) {
        // The parser owns this one: an independent claim depends on nothing,
        // whatever a `parent` attribute on its markup claimed. A dependent
        // claim's parent is only as trustworthy as the input, so it is not
        // asserted on here.
        if claim.independent {
            assert!(claim.parent.is_none());
        }
    }
});
