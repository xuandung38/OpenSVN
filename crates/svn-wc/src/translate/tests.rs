//! Unit and property tests for translation. Byte-exact comparison with a real svn client
//! lives in `tests/translate_golden.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::borrow::Cow;

use proptest::prelude::*;

use super::keywords::human_date;
use super::*;
use std::io::Cursor;

// Owned-result wrappers so results compare directly with byte-string literals.
fn to_working(
    i: &TranslateInfo,
    input: &[u8],
    ctx: &KeywordContext,
    tz: UtcOffsetAt<'_>,
) -> Result<Vec<u8>, TranslateError> {
    super::to_working(i, input, ctx, tz).map(Cow::into_owned)
}

fn to_working_with_native(
    i: &TranslateInfo,
    input: &[u8],
    ctx: &KeywordContext,
    tz: UtcOffsetAt<'_>,
    native: &'static [u8],
) -> Result<Vec<u8>, TranslateError> {
    super::to_working_with_native(i, input, ctx, tz, native).map(Cow::into_owned)
}

fn to_repo_normal(
    i: &TranslateInfo,
    input: &[u8],
    repair: bool,
) -> Result<Vec<u8>, TranslateError> {
    super::to_repo_normal(i, input, repair).map(Cow::into_owned)
}

fn info(eol: EolStyle, keywords: Option<&str>) -> TranslateInfo {
    TranslateInfo {
        eol_style: eol,
        keywords_prop: keywords.map(|k| k.as_bytes().to_vec()),
        special: false,
    }
}

fn ctx() -> KeywordContext {
    KeywordContext {
        rev: Some(42),
        url: Some("http://h/repo/trunk/a%20b.txt".into()),
        repos_root_url: Some("http://h/repo".into()),
        date_micros: Some(1_000_000_000_123_456),
        author: Some("al".into()),
    }
}

#[test]
fn eol_style_parsing() {
    assert_eq!(EolStyle::from_prop(None), EolStyle::None);
    assert_eq!(EolStyle::from_prop(Some(b"native")), EolStyle::Native);
    assert_eq!(EolStyle::from_prop(Some(b"LF")), EolStyle::Lf);
    assert_eq!(EolStyle::from_prop(Some(b"CR")), EolStyle::Cr);
    assert_eq!(EolStyle::from_prop(Some(b"CRLF")), EolStyle::CrLf);
    assert_eq!(EolStyle::from_prop(Some(b"lf")), EolStyle::Unknown);
}

#[test]
fn native_uses_given_platform_eol_and_normalises_to_lf() {
    let i = info(EolStyle::Native, None);
    let w = to_working_with_native(&i, b"a\nb\n", &ctx(), &utc_offset_zero, b"\r\n").unwrap();
    assert_eq!(w, b"a\r\nb\r\n");
    assert_eq!(to_repo_normal(&i, &w, false).unwrap(), b"a\nb\n");
}

#[test]
fn native_mixed_fails_unless_repaired() {
    let i = info(EolStyle::Native, None);
    assert!(matches!(
        to_repo_normal(&i, b"a\nb\r\n", false),
        Err(TranslateError::InconsistentEol)
    ));
    assert_eq!(
        to_repo_normal(&i, b"a\nb\r\nc\r", true).unwrap(),
        b"a\nb\nc\n"
    );
}

#[test]
fn fixed_style_is_repaired_when_normalising() {
    let i = info(EolStyle::CrLf, None);
    assert_eq!(
        to_repo_normal(&i, b"a\nb\rc\r\n", false).unwrap(),
        b"a\r\nb\r\nc\r\n"
    );
}

#[test]
fn expansion_always_repairs() {
    let i = info(EolStyle::Lf, None);
    assert_eq!(
        to_working(&i, b"a\r\nb\rc\n", &ctx(), &utc_offset_zero).unwrap(),
        b"a\nb\nc\n"
    );
}

#[test]
fn unknown_style_normalise_errors_but_expand_keeps_eols() {
    let i = info(EolStyle::Unknown, Some("Rev"));
    assert!(matches!(
        to_repo_normal(&i, b"x", false),
        Err(TranslateError::UnknownEol)
    ));
    assert_eq!(
        to_working(&i, b"$Rev$\r\n", &ctx(), &utc_offset_zero).unwrap(),
        b"$Rev: 42 $\r\n"
    );
}

#[test]
fn no_translation_is_identity() {
    let i = info(EolStyle::None, None);
    let data = b"a\r\nb\n$Id$";
    assert_eq!(
        to_working(&i, data, &ctx(), &utc_offset_zero).unwrap(),
        data
    );
    assert_eq!(to_repo_normal(&i, data, false).unwrap(), data);
}

#[test]
fn keywords_expand_and_contract() {
    let i = info(EolStyle::None, Some("Rev Author Id Header"));
    let w = to_working(
        &i,
        b"$Rev$ $Author$ $Id$ $Header$",
        &ctx(),
        &utc_offset_zero,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(w.clone()).unwrap(),
        "$Rev: 42 $ $Author: al $ $Id: a b.txt 42 2001-09-09 01:46:40Z al $ \
         $Header: http://h/repo/trunk/a%20b.txt 42 2001-09-09 01:46:40Z al $"
    );
    assert_eq!(
        to_repo_normal(&i, &w, false).unwrap(),
        b"$Rev$ $Author$ $Id$ $Header$"
    );
}

#[test]
fn fixed_length_keywords_pad_and_truncate() {
    let i = info(EolStyle::None, Some("Author"));
    let w = to_working(
        &i,
        b"$Author:: $\n$Author::      $\n$Author::   $",
        &ctx(),
        &utc_offset_zero,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(w).unwrap(),
        "$Author:: $\n$Author:: al   $\n$Author:: a#$"
    );
}

#[test]
fn unknown_and_unterminated_keywords_are_left_alone() {
    let i = info(EolStyle::None, Some("Rev"));
    let data = b"$Foo$ $rev$ $Rev $ $$ $Rev: unterminated\nnext $Rev$";
    let w = to_working(&i, data, &ctx(), &utc_offset_zero).unwrap();
    assert_eq!(
        w,
        b"$Foo$ $rev$ $Rev $ $$ $Rev: unterminated\nnext $Rev: 42 $"
    );
}

#[test]
fn custom_keyword_definitions() {
    let i = info(EolStyle::None, Some("Mine=%a:%r:%b:%P:%%:%_:%R"));
    let w = to_working(&i, b"$Mine$", &ctx(), &utc_offset_zero).unwrap();
    assert_eq!(
        String::from_utf8(w).unwrap(),
        "$Mine: al:42:a b.txt:trunk/a b.txt:%: :http://h/repo $"
    );
}

#[test]
fn keyword_value_is_truncated_to_the_255_byte_limit() {
    let i = info(EolStyle::None, Some("Foo=%a"));
    let mut c = ctx();
    c.author = Some("x".repeat(400));
    let w = to_working(&i, b"$Foo$", &c, &utc_offset_zero).unwrap();
    assert_eq!(w.len(), 255);
    assert!(w.starts_with(b"$Foo: xxx") && w.ends_with(b"x $"));
}

#[test]
fn missing_values_expand_to_nothing() {
    let i = info(EolStyle::None, Some("Rev Date Author URL"));
    let w = to_working(
        &i,
        b"$Rev$ $Date$ $Author$ $URL$",
        &KeywordContext::default(),
        &utc_offset_zero,
    )
    .unwrap();
    assert_eq!(w, b"$Rev: $ $Date: $ $Author: $ $URL: $");
}

#[test]
fn human_date_matches_svn_format() {
    let micros = 1_024_830_782_000_000; // 2002-06-23 11:13:02 UTC
    assert_eq!(
        human_date(micros, 3 * 3600),
        "2002-06-23 14:13:02 +0300 (Sun, 23 Jun 2002)"
    );
    assert_eq!(
        human_date(micros, -(2 * 3600 + 1800)),
        "2002-06-23 08:43:02 -0230 (Sun, 23 Jun 2002)"
    );
    // svn prints gmtoff / 3600 with a forced sign, so a negative sub-hour offset shows "+00".
    assert_eq!(
        human_date(micros, -1800),
        "2002-06-23 10:43:02 +0030 (Sun, 23 Jun 2002)"
    );
    assert_eq!(
        human_date(86_400_000_000 * 59, 0),
        "1970-03-01 00:00:00 +0000 (Sun, 01 Mar 1970)"
    );
}

#[test]
fn special_parse_and_roundtrip() {
    use special::{SpecialContent, parse, symlink_to_repo_normal};
    assert_eq!(
        parse(b"link a/b c"),
        SpecialContent::Link(b"a/b c".to_vec())
    );
    assert_eq!(parse(b"link t\nrest"), SpecialContent::Link(b"t".to_vec()));
    assert_eq!(
        parse(b"device x"),
        SpecialContent::Regular(b"device x".to_vec())
    );
    assert_eq!(symlink_to_repo_normal(b"t"), b"link t");
}

#[cfg(unix)]
#[test]
fn special_install_creates_symlink_and_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("l");
    special::install(&p, b"link dest/x").unwrap();
    assert_eq!(
        std::fs::read_link(&p).unwrap(),
        std::path::Path::new("dest/x")
    );
    assert_eq!(special::read_repo_normal(&p).unwrap(), b"link dest/x");
    special::install(&p, b"link other").unwrap();
    assert_eq!(
        std::fs::read_link(&p).unwrap(),
        std::path::Path::new("other")
    );
    let q = dir.path().join("q");
    special::install(&q, b"weird thing\n").unwrap();
    assert_eq!(std::fs::read(&q).unwrap(), b"weird thing\n");
}

#[cfg(unix)]
#[test]
fn permission_helpers_follow_svn_bit_rules() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f");
    std::fs::write(&p, b"x").unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let set = |m| std::fs::set_permissions(&p, std::fs::Permissions::from_mode(m)).unwrap();

    set(0o644);
    flags::set_executable(&p, true).unwrap();
    assert_eq!(mode(&p), 0o755);
    flags::set_executable(&p, false).unwrap();
    assert_eq!(mode(&p), 0o644);
    set(0o600);
    flags::set_executable(&p, true).unwrap();
    assert_eq!(mode(&p), 0o700);

    set(0o664);
    flags::set_read_only(&p).unwrap();
    assert_eq!(mode(&p), 0o444);
    flags::set_read_write(&p).unwrap();
    assert_eq!(mode(&p), 0o644);

    // A write bit without a matching read bit stays (svn only clears where read is set).
    set(0o602);
    flags::set_read_only(&p).unwrap();
    assert_eq!(mode(&p), 0o402);
}

#[cfg(unix)]
#[test]
fn sync_with_props_rules() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f");
    std::fs::write(&p, b"x").unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let base = flags::SyncInput {
        status_is_normal: true,
        needs_lock_working: true,
        needs_lock_pristine: true,
        has_lock: false,
        executable: true,
    };
    flags::sync_with_props(&p, base).unwrap();
    assert_eq!(mode(&p), 0o555);
    flags::sync_with_props(
        &p,
        flags::SyncInput {
            has_lock: true,
            ..base
        },
    )
    .unwrap();
    assert_eq!(mode(&p) & 0o200, 0o200);
    // Uncommitted needs-lock leaves the write bit alone.
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    let uncommitted = flags::SyncInput {
        needs_lock_pristine: false,
        executable: false,
        ..base
    };
    flags::sync_with_props(&p, uncommitted).unwrap();
    assert_eq!(mode(&p), 0o644);
}

#[test]
fn fixed_length_fill_stops_at_the_first_dollar() {
    // Differential results from svn 1.15.0 with svn:keywords=Rev.
    let i = info(EolStyle::None, Some("Rev Id"));
    let mut c = ctx();
    c.rev = Some(1);
    assert_eq!(
        to_working(&i, b"$Rev:: a$b $\n", &c, &utc_offset_zero).unwrap(),
        b"$Rev:: 1$b $\n"
    );
    assert_eq!(
        to_repo_normal(&i, b"$Rev:: 5$b $\n", false).unwrap(),
        b"$Rev::  $b $\n"
    );
}

#[test]
fn special_files_are_never_translated() {
    let i = TranslateInfo {
        eol_style: EolStyle::CrLf,
        keywords_prop: Some(b"Id".to_vec()),
        special: true,
    };
    let data = b"link a\nb$Id$\r";
    assert!(matches!(
        super::to_working(&i, data, &ctx(), &utc_offset_zero).unwrap(),
        Cow::Borrowed(_)
    ));
    assert_eq!(
        to_working(&i, data, &ctx(), &utc_offset_zero).unwrap(),
        &data[..]
    );
    assert_eq!(to_repo_normal(&i, data, false).unwrap(), &data[..]);
}

#[test]
fn unknown_keyword_names_do_not_require_translation() {
    assert!(!info(EolStyle::None, Some("foo bar")).translation_required());
    assert!(info(EolStyle::None, Some("foo Id")).translation_required());
    assert!(info(EolStyle::Native, None).translation_required());
}

/// svn resets its rollback marker for every chunk it translates, so a malformed candidate that
/// straddles the normalisation chunk boundary is flushed raw (differential result, svn 1.15.0).
#[test]
fn normalisation_chunk_boundary_matches_svn() {
    let i = info(EolStyle::None, Some("Rev Id"));
    let mut data = vec![b'x'; NORMALIZE_CHUNK_SIZE - 7];
    data.extend_from_slice(b"$Rev:a$Id: x $\n");
    let got = to_repo_normal(&i, &data, false).unwrap();
    assert!(
        got.ends_with(b"$Rev:a$Id: x $\n"),
        "{:?}",
        String::from_utf8_lossy(&got[got.len() - 20..])
    );
    // Without the boundary the rollback applies and the second candidate is contracted.
    let small = to_repo_normal(&i, b"$Rev:a$Id: x $\n", false).unwrap();
    assert_eq!(small, b"$Rev:a$Id$\n");
}

#[test]
fn streaming_equals_slice_translation() {
    let i = info(EolStyle::CrLf, Some("Id Rev"));
    let mut data = Vec::new();
    for n in 0..20_000 {
        data.extend_from_slice(format!("line {n} $Rev$ $Id: stale $\n").as_bytes());
    }
    let expected = to_working(&i, &data, &ctx(), &utc_offset_zero).unwrap();
    let t = ContentTranslator::to_working(&i, &ctx(), &utc_offset_zero);
    let mut out = Vec::new();
    // A reader that returns tiny reads must not change the result.
    struct Trickle<'a>(&'a [u8]);
    impl std::io::Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.len().min(buf.len()).min(1000);
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }
    translate_stream(&mut Trickle(&data), &mut out, t).unwrap();
    assert_eq!(out, expected);
    let back = ContentTranslator::to_repo_normal(&i, false).unwrap();
    let mut normal = Vec::new();
    translate_stream(&mut Cursor::new(&out), &mut normal, back).unwrap();
    assert_eq!(normal, to_repo_normal(&i, &out, false).unwrap());
}

#[test]
fn date_uses_the_offset_in_effect_at_the_commit_time() {
    let i = info(EolStyle::None, Some("Date"));
    let mut c = ctx();
    // 2026-03-01 12:00:00 UTC and 2026-07-01 12:00:00 UTC.
    let (winter, summer) = (1_772_366_400_i64, 1_782_907_200_i64);
    // A fake zone with DST: +1h until the end of March, +2h afterwards.
    let tz = |secs: i64| if secs < 1_774_000_000 { 3600 } else { 7200 };
    c.date_micros = Some(winter * 1_000_000);
    assert_eq!(
        String::from_utf8(to_working(&i, b"$Date$", &c, &tz).unwrap()).unwrap(),
        "$Date: 2026-03-01 13:00:00 +0100 (Sun, 01 Mar 2026) $"
    );
    c.date_micros = Some(summer * 1_000_000);
    assert_eq!(
        String::from_utf8(to_working(&i, b"$Date$", &c, &tz).unwrap()).unwrap(),
        "$Date: 2026-07-01 14:00:00 +0200 (Wed, 01 Jul 2026) $"
    );
    // A fixed zone is just a constant resolver; the short date used by $Id$ stays UTC.
    let id = info(EolStyle::None, Some("Id"));
    assert_eq!(
        String::from_utf8(to_working(&id, b"$Id$", &c, &|_| 7200).unwrap()).unwrap(),
        "$Id: a b.txt 42 2026-07-01 12:00:00Z al $"
    );
}

#[test]
fn eol_consistency_check_matches_propset() {
    assert!(check_eol_consistency(b"a\nb\n").is_ok());
    assert!(check_eol_consistency(b"a\r\nb\r\n").is_ok());
    assert!(check_eol_consistency(b"a\rb\r").is_ok());
    assert!(check_eol_consistency(b"").is_ok());
    assert!(matches!(
        check_eol_consistency(b"a\nb\r\n"),
        Err(TranslateError::InconsistentEol)
    ));
    assert!(matches!(
        check_eol_consistency(b"a\rb\n"),
        Err(TranslateError::InconsistentEol)
    ));
}

#[test]
fn errors_convert_to_svn_error_codes() {
    use svn_types::{ErrorCode, SvnError};
    let e: SvnError = TranslateError::InconsistentEol.into();
    assert_eq!((e.code, e.svn_code), (ErrorCode::Corrupt, Some(135_000)));
    let e: SvnError = TranslateError::UnknownEol.into();
    assert_eq!((e.code, e.svn_code), (ErrorCode::Corrupt, Some(135_001)));
    let io = std::io::Error::from(std::io::ErrorKind::NotFound);
    let e: SvnError = TranslateError::Stream {
        op: "read",
        source: io,
    }
    .into();
    assert_eq!(e.code, ErrorCode::PathNotFound);
}

#[test]
fn special_install_keeps_full_content_for_regular_files() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("q");
    special::install(&p, b"unknown kind\nsecond line\n").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"unknown kind\nsecond line\n");
}

#[cfg(windows)]
#[test]
fn windows_read_only_attribute_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f");
    std::fs::write(&p, b"x").unwrap();
    flags::set_read_only(&p).unwrap();
    assert!(std::fs::metadata(&p).unwrap().permissions().readonly());
    flags::set_read_write(&p).unwrap();
    assert!(!std::fs::metadata(&p).unwrap().permissions().readonly());
    // The executable flag does not exist on Windows.
    flags::set_executable(&p, true).unwrap();
}

#[cfg(windows)]
#[test]
fn windows_special_install_writes_the_repo_normal_text() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("l");
    special::install(&p, b"link dest/x\nrest").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"link dest/x\nrest");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn engine_never_panics_and_contraction_is_idempotent(
        data in proptest::collection::vec(prop_oneof![
            Just(b'$'), Just(b':'), Just(b' '), Just(b'\n'), Just(b'\r'), Just(b'#'),
            Just(b'I'), Just(b'd'), Just(b'R'), Just(b'e'), Just(b'v'), any::<u8>()], 0..300),
        style in prop_oneof![Just(EolStyle::None), Just(EolStyle::Native), Just(EolStyle::Lf),
                              Just(EolStyle::Cr), Just(EolStyle::CrLf)],
    ) {
        let i = info(style, Some("Id Rev"));
        let w = to_working(&i, &data, &ctx(), &utc_offset_zero).unwrap();
        prop_assert!(w.len() <= data.len() * 2 + 4 * 255);
        let n = to_repo_normal(&i, &data, true).unwrap();
        prop_assert_eq!(to_repo_normal(&i, &n, true).unwrap(), n);
    }

    #[test]
    fn eol_roundtrip(lines in proptest::collection::vec("[a-z ]{0,8}", 0..12), trailing in any::<bool>(),
                     style in prop_oneof![Just(EolStyle::Native), Just(EolStyle::Lf),
                                           Just(EolStyle::Cr), Just(EolStyle::CrLf)],
                     native_crlf in any::<bool>()) {
        let native: &'static [u8] = if native_crlf { b"\r\n" } else { b"\n" };
        let eol: &[u8] = match style {
            EolStyle::Native | EolStyle::Lf => b"\n",
            EolStyle::Cr => b"\r",
            _ => b"\r\n",
        };
        let mut repo = Vec::new();
        for (idx, l) in lines.iter().enumerate() {
            repo.extend_from_slice(l.as_bytes());
            if idx + 1 < lines.len() || trailing { repo.extend_from_slice(eol); }
        }
        let i = info(style, None);
        let working = to_working_with_native(&i, &repo, &ctx(), &utc_offset_zero, native).unwrap();
        prop_assert_eq!(to_repo_normal(&i, &working, false).unwrap(), repo);
    }

    #[test]
    fn keyword_roundtrip(prefix in "[a-z ]{0,5}", suffix in "[a-z ]{0,5}", which in 0usize..6) {
        let names = ["Id", "Rev", "Date", "Author", "URL", "Header"];
        let src = format!("{prefix}${}${suffix}", names[which]);
        let i = info(EolStyle::None, Some("Id Rev Date Author URL Header"));
        let w = to_working(&i, src.as_bytes(), &ctx(), &utc_offset_zero).unwrap();
        prop_assert!(w.len() > src.len());
        prop_assert_eq!(to_repo_normal(&i, &w, false).unwrap(), src.as_bytes());
    }
}
