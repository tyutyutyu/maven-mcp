use super::*;

#[test]
fn xml_structural_and_aggregate_boundaries() {
    let nested = |n: usize| format!("{}{}", "<a>".repeat(n), "</a>".repeat(n));
    validate_xml_structure(&nested(MAX_XML_DEPTH)).unwrap();
    assert!(
        validate_xml_structure(&nested(MAX_XML_DEPTH + 1))
            .unwrap_err()
            .to_string()
            .contains("MAX_XML_DEPTH")
    );
    validate_xml_structure(&format!("<a>{}</a>", "<b/>".repeat(MAX_XML_ELEMENTS - 1))).unwrap();
    assert!(
        validate_xml_structure(&format!("<a>{}</a>", "<b/>".repeat(MAX_XML_ELEMENTS)))
            .unwrap_err()
            .to_string()
            .contains("MAX_XML_ELEMENTS")
    );
    let root = tempfile::TempDir::new().unwrap();
    let path = root.path().join("a.xml");
    std::fs::write(&path, "<a/>").unwrap();
    read_bounded_xml_with_limit(&path, usize::MAX).unwrap();
    assert!(validate_resource_limit("MAX_XML_BYTES", usize::MAX, MAX_XML_TOTAL_BYTES).is_err());
    let mut budget = XmlBudget {
        bytes: MAX_XML_TOTAL_BYTES - 4,
        files: 0,
    };
    budget.read(&path).unwrap();
    assert!(
        budget
            .read(&path)
            .unwrap_err()
            .to_string()
            .contains("MAX_XML_TOTAL_BYTES")
    );
    let mut budget = XmlBudget {
        bytes: 0,
        files: MAX_XML_FILES - 1,
    };
    budget.read(&path).unwrap();
    assert!(
        budget
            .read(&path)
            .unwrap_err()
            .to_string()
            .contains("MAX_XML_FILES")
    );
}

#[test]
fn xml_reads_accept_the_limit_and_reject_one_byte_more() {
    let directory = std::env::temp_dir().join(format!("xml-limit-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("a.xml");
    std::fs::write(&path, "<a/>").unwrap();
    assert_eq!(read_bounded_xml_with_limit(&path, 4).unwrap(), "<a/>");
    let error = read_bounded_xml_with_limit(&path, 3).unwrap_err();
    assert!(error.to_string().contains("MAX_XML_BYTES"));
    std::fs::remove_dir_all(&directory).unwrap();
}
