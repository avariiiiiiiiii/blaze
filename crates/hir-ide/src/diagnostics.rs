//! Type inference-based diagnostics.
mod decl_check;
mod expr;

pub use crate::diagnostics::{
    decl_check::{CaseType, IncorrectCase, incorrect_case},
    expr::{
        BodyValidationDiagnostic, record_literal_missing_fields, record_pattern_missing_fields,
    },
};
