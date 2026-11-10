use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};

const START: &str = "https://provision.example.test/start";
const FINISH: &str = "https://provision.example.test/finish";

fn document(fields: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict>{fields}</dict></plist>"
    )
}

fn lookup(start: &str, finish: &str) -> String {
    document(&format!(
        "<key>urls</key><dict><key>midStartProvisioning</key><string>{}</string><key>midFinishProvisioning</key><string>{}</string></dict>",
        quick_xml::escape::escape(start),
        quick_xml::escape::escape(finish),
    ))
}

#[test]
fn lookup_candidates_do_not_grant_production_approval() {
    let candidates = LookupCandidates::parse(200, lookup(START, FINISH).as_bytes()).unwrap();
    assert_eq!(candidates.start(), START);
    assert_eq!(candidates.finish(), FINISH);
    assert!(
        ReviewedEndpointPolicy::production()
            .approve(&candidates)
            .is_err()
    );
    let suffix = LookupCandidates::parse(
        200,
        lookup(
            "https://unreviewed.apple.com/start",
            "https://unreviewed.apple.com/finish",
        )
        .as_bytes(),
    )
    .unwrap();
    assert!(
        ReviewedEndpointPolicy::production()
            .approve(&suffix)
            .is_err()
    );
}

#[test]
fn reviewed_policy_matches_the_complete_pair() {
    let policy = ReviewedEndpointPolicy {
        pairs: &[(START, FINISH)],
    };
    let candidates = LookupCandidates::parse(200, lookup(START, FINISH).as_bytes()).unwrap();
    let approved = policy.approve(&candidates).unwrap();
    assert_eq!(approved.start(), START);
    assert_eq!(approved.finish(), FINISH);
    for (start, finish) in [
        (FINISH, START),
        ("https://provision.example.test/other", FINISH),
        (START, "https://provision.example.test/finish?other=1"),
        ("https://provision.example.test.evil.test/start", FINISH),
    ] {
        let candidates = LookupCandidates::parse(200, lookup(start, finish).as_bytes()).unwrap();
        assert!(policy.approve(&candidates).is_err());
    }
}

#[test]
fn lookup_requires_success_and_exact_required_types() {
    let body = lookup(START, FINISH);
    for status in [0, 201, 301, 401, 500] {
        assert!(LookupCandidates::parse(status, body.as_bytes()).is_err());
    }
    for body in [
        document(""),
        document("<key>urls</key><string>value</string>"),
        document("<key>urls</key><dict/>"),
        lookup(START, FINISH)
            .replace("<string>", "<integer>")
            .replace("</string>", "</integer>"),
        lookup(START, FINISH).replace("midFinishProvisioning", "wrongKey"),
    ] {
        assert!(LookupCandidates::parse(200, body.as_bytes()).is_err());
    }
}

#[test]
fn url_filter_rejects_ambiguous_or_non_https_destinations() {
    for url in [
        "http://example.test/start",
        "https:example.test/start",
        "//example.test/start",
        "https://user@example.test/start",
        "https://user:secret@example.test/start",
        "https://@example.test/start",
        "https://example.test:444/start",
        "https://example.test:/start",
        "https://example.test:0443/start",
        "https://example.test/start#",
        "https://example.test/start#fragment",
        "https://127.0.0.1/start",
        "https://2130706433/start",
        "https://0x7f000001/start",
        "https://[::1]/start",
        "https://%65xample.test/start",
        "https://example.test./start",
        "https://-example.test/start",
        "https://example_.test/start",
        "https://example.test\\@evil.test/start",
        "https://example.test/\nstart",
        " https://example.test/start",
        "https://example.test/start ",
        "https://example.test/%0astart",
        "https://example.test/%5Cstart",
        "https://example.test/%",
        "https://example.test/%0",
        "https://example.test/%GG",
        "https://éxample.test/start",
        "https://example.test/\u{7f}start",
    ] {
        assert!(
            candidate_url(url).is_err(),
            "accepted synthetic URL: {url:?}"
        );
    }
    assert!(candidate_url(&format!("https://{}.test/start", "a".repeat(64))).is_err());
    assert!(candidate_url(&format!("{START}{}", "a".repeat(MAX_URL_BYTES))).is_err());
    assert_eq!(
        candidate_url("https://provision.example.test:443/start")
            .unwrap()
            .as_str(),
        START
    );
}

#[test]
fn xml_rejects_duplicate_dictionary_keys_at_every_level() {
    let fields = "<key>a</key><string>first</string><key>a</key><string>second</string>";
    for body in [
        document(fields),
        document(&format!("<key>nested</key><dict>{fields}</dict>")),
        lookup(START, FINISH).replace("midFinishProvisioning", "midStartProvisioning"),
        lookup(START, FINISH).replace("</dict></plist>", "<key>urls</key><dict/></dict></plist>"),
        document("<key>a</key><string>first</string><key>&#97;</key><string>second</string>"),
    ] {
        assert!(plist::parse(body.as_bytes(), MAX_LOOKUP_BYTES).is_err());
    }
}

#[test]
fn xml_accepts_only_inert_standard_doctype_and_builtin_references() {
    let body = lookup(&format!("{START}?a=1&b=2"), FINISH);
    let doctype = "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">";
    let body = body.replace("?><plist", &format!("?>{doctype}<plist"));
    assert_eq!(
        LookupCandidates::parse(200, body.as_bytes())
            .unwrap()
            .start(),
        format!("{START}?a=1&b=2")
    );
    for dtd in [
        "<!DOCTYPE plist [<!ENTITY secret SYSTEM 'file:///synthetic-secret'>]>",
        "<!DOCTYPE plist [<!ENTITY bomb 'repeated'>]>",
        "<!DOCTYPE plist SYSTEM 'https://unreviewed.example.test/external.dtd'>",
    ] {
        let body = lookup(START, FINISH).replace("?><plist", &format!("?>{dtd}<plist"));
        assert!(LookupCandidates::parse(200, body.as_bytes()).is_err());
    }
    for reference in ["&unknown;", "&#0;", "&#1;", "&#xD800;", "&#x110000;"] {
        assert!(
            plist::parse(
                document(&format!("<key>x</key><string>{reference}</string>")).as_bytes(),
                MAX_LOOKUP_BYTES
            )
            .is_err()
        );
    }
}

#[test]
fn xml_rejects_malformed_documents_and_unexpected_structure() {
    for body in [
        "",
        "<plist version=\"1.0\"><dict>",
        "<plist version=\"1.0\"><dict></array></plist>",
        "<plist version=\"1.0\"><dict/><dict/></plist>",
        "<plist version=\"1.0\"><array/></plist>",
        "<plist version=\"1.0\"><dict><key>x</key></dict></plist>",
        "<plist version=\"1.0\"><dict><string>x</string><string>y</string></dict></plist>",
        "<plist version=\"1.0\"><dict bad='attribute'/></plist>",
        "<plist version=\"1.0\" version=\"1.0\"><dict/></plist>",
        "<plist version=\"1.0\"><dict/></plist><extra/>",
        "<?xml version='1.0' encoding='UTF-16'?><plist version='1.0'><dict/></plist>",
        "<?xml version='1.0' version='1.0'?><plist version='1.0'><dict/></plist>",
    ] {
        assert!(plist::parse(body.as_bytes(), MAX_LOOKUP_BYTES).is_err());
    }
    for scalar in [
        "<string>bad]]>text</string>",
        "<string><string>nested</string></string>",
        "<string>\u{1}</string>",
        "<string>&unterminated</string>",
        "<integer>not-an-integer</integer>",
    ] {
        assert!(
            plist::parse(
                document(&format!("<key>x</key>{scalar}")).as_bytes(),
                MAX_LOOKUP_BYTES
            )
            .is_err()
        );
    }
    assert!(plist::parse(b"\xff", MAX_LOOKUP_BYTES).is_err());
}

#[test]
fn xml_bounds_bytes_nesting_and_elements() {
    let body = lookup(START, FINISH);
    assert!(plist::parse(body.as_bytes(), body.len()).is_ok());
    assert!(plist::parse(body.as_bytes(), body.len() - 1).is_err());
    let deep = document(&format!(
        "<key>x</key>{}{}",
        "<array>".repeat(17),
        "</array>".repeat(17)
    ));
    assert!(plist::parse(deep.as_bytes(), MAX_LOOKUP_BYTES).is_err());
    let many = document(&format!(
        "<key>x</key><array>{}</array>",
        "<true/>".repeat(4096)
    ));
    assert!(plist::parse(many.as_bytes(), MAX_LOOKUP_BYTES).is_err());
    let large = document(&format!(
        "<key>x</key><string>{}</string>",
        "a".repeat(MAX_LOOKUP_BYTES)
    ));
    assert!(LookupCandidates::parse(200, large.as_bytes()).is_err());
}

#[test]
fn parser_errors_do_not_include_supplied_content() {
    let secret = "synthetic-sensitive-value";
    let body = document(&format!("<key>{secret}</key><string>&{secret};</string>"));
    let error = LookupCandidates::parse(200, body.as_bytes()).err().unwrap();
    assert_eq!(error, "Apple provisioning plist is invalid");
    assert!(!error.contains(secret));
}

fn response(fields: &str) -> String {
    document(&format!(
        "<key>Response</key><dict><key>Status</key><dict><key>ec</key><integer>0</integer></dict>{fields}</dict>"
    ))
}

fn start_response(value: &str) -> String {
    response(&format!("<key>spim</key><string>{value}</string>"))
}

fn finish_response(ptm: &str, tk: &str) -> String {
    response(&format!(
        "<key>ptm</key><string>{ptm}</string><key>tk</key><string>{tk}</string><key>X-Apple-I-MD-RINFO</key><integer>17106176</integer>"
    ))
}

#[test]
fn encoders_produce_the_required_request_dictionaries() {
    let start = encode_start_request();
    let root = plist::parse(start.as_bytes(), MAX_RESPONSE_BYTES).unwrap();
    assert!(
        matches!(root.field("Header").unwrap(), plist::Value::Dictionary(value) if value.is_empty())
    );
    assert!(
        matches!(root.field("Request").unwrap(), plist::Value::Dictionary(value) if value.is_empty())
    );
    let finish = encode_finish_request("YQ==").unwrap();
    let root = plist::parse(finish.as_bytes(), MAX_RESPONSE_BYTES).unwrap();
    assert!(
        matches!(root.field("Header").unwrap(), plist::Value::Dictionary(value) if value.is_empty())
    );
    assert_eq!(
        root.field("Request")
            .unwrap()
            .field("cpim")
            .unwrap()
            .string()
            .unwrap(),
        "YQ=="
    );
}

#[test]
fn successful_responses_return_existing_typed_data() {
    let _: StartProvisioningData =
        parse_start_response(200, start_response("YQ==").as_bytes()).unwrap();
    let _: EndProvisioningData =
        parse_finish_response(200, finish_response("Yg==", "Yw==").as_bytes()).unwrap();
}

#[test]
fn response_requires_both_http_success_and_signed_zero_status() {
    let body = start_response("YQ==");
    for http_status in [0, 201, 301, 401, 500] {
        assert!(parse_start_response(http_status, body.as_bytes()).is_err());
        assert!(
            parse_finish_response(http_status, finish_response("Yg==", "Yw==").as_bytes()).is_err()
        );
    }
    for status in [
        "<integer>1</integer>",
        "<integer>-1</integer>",
        "<string>0</string>",
        "<true/>",
        "<integer>9223372036854775808</integer>",
    ] {
        assert!(
            parse_start_response(200, body.replace("<integer>0</integer>", status).as_bytes())
                .is_err()
        );
        let finish = finish_response("Yg==", "Yw==").replace("<integer>0</integer>", status);
        assert!(parse_finish_response(200, finish.as_bytes()).is_err());
    }
    for body in [
        body.replace(
            "<key>Status</key><dict><key>ec</key><integer>0</integer></dict>",
            "",
        ),
        body.replace("<key>ec</key><integer>0</integer>", ""),
        body.replace("<key>Response</key>", "<key>WrongResponse</key>"),
    ] {
        assert!(parse_start_response(200, body.as_bytes()).is_err());
        assert!(parse_finish_response(200, body.as_bytes()).is_err());
    }
}

#[test]
fn response_rejects_duplicate_required_fields() {
    for body in [
        start_response("YQ==").replace(
            "<key>spim</key>",
            "<key>spim</key><string>Yg==</string><key>spim</key>",
        ),
        start_response("YQ==").replace(
            "<key>ec</key>",
            "<key>ec</key><integer>1</integer><key>ec</key>",
        ),
        start_response("YQ==").replace(
            "<key>Status</key>",
            "<key>Status</key><dict/><key>Status</key>",
        ),
        start_response("YQ==").replace(
            "<key>Response</key>",
            "<key>Response</key><dict/><key>Response</key>",
        ),
    ] {
        assert!(parse_start_response(200, body.as_bytes()).is_err());
    }
    for field in ["ptm", "tk", ROUTING_INFO_HEADER] {
        let body = finish_response("Yg==", "Yw==").replace(
            &format!("<key>{field}</key>"),
            &format!("<key>{field}</key><string>YQ==</string><key>{field}</key>"),
        );
        assert!(parse_finish_response(200, body.as_bytes()).is_err());
    }
}

#[test]
fn response_rejects_missing_or_wrong_binary_field_types() {
    for body in [
        response(""),
        response("<key>spim</key><integer>1</integer>"),
        response("<key>spim</key><dict/>"),
    ] {
        assert!(parse_start_response(200, body.as_bytes()).is_err());
    }
    for body in [
        response("<key>ptm</key><string>Yg==</string>"),
        response("<key>tk</key><string>Yw==</string>"),
        response("<key>ptm</key><true/><key>tk</key><string>Yw==</string>"),
        response("<key>ptm</key><string>Yg==</string><key>tk</key><integer>1</integer>"),
    ] {
        assert!(parse_finish_response(200, body.as_bytes()).is_err());
    }
}

#[test]
fn all_binary_fields_require_canonical_nonempty_bounded_base64() {
    for value in [
        "",
        "a",
        "YQ",
        "YR==",
        "YQ==\n",
        "YQ==Yg==",
        "__8=",
        "<invalid/>",
    ] {
        assert!(encode_finish_request(value).is_err());
        assert!(parse_start_response(200, start_response(value).as_bytes()).is_err());
        assert!(parse_finish_response(200, finish_response(value, "YQ==").as_bytes()).is_err());
        assert!(parse_finish_response(200, finish_response("YQ==", value).as_bytes()).is_err());
    }
    let maximum = STANDARD.encode(vec![0x61; MAX_CPIM_BYTES]);
    assert!(encode_finish_request(&maximum).is_ok());
    assert!(parse_start_response(200, start_response(&maximum).as_bytes()).is_ok());
    assert!(parse_finish_response(200, finish_response(&maximum, &maximum).as_bytes()).is_ok());
    let oversized = STANDARD.encode(vec![0x61; MAX_CPIM_BYTES + 1]);
    assert!(encode_finish_request(&oversized).is_err());
    assert!(parse_start_response(200, start_response(&oversized).as_bytes()).is_err());
    assert!(parse_finish_response(200, finish_response(&oversized, "YQ==").as_bytes()).is_err());
    assert!(parse_finish_response(200, finish_response("YQ==", &oversized).as_bytes()).is_err());
}

#[test]
fn routing_field_requires_exact_response_key() {
    let body = finish_response("Yg==", "Yw==");
    for invalid in [
        body.replace("<key>X-Apple-I-MD-RINFO</key><integer>17106176</integer>", ""),
        body.replace("X-Apple-I-MD-RINFO", "x-apple-i-md-rinfo"),
        body.replace(
            "<key>Response</key>",
            "<key>X-Apple-I-MD-RINFO</key><integer>17106176</integer><key>Response</key>",
        ).replace("<key>tk</key><string>Yw==</string><key>X-Apple-I-MD-RINFO</key><integer>17106176</integer>", "<key>tk</key><string>Yw==</string>"),
    ] {
        assert!(parse_finish_response(200, invalid.as_bytes()).is_err());
    }
}

#[test]
fn routing_field_accepts_positive_u64_integer_or_strict_decimal_string() {
    let body = finish_response("Yg==", "Yw==");
    for value in [1, 17106176, i64::MAX as u64 + 1, u64::MAX] {
        for tag in ["integer", "string"] {
            let body = body.replace(
                "<integer>17106176</integer>",
                &format!("<{tag}>{value}</{tag}>"),
            );
            assert!(parse_finish_response(200, body.as_bytes()).is_ok());
            let root = successful_response(200, body.as_bytes()).unwrap();
            let routing = extract_routing_info(
                root.field("Response")
                    .unwrap()
                    .field(ROUTING_INFO_HEADER)
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(routing.value(), value);
        }
    }
    for value in [
        "<integer>0</integer>",
        "<integer>-1</integer>",
        "<integer>-9223372036854775809</integer>",
        "<integer>18446744073709551616</integer>",
        "<integer>340282366920938463463374607431768211455</integer>",
        "<string></string>",
        "<string>0</string>",
        "<string>-1</string>",
        "<string>+1</string>",
        "<string> 1</string>",
        "<string>1 </string>",
        "<string>1\r\n</string>",
        "<string>1,1</string>",
        "<string>1.0</string>",
        "<string>18446744073709551616</string>",
        "<string>000000000000000000001</string>",
        "<true/>",
        "<dict/>",
        "<array><integer>1</integer></array>",
    ] {
        let body = body.replace("<integer>17106176</integer>", value);
        assert!(parse_finish_response(200, body.as_bytes()).is_err());
    }
}

#[test]
fn response_body_is_bounded() {
    let large = "a".repeat(MAX_RESPONSE_BYTES);
    let start = response(&format!(
        "<key>spim</key><string>YQ==</string><key>extra</key><string>{large}</string>"
    ));
    assert!(parse_start_response(200, start.as_bytes()).is_err());
    let finish = finish_response("Yg==", "Yw==").replace(
        "<key>ptm</key>",
        &format!("<key>extra</key><string>{large}</string><key>ptm</key>"),
    );
    assert!(parse_finish_response(200, finish.as_bytes()).is_err());
}

#[test]
fn response_errors_discard_apple_and_routing_detail() {
    let secret = "synthetic-sensitive-value";
    let body = start_response("YQ==").replace(
        "<integer>0</integer>",
        &format!("<integer>1</integer><key>em</key><string>{secret}</string>"),
    );
    assert_eq!(
        parse_start_response(200, body.as_bytes()).err(),
        Some(INVALID_RESPONSE)
    );
    let body = finish_response("Yg==", "Yw==").replace(
        "<integer>17106176</integer>",
        &format!("<string>{secret}</string>"),
    );
    assert_eq!(
        parse_finish_response(200, body.as_bytes()).err(),
        Some(INVALID_ROUTING)
    );
}
