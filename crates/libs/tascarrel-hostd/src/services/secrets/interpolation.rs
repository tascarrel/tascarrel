//! Parsing and rendering for secret references embedded in environment values.

use reportify::Report;

use super::service::SecretReference;
use super::service::SecretsServiceError;

/// Parsed environment value containing literal spans and secret references.
pub(crate) struct SecretTemplate {
    parts: Vec<SecretTemplatePart>,
}

impl SecretTemplate {
    /// Parses `${secrets.<provider>.<name>}` references without interpreting
    /// ordinary shell text.
    pub(crate) fn parse(value: &str) -> Result<Self, Report<SecretsServiceError>> {
        let mut parts = Vec::new();
        let mut remaining = value;
        while let Some(start) = remaining.find("${secrets.") {
            if start > 0 {
                parts.push(SecretTemplatePart::Literal(remaining[..start].to_owned()));
            }
            let reference_start = start + "${secrets.".len();
            let reference_tail = &remaining[reference_start..];
            let Some(end) = reference_tail.find('}') else {
                return Err(SecretsServiceError::invalid_request(
                    "secret interpolation is missing a closing brace",
                ));
            };
            let reference = SecretReference::parse(&reference_tail[..end])?;
            parts.push(SecretTemplatePart::Secret(reference));
            remaining = &reference_tail[end + 1..];
        }
        if !remaining.is_empty() {
            parts.push(SecretTemplatePart::Literal(remaining.to_owned()));
        }
        Ok(Self { parts })
    }

    /// Returns the references in their rendering order.
    pub(crate) fn references(&self) -> impl Iterator<Item = &SecretReference> {
        self.parts.iter().filter_map(|part| match part {
            SecretTemplatePart::Literal(_) => None,
            SecretTemplatePart::Secret(reference) => Some(reference),
        })
    }

    /// Renders the value with plaintext obtained from the supplied resolver.
    pub(crate) fn render<'a>(
        &self,
        mut resolve: impl FnMut(&SecretReference) -> Option<&'a str>,
    ) -> Option<String> {
        let mut rendered = String::new();
        for part in &self.parts {
            match part {
                SecretTemplatePart::Literal(value) => rendered.push_str(value),
                SecretTemplatePart::Secret(reference) => rendered.push_str(resolve(reference)?),
            }
        }
        Some(rendered)
    }
}

/// One literal or provider-backed span of a parsed environment value.
enum SecretTemplatePart {
    Literal(String),
    Secret(SecretReference),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Secret interpolation preserves ordinary shell syntax while replacing
    /// multiple references.
    #[test]
    fn renders_only_tascarrel_secret_references() {
        let template =
            SecretTemplate::parse("Bearer ${secrets.project.TOKEN}:$HOME:${secrets.shared.SUFFIX}")
                .unwrap();
        let rendered = template
            .render(
                |reference| match (reference.provider(), reference.secret()) {
                    ("project", "TOKEN") => Some("alpha"),
                    ("shared", "SUFFIX") => Some("omega"),
                    _ => None,
                },
            )
            .unwrap();

        assert_eq!(rendered, "Bearer alpha:$HOME:omega");
    }
}
