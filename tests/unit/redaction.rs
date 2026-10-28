use std::collections::BTreeMap;

use super::*;

const MARKER: &str = "synthetic-review-value";

fn redacted(text: &str) -> (String, usize) {
    let (output, count) = redact_sensitive_values(text);
    assert!(!output.contains(MARKER), "{output}");
    (output, count)
}

#[test]
fn removes_the_complete_authorization_value_with_scheme_and_whitespace_variants() {
    for input in [
        "Authorization: Bearer synthetic-review-value",
        "authorization:Bearer synthetic-review-value",
        "AUTHORIZATION =  basic synthetic-review-value trailing",
        "Proxy-Authorization:\tBasic synthetic-review-value",
        r#"{"Authorization": "Bearer synthetic-review-value"}"#,
        "Authorization: Digest username=\"u\", response=synthetic-review-value",
    ] {
        let (output, count) = redacted(input);
        assert!(count >= 1, "{input} -> {output}");
        assert!(output.contains("<redacted>"), "{output}");
        assert!(!output.to_lowercase().contains("bearer"), "{output}");
        assert!(!output.to_lowercase().contains("basic"), "{output}");
    }
    assert_eq!(
        redacted("Authorization: Bearer synthetic-review-value\nnext line"),
        ("Authorization: <redacted>\nnext line".to_owned(), 1)
    );
}

#[test]
fn redacts_key_values_urls_xml_elements_and_known_tokens() {
    let cases = [
        ("password=synthetic-review-value", "password=<redacted>"),
        (
            "-Dgpg.passphrase=synthetic-review-value -X",
            "-Dgpg.passphrase=<redacted> -X",
        ),
        (
            "Invalid TOKEN: synthetic-review-value",
            "Invalid TOKEN: <redacted>",
        ),
        (
            "client_secret = 'synthetic-review-value'",
            "client_secret = <redacted>",
        ),
        (
            r#"{"password":"synthetic review value"}"#,
            r#"{"password":<redacted>}"#,
        ),
        (
            r#"{"apiKey": "synthetic-review-value"}"#,
            r#"{"apiKey": <redacted>}"#,
        ),
        (
            "expected secret: <synthetic-review-value> but",
            "expected secret: <redacted> but",
        ),
        (
            "<db.password>synthetic-review-value</db.password>",
            "<db.password><redacted></db.password>",
        ),
        (
            "https://user:synthetic-review-value@repo.example/x",
            "https://<credentials>@repo.example/x",
        ),
        (
            "https://user:p@ss-synthetic-review-value@repo.example",
            "https://<credentials>@repo.example",
        ),
        ("use ghp_syntheticreviewvalue now", "use <redacted> now"),
    ];
    for (input, expected) in cases {
        let (output, count) = redact_sensitive_values(input);
        assert_eq!(output, expected, "{input}");
        assert_eq!(count, 1, "{input}");
    }
}

#[test]
fn unquoted_values_with_punctuation_are_redacted_through_the_next_whitespace() {
    let cases = [
        (
            "-Dpassword=;synthetic-review-value -X",
            "-Dpassword=<redacted> -X",
        ),
        (
            "password=[REDACTED];synthetic-review-value",
            "password=<redacted>",
        ),
        (
            "token=a,synthetic-review-value next",
            "token=<redacted> next",
        ),
        ("secret=a&synthetic-review-value", "secret=<redacted>"),
        ("token=a)synthetic-review-value", "token=<redacted>"),
        (
            "fails[password=a;synthetic-review-value]",
            "fails[password=<redacted>]",
        ),
        (
            "Config{secret=a}synthetic-review-value} next",
            "Config{secret=<redacted>} next",
        ),
        ("(token=<a>;synthetic-review-value)", "(token=<redacted>)"),
    ];
    for (input, expected) in cases {
        let (output, count) = redacted(input);
        assert_eq!(output, expected, "{input}");
        assert_eq!(count, 1, "{input}");
        assert_eq!(redact_sensitive_values(&output), (output.clone(), 0));
    }
    assert_eq!(
        redact_sensitive_values("(password=)"),
        ("(password=)".to_owned(), 0)
    );
}

#[test]
fn quoted_values_with_escaped_quotes_and_backslashes_are_redacted_completely() {
    let cases = [
        (
            r#"{"password":"prefix\"synthetic-review-value"}"#,
            r#"{"password":<redacted>}"#,
        ),
        (
            r#"{"token": "a\\\"synthetic-review-value", "user": "u"}"#,
            r#"{"token": <redacted>, "user": "u"}"#,
        ),
        (
            r"secret='a\'synthetic-review-value' next",
            "secret=<redacted> next",
        ),
        (
            r#"password = "a\\" synthetic"#,
            "password = <redacted> synthetic",
        ),
    ];
    for (input, expected) in cases {
        let (output, count) = redact_sensitive_values(input);
        assert!(!output.contains(MARKER), "{output}");
        assert_eq!(output, expected, "{input}");
        assert_eq!(count, 1, "{input}");
    }
}

#[test]
fn unicode_input_never_panics_and_keeps_surrounding_text() {
    for input in [
        "İİİ password=synthetic-review-value ő",
        "ﬀ Authorization: Bearer synthetic-review-value",
        "ǅ TOKEN=ﬀsynthetic-review-value",
        "κ https://ü:synthetic-review-value@例え.jp/",
        "password=",
        "Authorization:",
        "İ",
    ] {
        redacted(input);
    }
    assert_eq!(
        redact_sensitive_values("İ password=é ő"),
        ("İ password=<redacted> ő".to_owned(), 1)
    );
}

#[test]
fn counts_only_actual_replacements_and_is_idempotent() {
    let input = "token=synthetic-review-value Authorization: Basic synthetic-review-value";
    let (once, count) = redacted(input);
    assert_eq!(count, 2, "{once}");
    let (twice, second_count) = redact_sensitive_values(&once);
    assert_eq!(twice, once);
    assert_eq!(second_count, 0);
    assert_eq!(redact_sensitive_values("nothing to hide").1, 0);
}

#[test]
fn keeps_diagnostics_that_only_mention_sensitive_words() {
    for input in [
        "[ERROR]   PasswordEncoderTest.encodes:42 expected: <1> but was: <2>",
        "java.lang.IllegalStateException: token expired",
        "Tests run: 3, Failures: 1, Errors: 0, Skipped: 0",
        "Downloading from central: https://repo.maven.apache.org/maven2/x.pom",
    ] {
        assert_eq!(redact_sensitive_values(input), (input.to_owned(), 0));
    }
}

#[test]
fn masks_sensitive_property_values_without_touching_other_properties() {
    let properties = BTreeMap::from([
        ("db.password".to_owned(), MARKER.to_owned()),
        ("gpg_PassPhrase".to_owned(), MARKER.to_owned()),
        ("sonar.login.token".to_owned(), MARKER.to_owned()),
        ("empty.secret".to_owned(), String::new()),
        (
            "repository.url".to_owned(),
            format!("https://deploy:{MARKER}@repo.example/releases"),
        ),
        ("java.version".to_owned(), "21".to_owned()),
        (
            "project.build.sourceEncoding".to_owned(),
            "UTF-8".to_owned(),
        ),
    ]);
    let redacted = redact_properties(properties);
    assert_eq!(redacted["db.password"], REDACTED);
    assert_eq!(redacted["gpg_PassPhrase"], REDACTED);
    assert_eq!(redacted["sonar.login.token"], REDACTED);
    assert_eq!(redacted["empty.secret"], "");
    assert_eq!(
        redacted["repository.url"],
        "https://<credentials>@repo.example/releases"
    );
    assert_eq!(redacted["java.version"], "21");
    assert_eq!(redacted["project.build.sourceEncoding"], "UTF-8");
    assert!(is_sensitive_name("X-API-Key"));
    assert!(!is_sensitive_name("maven.compiler.release"));
}
