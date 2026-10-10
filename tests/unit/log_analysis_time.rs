use super::*;
#[test]
fn normalizes_offsets_epochs_and_calendar_edges() {
    assert_eq!(
        normalize_timestamp("2026-01-01T01:00:00+01:00"),
        normalize_timestamp("2026-01-01")
    );
    assert_eq!(
        normalize_timestamp("2026-01-01 00:00:00,123").as_deref(),
        Some("2026-01-01T00:00:00.123Z")
    );
    assert_eq!(
        normalize_timestamp("1767225600000"),
        normalize_timestamp("2026-01-01")
    );
    assert!(normalize_timestamp("2025-02-29").is_none());
    assert!(normalize_timestamp("2026-01-01T24:00:00Z").is_none());
    assert!(normalize_timestamp("2024-02-29").is_some());
}
