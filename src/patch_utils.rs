//! Utility functions for patch operations
//!
//! Provides helper functions for common patch-related operations
//! like path normalization, conflict detection, and result formatting.

use std::path::{Path, PathBuf};

/// Normalizes a path for consistent comparison across platforms.
/// Converts to Unix-style separators and removes redundant components.
pub fn normalize_path(path: &str) -> PathBuf {
    Path::new(path).components().collect::<PathBuf>()
}

/// Checks if two paths refer to the same location.
/// Normalizes both paths before comparison.
pub fn paths_equivalent(a: &str, b: &str) -> bool {
    normalize_path(a) == normalize_path(b)
}

/// Formats a patch outcome summary as a human-readable string.
pub fn format_outcome_summary(outcome: &crate::applier::PatchOutcome) -> String {
    let ops = &outcome.operations;
    let total = ops.len();
    let successful = ops
        .iter()
        .filter(|o| o.status == crate::applier::OpStatus::Ok)
        .count();
    let failed = ops
        .iter()
        .filter(|o| {
            matches!(
                o.status,
                crate::applier::OpStatus::FatalError | crate::applier::OpStatus::RecoverableError
            )
        })
        .count();

    format!(
        "PatchOutcome: {} total, {} successful, {} failed",
        total, successful, failed
    )
}

/// Calculates the success rate as a percentage.
pub fn success_rate(outcome: &crate::applier::PatchOutcome) -> f32 {
    if outcome.operations.is_empty() {
        return 0.0;
    }
    let successful = outcome
        .operations
        .iter()
        .filter(|o| o.status == crate::applier::OpStatus::Ok)
        .count();
    (successful as f32 / outcome.operations.len() as f32) * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_path_simple() {
        let result = normalize_path("src/foo.rs");
        assert_eq!(result, PathBuf::from("src/foo.rs"));
    }

    #[test]
    fn test_paths_equivalent() {
        assert!(paths_equivalent("src/foo.rs", "src/foo.rs"));
        assert!(!paths_equivalent("src/foo.rs", "src/bar.rs"));
    }

    #[test]
    fn test_success_rate_empty() {
        let empty = crate::applier::PatchOutcome { operations: vec![] };
        assert_eq!(success_rate(&empty), 0.0);
    }
}
