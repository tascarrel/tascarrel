//! GraphQL request admission for body-constrained secret-injection rules.
//!
//! [`admit_queries_only`] validates one JSON GraphQL request and admits only a
//! uniquely selected query operation.

use apollo_parser::Parser;
use apollo_parser::cst;
use serde::Deserialize;
use thiserror::Error;

/// Validates a JSON GraphQL request and admits only query operations.
pub(crate) fn admit_queries_only(body: &[u8]) -> Result<(), GraphQlAdmissionError> {
    let envelope = serde_json::from_slice::<GraphQlEnvelope>(body)
        .map_err(|_| GraphQlAdmissionError::InvalidEnvelope)?;
    let syntax = Parser::new(&envelope.query)
        .token_limit(MAX_GRAPHQL_TOKENS)
        .parse();
    if syntax.errors().next().is_some() {
        return Err(GraphQlAdmissionError::InvalidDocument);
    }

    let mut operations = Vec::new();
    for definition in syntax.document().definitions() {
        match definition {
            cst::Definition::OperationDefinition(operation) => operations.push(operation),
            cst::Definition::FragmentDefinition(_) => {}
            _ => return Err(GraphQlAdmissionError::InvalidDocument),
        }
    }
    if operations.iter().any(|operation| !is_query(operation)) {
        return Err(GraphQlAdmissionError::NonQueryOperation);
    }
    if operations.len() > 1
        && operations
            .iter()
            .any(|operation| operation.name().is_none())
    {
        return Err(GraphQlAdmissionError::InvalidOperationSelection);
    }

    let selected = match envelope.operation_name {
        Some(name) => operations
            .iter()
            .filter(|operation| {
                operation
                    .name()
                    .is_some_and(|operation_name| operation_name.text() == name)
            })
            .count(),
        None => operations.len(),
    };
    if selected != 1 {
        return Err(GraphQlAdmissionError::InvalidOperationSelection);
    }
    Ok(())
}

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum GraphQlAdmissionError {
    #[error("GraphQL request body is not a JSON request object")]
    InvalidEnvelope,
    #[error("GraphQL request document is invalid")]
    InvalidDocument,
    #[error("GraphQL request does not select exactly one operation")]
    InvalidOperationSelection,
    #[error("GraphQL request contains a non-query operation")]
    NonQueryOperation,
}

const MAX_GRAPHQL_TOKENS: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlEnvelope {
    query: String,
    operation_name: Option<String>,
}

fn is_query(operation: &cst::OperationDefinition) -> bool {
    operation
        .operation_type()
        .is_none_or(|operation_type| operation_type.query_token().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the query forms used by GitHub CLI and anonymous clients.
    #[test]
    fn admits_single_queries() {
        admit_queries_only(br#"{"query":"query UserCurrent{viewer{login}}"}"#).unwrap();
        admit_queries_only(br#"{"query":"{ viewer { login } }","variables":{}}"#).unwrap();
    }

    /// Verifies an operation name selects exactly one query from a document.
    #[test]
    fn admits_one_named_query() {
        admit_queries_only(
            br#"{"query":"query First { viewer { login } } query Second { viewer { id } }","operationName":"Second"}"#,
        )
        .unwrap();
        assert_eq!(
            admit_queries_only(
                br#"{"query":"query First { viewer { login } } query Second { viewer { id } }"}"#,
            ),
            Err(GraphQlAdmissionError::InvalidOperationSelection)
        );
    }

    /// Verifies mutations and subscriptions are denied even when not selected.
    #[test]
    fn rejects_non_query_operations() {
        for body in [
            br#"{"query":"mutation { createIssue(input: {}) { clientMutationId } }"}"#.as_slice(),
            br#"{"query":"subscription { notifications { id } }"}"#.as_slice(),
            br#"{"query":"query Read { viewer { login } } mutation Write { deleteIssue(input: {}) { clientMutationId } }","operationName":"Read"}"#.as_slice(),
        ] {
            assert_eq!(
                admit_queries_only(body),
                Err(GraphQlAdmissionError::NonQueryOperation)
            );
        }
    }

    /// Verifies parsing, rather than textual matching, determines operation
    /// type.
    #[test]
    fn ignores_operation_words_outside_operation_types() {
        admit_queries_only(
            br#"{"query":"query { repository(name: \"mutation\") { id } } # mutation"}"#,
        )
        .unwrap();
    }

    /// Verifies malformed envelopes, documents, and selections fail closed.
    #[test]
    fn rejects_invalid_requests() {
        for body in [
            br#"[]"#.as_slice(),
            br#"{"variables":{}}"#.as_slice(),
            br#"{"query":"query {"}"#.as_slice(),
            br#"{"query":"fragment User on User { login }"}"#.as_slice(),
            br#"{"query":"query Read { viewer { login } }","operationName":"Missing"}"#.as_slice(),
            br#"{"query":"{ viewer { id } } query Read { viewer { login } }","operationName":"Read"}"#.as_slice(),
        ] {
            assert!(admit_queries_only(body).is_err());
        }
    }
}
