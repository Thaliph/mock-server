//! GW-01: the name of the generated objects (GEP-1762, spec §7.10 "Generated objects").

use super::objects::name;

#[test]
fn gw_01_short_names_get_the_mimic_suffix() {
    assert_eq!(name("web"), "web-mimic");
    let long = "a".repeat(57);
    let got = name(&long);
    assert_eq!(got, format!("{long}-mimic"));
    assert_eq!(got.len(), 63);
}

#[test]
fn gw_01_other_names_get_the_hashed_form() {
    let cases = [
        (
            "a".repeat(58),
            format!("{}-d5c039b748-mimic", "a".repeat(46)),
        ),
        ("a.b".into(), "a-b-2e7336dc8e-mimic".into()),
        ("1web".into(), "g1web-bac40f31e8-mimic".into()),
        (
            "web.prod.example".into(),
            "web-prod-example-f17b22d722-mimic".into(),
        ),
        // The 46-character cut ends with `-`, which is trimmed.
        (
            format!("{}-{}", "a".repeat(45), "b".repeat(12)),
            format!("{}-eba427ac15-mimic", "a".repeat(45)),
        ),
    ];
    for (input, want) in cases {
        assert_eq!(name(&input), want, "name of {input}");
    }
    assert_eq!(name(&"a".repeat(58)).len(), 63);
}
