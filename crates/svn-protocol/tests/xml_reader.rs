//! Behaviour of the streaming, namespace-aware XML reader.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use proptest::prelude::*;
use svn_protocol::xml::{Ns, XmlEvent, XmlLimits, XmlReader, read_all};

const DOC: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:S="http://subversion.tigris.org/xmlns/svn/">
  <!-- a comment -->
  <D:response xmlns="svn:" id="1"><D:href>a &amp; b &#233; &lt;x&gt;</D:href><item/><![CDATA[raw <&> data]]></D:response>
</D:multistatus>"#;

/// Merges adjacent text pieces so that chunking does not change the comparison.
fn normalize(events: Vec<XmlEvent>) -> Vec<XmlEvent> {
    let mut out: Vec<XmlEvent> = Vec::new();
    for e in events {
        match (out.last_mut(), e) {
            (Some(XmlEvent::Text(prev)), XmlEvent::Text(t)) => prev.push_str(&t),
            (_, e) => out.push(e),
        }
    }
    out
}

fn feed_in_pieces(doc: &[u8], cuts: &[usize]) -> Vec<XmlEvent> {
    let mut r = XmlReader::new();
    let mut points: Vec<usize> = cuts.iter().map(|c| c % (doc.len() + 1)).collect();
    points.sort_unstable();
    let mut events = Vec::new();
    let mut last = 0;
    for p in points {
        events.extend(r.feed(&doc[last..p]).unwrap());
        last = p;
    }
    events.extend(r.feed(&doc[last..]).unwrap());
    events.extend(r.finish().unwrap());
    normalize(events)
}

#[test]
fn resolves_namespaces_entities_and_cdata() {
    let events = normalize(read_all(DOC.as_bytes()).unwrap());
    let XmlEvent::Start(root) = &events[0] else {
        panic!("{events:?}")
    };
    assert!(root.name.is(&Ns::Dav, "multistatus"));
    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let XmlEvent::Start(t) = e {
                Some(t)
            } else {
                None
            }
        })
        .collect();
    // default namespace svn: applies to `item`, not to the prefixed D: elements nor to attributes
    assert!(starts.iter().any(|t| t.name.is(&Ns::Svn, "item")));
    let response = starts
        .iter()
        .find(|t| t.name.is(&Ns::Dav, "response"))
        .unwrap();
    assert_eq!(response.attrs[0].name.ns, Ns::None);
    assert_eq!(response.attr("id"), Some("1"));
    let texts: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let XmlEvent::Text(t) = e {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(texts.contains(&"a & b \u{e9} <x>"), "{texts:?}");
    assert!(texts.contains(&"raw <&> data"), "{texts:?}");
}

#[test]
fn every_single_byte_chunk_yields_the_same_events() {
    let whole = normalize(read_all(DOC.as_bytes()).unwrap());
    let mut r = XmlReader::new();
    let mut events = Vec::new();
    for b in DOC.as_bytes() {
        events.extend(r.feed(&[*b]).unwrap());
    }
    events.extend(r.finish().unwrap());
    assert_eq!(normalize(events), whole);
}

proptest! {
    #[test]
    fn arbitrary_chunking_is_equivalent(cuts in proptest::collection::vec(0usize..4096, 0..12)) {
        let whole = normalize(read_all(DOC.as_bytes()).unwrap());
        prop_assert_eq!(feed_in_pieces(DOC.as_bytes(), &cuts), whole);
    }

    #[test]
    fn never_panics_on_arbitrary_bytes(data in proptest::collection::vec(any::<u8>(), 0..512), cut in 0usize..512) {
        let mut r = XmlReader::new();
        let cut = cut.min(data.len());
        let _ = r.feed(&data[..cut]);
        let _ = r.feed(&data[cut..]);
        let _ = r.finish();
    }
}

#[test]
fn unknown_elements_are_still_reported_so_callers_can_skip_them() {
    let events = read_all(b"<a xmlns:x='urn:new'><x:future><b/></x:future><c/></a>").unwrap();
    let names: Vec<String> = events
        .iter()
        .filter_map(|e| {
            if let XmlEvent::Start(t) = e {
                Some(t.name.local.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(names, ["a", "future", "b", "c"]);
}

#[test]
fn rejects_malformed_documents() {
    let bad: [&[u8]; 12] = [
        b"",
        b"   ",
        b"<a>",
        b"<a></b>",
        b"</a>",
        b"<a/><b/>",
        b"<a/>trailing",
        b"<a>&undefined;</a>",
        b"<a><x:b/></a>",
        b"<a b='1' b='2'/>",
        b"<!DOCTYPE a [<!ENTITY x 'y'>]><a>&x;</a>",
        b"<a>\xff</a>",
    ];
    for doc in bad {
        assert!(read_all(doc).is_err(), "{:?}", String::from_utf8_lossy(doc));
    }
}

#[test]
fn entity_expansion_attacks_are_rejected_not_expanded() {
    let bomb = br#"<?xml version="1.0"?><!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;">]><lolz>&lol2;</lolz>"#;
    assert!(read_all(bomb).is_err());
}

#[test]
fn limits_bound_memory() {
    let mut deep = String::new();
    for _ in 0..200 {
        deep.push_str("<a>");
    }
    assert!(read_all(deep.as_bytes()).is_err(), "depth limit");

    let huge_tag = format!("<a b='{}'/>", "x".repeat(2 << 20));
    assert!(read_all(huge_tag.as_bytes()).is_err(), "markup limit");

    let many_attrs = format!(
        "<a {}/>",
        (0..300)
            .map(|i| format!("a{i}='1'"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert!(read_all(many_attrs.as_bytes()).is_err(), "attribute limit");
}

#[test]
fn long_text_is_delivered_in_pieces_without_buffering_it_all() {
    let limits = XmlLimits {
        text_flush: 64,
        ..XmlLimits::default()
    };
    let mut r = XmlReader::with_limits(limits);
    let mut events = r.feed(b"<a>").unwrap();
    let chunk = "caf\u{e9}&amp;".repeat(40);
    let mut pieces = 0;
    for _ in 0..50 {
        let out = r.feed(chunk.as_bytes()).unwrap();
        pieces += out
            .iter()
            .filter(|e| matches!(e, XmlEvent::Text(_)))
            .count();
        events.extend(out);
    }
    assert!(pieces > 10, "text should stream, got {pieces} pieces");
    events.extend(r.feed(b"</a>").unwrap());
    events.extend(r.finish().unwrap());
    let total: String = events
        .iter()
        .filter_map(|e| {
            if let XmlEvent::Text(t) = e {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(total, "caf\u{e9}&".repeat(2000));
}

#[test]
fn reader_refuses_to_continue_after_an_error() {
    let mut r = XmlReader::new();
    assert!(r.feed(b"<a></b>").is_err());
    assert!(r.feed(b"<a/>").is_err());
    assert!(r.finish().is_err());
}

#[test]
fn byte_order_mark_is_skipped() {
    let events = read_all(b"\xef\xbb\xbf<a/>").unwrap();
    assert_eq!(events.len(), 2);
}
