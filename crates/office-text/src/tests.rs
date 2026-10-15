#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Packages are built in memory from hand-written parts; namespace names use the reserved
//! `example.com` host.

use std::io::{Cursor, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::*;

const W: &str = r#"xmlns:w="http://example.com/w" xmlns:mc="http://example.com/mc""#;

fn package(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, data) in parts {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn docx(body: &str, extra: &[(&str, &[u8])]) -> Vec<u8> {
    let doc = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><w:document {W}><w:body>{body}</w:body></w:document>"#
    );
    let mut parts: Vec<(&str, &[u8])> = vec![("word/document.xml", doc.as_bytes())];
    parts.extend_from_slice(extra);
    package(&parts)
}

fn text_of(data: Vec<u8>, format: OfficeFormat) -> Result<String, ExtractError> {
    extract_text(Cursor::new(data), format, &Limits::default())
}

fn para(text: &str) -> String {
    format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>")
}

#[test]
fn formats_follow_the_extension() {
    assert_eq!(
        OfficeFormat::from_path("a/b/Report.DOCX"),
        Some(OfficeFormat::Docx)
    );
    assert_eq!(OfficeFormat::from_path("x.xlsm"), Some(OfficeFormat::Xlsx));
    assert_eq!(
        OfficeFormat::from_path("old.doc"),
        Some(OfficeFormat::LegacyDoc)
    );
    assert_eq!(
        OfficeFormat::from_path(r"C:\d\old.xls"),
        Some(OfficeFormat::LegacyXls)
    );
    assert_eq!(OfficeFormat::from_path("notes.txt"), None);
    assert_eq!(OfficeFormat::from_path("docx"), None);
    assert!(!OfficeFormat::LegacyDoc.is_supported());
}

#[test]
fn word_paragraphs_tables_and_headers() {
    let body = [
        para("Hợp đồng &amp; phụ lục"),
        // Two runs, a tab and a manual break inside one paragraph; tab stops are not text.
        r#"<w:p><w:pPr><w:tabs><w:tab w:val="left" w:pos="720"/></w:tabs></w:pPr><w:r><w:t xml:space="preserve">Giá </w:t></w:r><w:r><w:tab/><w:t>100</w:t><w:br/><w:t>dòng 2</w:t></w:r></w:p>"#.to_owned(),
        // Empty paragraphs leave no line.
        "<w:p/>".to_owned(),
        // Tracked deletion, field code and the fallback of alternate content are left out.
        r#"<w:p><w:del><w:r><w:delText>đã xoá</w:delText></w:r></w:del><w:r><w:instrText>PAGE</w:instrText></w:r><w:ins><w:r><w:t>đã thêm</w:t></w:r></w:ins></w:p>"#.to_owned(),
        r#"<w:p><w:r><mc:AlternateContent><mc:Choice><w:t>hộp</w:t></mc:Choice><mc:Fallback><w:t>bản sao</w:t></mc:Fallback></mc:AlternateContent></w:r></w:p>"#.to_owned(),
        format!(
            "<w:tbl><w:tr><w:tc>{}</w:tc><w:tc>{}{}</w:tc></w:tr><w:tr><w:tc>{}</w:tc><w:tc><w:tbl><w:tr><w:tc>{}</w:tc></w:tr></w:tbl></w:tc></w:tr></w:tbl>",
            para("A"),
            para("B1"),
            para("B2"),
            para("C"),
            para("lồng")
        ),
        para("Kết"),
    ]
    .concat();
    let header = format!(r#"<w:hdr {W}>{}</w:hdr>"#, para("Đầu trang"));
    let footer = format!(r#"<w:ftr {W}><w:p/></w:ftr>"#);
    let data = docx(
        &body,
        &[
            ("word/header2.xml", header.as_bytes()),
            ("word/footer1.xml", footer.as_bytes()),
            ("word/media/image1.png", &[0u8; 64]),
        ],
    );
    let text = text_of(data, OfficeFormat::Docx).unwrap();
    assert_eq!(
        text,
        "Hợp đồng & phụ lục\n\
         Giá \t100\n\
         dòng 2\n\
         đã thêm\n\
         hộp\n\
         | A | B1 / B2 |\n\
         | C | | lồng | |\n\
         Kết\n\
         === header2.xml ===\n\
         Đầu trang\n"
    );
}

#[test]
fn the_main_document_is_found_through_the_package_relationships() {
    let doc = format!(
        r#"<w:document {W}><w:body>{}</w:body></w:document>"#,
        para("ở chỗ khác")
    );
    let rels = r#"<Relationships xmlns="http://example.com/rels"><Relationship Id="rId1" Type="http://example.com/officeDocument/2006/relationships/officeDocument" Target="/word/document2.xml"/></Relationships>"#;
    let data = package(&[
        ("_rels/.rels", rels.as_bytes()),
        ("word/document2.xml", doc.as_bytes()),
    ]);
    assert_eq!(text_of(data, OfficeFormat::Docx).unwrap(), "ở chỗ khác\n");
}

fn xlsx() -> Vec<u8> {
    let workbook = r#"<workbook xmlns="http://example.com/x" xmlns:r="http://example.com/r"><sheets>
        <sheet name="Dữ liệu" sheetId="1" r:id="rId2"/>
        <sheet name="Ẩn" sheetId="2" state="hidden" r:id="rId1"/>
      </sheets></workbook>"#;
    let rels = r#"<Relationships xmlns="http://example.com/rels">
        <Relationship Id="rId1" Type="http://example.com/worksheet" Target="worksheets/sheet1.xml"/>
        <Relationship Id="rId2" Type="http://example.com/worksheet" Target="/xl/worksheets/sheet2.xml"/>
        <Relationship Id="rId3" Type="http://example.com/sharedStrings" Target="sharedStrings.xml"/>
        <Relationship Id="rId9" Type="http://example.com/hyperlink" Target="https://example.com/" TargetMode="External"/>
      </Relationships>"#;
    let strings = r#"<sst xmlns="http://example.com/x"><si><t>Tên</t></si><si><r><t>Đơn </t></r><r><t>giá</t></r><rPh><t>phiên âm</t></rPh></si><si><t>hai
dòng</t></si></sst>"#;
    let data_sheet = r#"<worksheet xmlns="http://example.com/x"><sheetData>
        <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row>
        <row r="2"><c r="A2" t="inlineStr"><is><t>Bút</t></is></c><c r="B2"><v>1500</v></c></row>
        <row r="3"><c r="A3" t="s"><v>2</v></c><c r="B3"><f>SUM(B2:B2)</f><v>1500</v></c><c r="C3" t="b"><v>1</v></c><c r="D3" s="1"/></row>
        <row><c><v>7</v></c><c t="e"><v>#DIV/0!</v></c></row>
      </sheetData></worksheet>"#;
    let hidden_sheet = r#"<worksheet xmlns="http://example.com/x"><sheetData><row r="5"><c r="E5" t="str"><f>"x"&amp;"y"</f><v>xy</v></c></row></sheetData></worksheet>"#;
    package(&[
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
        ("xl/sharedStrings.xml", strings.as_bytes()),
        ("xl/worksheets/sheet1.xml", hidden_sheet.as_bytes()),
        ("xl/worksheets/sheet2.xml", data_sheet.as_bytes()),
    ])
}

#[test]
fn excel_cells_in_workbook_order() {
    let text = text_of(xlsx(), OfficeFormat::Xlsx).unwrap();
    assert_eq!(
        text,
        "=== Sheet: Dữ liệu ===\n\
         Dữ liệu!A1: Tên\n\
         Dữ liệu!B1: Đơn giá\n\
         Dữ liệu!A2: Bút\n\
         Dữ liệu!B2: 1500\n\
         Dữ liệu!A3: hai\\ndòng\n\
         Dữ liệu!B3: 1500  [=SUM(B2:B2)]\n\
         Dữ liệu!C3: TRUE\n\
         Dữ liệu!A4: 7\n\
         Dữ liệu!B4: #DIV/0!\n\
         === Sheet: Ẩn (hidden) ===\n\
         Ẩn!E5: xy  [=\"x\"&\"y\"]\n"
    );
}

#[test]
fn legacy_and_foreign_files_are_refused() {
    assert_eq!(
        text_of(b"whatever".to_vec(), OfficeFormat::LegacyDoc),
        Err(ExtractError::Legacy(OfficeFormat::LegacyDoc))
    );
    // A .doc renamed .docx, or an encrypted document (both are compound files).
    let mut ole = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    ole.extend_from_slice(&[0; 600]);
    assert_eq!(
        text_of(ole, OfficeFormat::Docx),
        Err(ExtractError::NotAPackage)
    );
    assert_eq!(
        text_of(b"plain".to_vec(), OfficeFormat::Xlsx),
        Err(ExtractError::NotAPackage)
    );
    let empty = package(&[("other.xml", b"<a/>")]);
    assert_eq!(
        text_of(empty, OfficeFormat::Docx),
        Err(ExtractError::MissingPart("word/document.xml".into()))
    );
}

fn small_limits() -> Limits {
    Limits {
        max_entry_bytes: 1024 * 1024,
        max_total_bytes: 1536 * 1024,
        ..Limits::default()
    }
}

#[test]
fn a_part_that_decompresses_beyond_the_limit_is_refused() {
    // 8 MiB of the same byte compresses to a few kilobytes.
    let filler = format!(
        "<w:p><w:r><w:t>{}</w:t></w:r></w:p>",
        "a".repeat(8 * 1024 * 1024)
    );
    let data = docx(&filler, &[]);
    assert!(
        data.len() < 64 * 1024,
        "the bomb should be small: {}",
        data.len()
    );
    let err = extract_text(Cursor::new(data), OfficeFormat::Docx, &small_limits()).unwrap_err();
    assert!(matches!(err, ExtractError::LimitExceeded(_)), "{err:?}");
}

#[test]
fn parts_together_beyond_the_total_limit_are_refused() {
    let big = format!(r#"<w:hdr {W}>{}</w:hdr>"#, para(&"b".repeat(900 * 1024)));
    let data = docx(
        &para(&"a".repeat(900 * 1024)),
        &[("word/header1.xml", big.as_bytes())],
    );
    let err = extract_text(Cursor::new(data), OfficeFormat::Docx, &small_limits()).unwrap_err();
    assert_eq!(
        err,
        ExtractError::LimitExceeded("the package decompresses to more than the allowed size")
    );
}

#[test]
fn large_pictures_are_never_read() {
    let picture = vec![7u8; 4 * 1024 * 1024];
    let data = docx(&para("chữ"), &[("word/media/image1.jpeg", &picture)]);
    let text = extract_text(Cursor::new(data), OfficeFormat::Docx, &small_limits()).unwrap();
    assert_eq!(text, "chữ\n");
}

#[test]
fn too_many_entries_deep_nesting_dtds_and_long_output_are_refused() {
    let names: Vec<String> = (0..40).map(|i| format!("f{i}.xml")).collect();
    let mut parts: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"<a/>"[..])).collect();
    let doc = format!(r#"<w:document {W}/>"#);
    parts.push(("word/document.xml", doc.as_bytes()));
    let limits = Limits {
        max_entries: 32,
        ..Limits::default()
    };
    let err = extract_text(Cursor::new(package(&parts)), OfficeFormat::Docx, &limits).unwrap_err();
    assert_eq!(
        err,
        ExtractError::LimitExceeded("too many entries in the package")
    );

    let deep = format!("{}{}", "<w:sdt>".repeat(300), "</w:sdt>".repeat(300));
    let err = text_of(docx(&deep, &[]), OfficeFormat::Docx).unwrap_err();
    assert_eq!(err, ExtractError::LimitExceeded("XML nesting is too deep"));

    let dtd = br#"<?xml version="1.0"?><!DOCTYPE x [<!ENTITY a "aaaa">]><w:document><w:body/></w:document>"#;
    let err = text_of(package(&[("word/document.xml", dtd)]), OfficeFormat::Docx).unwrap_err();
    assert!(matches!(err, ExtractError::BadXml(_)), "{err:?}");

    let limits = Limits {
        max_output_bytes: 100,
        ..Limits::default()
    };
    let body = (0..20)
        .map(|i| para(&format!("đoạn {i}")))
        .collect::<String>();
    let err = extract_text(Cursor::new(docx(&body, &[])), OfficeFormat::Docx, &limits).unwrap_err();
    assert_eq!(
        err,
        ExtractError::LimitExceeded("the extracted text is too long")
    );
}
