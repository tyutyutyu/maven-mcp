use super::*;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn accepts_only_full_lowercase_hexadecimal_commit_ids() {
    assert!(is_commit_id(COMMIT));
    assert!(!is_commit_id(""));
    assert!(!is_commit_id("0123456789ab"));
    assert!(!is_commit_id(&COMMIT.to_uppercase()));
    assert!(!is_commit_id(&format!("{COMMIT}0")));
    assert!(!is_commit_id(&COMMIT.replacen('0', "g", 1)));
    assert!(!is_commit_id(&format!("{}-dirty", &COMMIT[..34])));
}

#[test]
fn server_version_carries_the_short_commit_as_build_metadata() {
    assert_eq!(
        server_version_for("1.2.3", Some(COMMIT)),
        "1.2.3+0123456789ab"
    );
    assert_eq!(server_version_for("1.2.3", None), "1.2.3");
}

#[test]
fn version_label_names_the_full_commit_or_unknown() {
    assert_eq!(
        version_label_for("1.2.3", Some(COMMIT)),
        format!("1.2.3 (commit {COMMIT})")
    );
    assert_eq!(version_label_for("1.2.3", None), "1.2.3 (commit unknown)");
}

#[test]
fn running_build_identity_is_consistent() {
    assert_eq!(
        server_version(),
        server_version_for(PACKAGE_VERSION, build_commit())
    );
    assert_eq!(
        version_label(),
        version_label_for(PACKAGE_VERSION, build_commit())
    );
    assert!(build_commit().is_none_or(is_commit_id));
}
