//! RTY-06: the stripper of the `x-mimic-*` fields of a response head.

use std::borrow::Cow;

use proptest::prelude::*;

use super::strip::strip;

fn run(head: &[u8]) -> (Vec<u8>, usize) {
    let (out, n) = strip(head);
    (out.into_owned(), n)
}

#[test]
fn rty_06_the_fields_go_whatever_their_case_and_every_other_byte_stays() {
    let head = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nx-mimic-error: overloaded\r\nSet-Cookie: a=1\r\nX-Mimic-Endpoint: 10.0.0.9:80\r\nSet-Cookie: b=2\r\nX-MIMIC-other: 1\r\n\r\n";
    let (out, n) = run(head);
    assert_eq!(
        out,
        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\n"
    );
    assert_eq!(n, 3);
}

#[test]
fn rty_06_a_head_without_such_a_field_is_unchanged_and_not_copied() {
    for head in [
        &b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"[..],
        b"HTTP/1.1 200 OK\r\nx-mimic: 1\r\nX-Mimicry: 2\r\nx_mimic-a: 3\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nAccept: x-mimic-a\r\n\r\n",
        b"",
        b"\r\n",
    ] {
        let (out, n) = strip(head);
        assert!(matches!(out, Cow::Borrowed(_)), "no copy");
        assert_eq!(&*out, head);
        assert_eq!(n, 0);
    }
}

#[test]
fn rty_06_line_ends_and_other_folds_are_kept() {
    let head = b"HTTP/1.1 200 OK\nx-mimic-a: 1\nX-Other: a\n  folded\n\tagain\nx-mimic-b: 2\n\n";
    let (out, n) = run(head);
    assert_eq!(out, b"HTTP/1.1 200 OK\nX-Other: a\n  folded\n\tagain\n\n");
    assert_eq!(n, 2);
    let mixed = b"HTTP/1.1 200 OK\r\nx-mimic-a: 1\nHost: a\r\n\r\n";
    assert_eq!(run(mixed).0, b"HTTP/1.1 200 OK\r\nHost: a\r\n\r\n");
}

#[test]
fn rty_06_the_fold_of_a_removed_field_goes_with_it() {
    let head = b"HTTP/1.1 200 OK\r\nx-mimic-a: one\r\n two\r\n\tthree\r\nKeep: 1\r\n folded\r\n\r\n";
    let (out, n) = run(head);
    assert_eq!(out, b"HTTP/1.1 200 OK\r\nKeep: 1\r\n folded\r\n\r\n");
    assert_eq!(n, 1, "one field, three lines");
}

#[test]
fn rty_06_the_status_line_and_what_follows_the_head_are_left_alone() {
    let head = b"x-mimic-a HTTP/1.1 200 OK\r\nx-mimic-b: 1\r\n\r\nx-mimic-c: body\r\n";
    let (out, n) = run(head);
    assert_eq!(out, b"x-mimic-a HTTP/1.1 200 OK\r\n\r\nx-mimic-c: body\r\n");
    assert_eq!(n, 1);
}

#[test]
fn rty_06_duplicates_count_one_each() {
    let head = b"HTTP/1.1 200 OK\r\nx-mimic-a: 1\r\nx-mimic-a: 2\r\n\r\n";
    assert_eq!(run(head), (b"HTTP/1.1 200 OK\r\n\r\n".to_vec(), 2));
}

#[test]
fn rty_06_a_last_line_without_an_end_is_a_line() {
    let head = b"HTTP/1.1 200 OK\r\nx-mimic-a: 1";
    assert_eq!(run(head), (b"HTTP/1.1 200 OK\r\n".to_vec(), 1));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn rty_06_stripping_a_stripped_head_changes_nothing(head in "(?s)[ -~\r\n\t]{0,200}|HTTP/1.1 200 OK\r\n((x-mimic-|X-Mimic-|a-)[a-z]{0,3}: ?[a-z]{0,3}(\r\n|\n)( [a-z]{1,3}(\r\n|\n))?){0,6}(\r\n|\n)") {
        let (once, _) = run(head.as_bytes());
        let (twice, n) = strip(&once);
        prop_assert!(matches!(twice, Cow::Borrowed(_)));
        prop_assert_eq!(n, 0);
    }
}
