//! Host-enforced Git branch and tag publication policies.
//!
//! [`RepositoryPolicy`] compiles one workspace or repository policy into
//! ordered branch and tag matchers. Every unmatched reference requires
//! approval unless the configuration selects another default.

use globset::GlobBuilder;
use globset::GlobMatcher;
use reportify::Report;
use tascarrel_api::types::config as config_api;
use tascarrel_git::ReferenceName;
use thiserror::Error;

/// Aggregate action for one atomic Git publication.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum RepositoryPushPolicy {
    /// Publishes every update immediately.
    Allow,
    /// Retains every update until the user approves the publication.
    RequireApproval,
    /// Rejects every update without creating an approval.
    Deny,
}

/// Compiled ordered rules for one repository.
#[derive(Clone, Debug)]
pub(crate) struct RepositoryPolicy {
    default: RepositoryPushPolicy,
    branches: Vec<RepositoryPolicyRule>,
    tags: Vec<RepositoryPolicyRule>,
}

impl RepositoryPolicy {
    /// Compiles the raw configuration or returns the approval-preserving
    /// product default.
    pub(crate) fn from_config(
        config: Option<&config_api::WorkspaceGitConfig>,
    ) -> Result<Self, Report<RepositoryPolicyError>> {
        let Some(config) = config else {
            return Ok(Self::default());
        };
        let default = parse_policy(config.default_policy.as_deref())?;
        let branches = compile_rules(config.branches.as_deref(), "branch")?;
        let tags = compile_rules(config.tags.as_deref(), "tag")?;
        Ok(Self {
            default,
            branches,
            tags,
        })
    }

    /// Returns the most restrictive policy among one atomic set of updates.
    pub(crate) fn updates_policy<'a>(
        &self,
        references: impl IntoIterator<Item = &'a ReferenceName>,
    ) -> RepositoryPushPolicy {
        references
            .into_iter()
            .map(|reference| self.reference_policy(reference))
            .max()
            .unwrap_or(RepositoryPushPolicy::Allow)
    }

    /// Resolves one full branch or tag reference against its ordered rules.
    pub(crate) fn reference_policy(&self, reference: &ReferenceName) -> RepositoryPushPolicy {
        let (name, rules) = if let Some(name) = reference.as_str().strip_prefix("refs/heads/") {
            (name, &self.branches)
        } else if let Some(name) = reference.as_str().strip_prefix("refs/tags/") {
            (name, &self.tags)
        } else {
            return RepositoryPushPolicy::Deny;
        };
        rules
            .iter()
            .find(|rule| rule.matcher.is_match(name))
            .map_or(self.default, |rule| rule.policy)
    }
}

impl Default for RepositoryPolicy {
    fn default() -> Self {
        Self {
            default: RepositoryPushPolicy::RequireApproval,
            branches: Vec::new(),
            tags: Vec::new(),
        }
    }
}

/// Invalid Git publication policy configuration.
#[derive(Debug, Error)]
pub(crate) enum RepositoryPolicyError {
    /// A default or rule uses an unsupported action.
    #[error("invalid Git publication policy {0:?}")]
    InvalidPolicy(String),
    /// A branch or tag rule uses an unsupported glob.
    #[error("invalid Git {kind} pattern {pattern:?}")]
    InvalidPattern {
        /// Reference kind selected by the rule list.
        kind: &'static str,
        /// Rejected short-name pattern.
        pattern: String,
    },
    /// One rule list exceeds its configured bound.
    #[error("Git {kind} policy is limited to {MAX_RULES} rules")]
    RuleLimit {
        /// Reference kind selected by the oversized rule list.
        kind: &'static str,
    },
}

const MAX_RULES: usize = 256;
const MAX_PATTERN_BYTES: usize = 1024;

#[derive(Clone)]
struct RepositoryPolicyRule {
    pattern: String,
    matcher: GlobMatcher,
    policy: RepositoryPushPolicy,
}

impl std::fmt::Debug for RepositoryPolicyRule {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepositoryPolicyRule")
            .field("pattern", &self.pattern)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

fn compile_rules(
    rules: Option<&[config_api::WorkspaceGitRuleConfig]>,
    kind: &'static str,
) -> Result<Vec<RepositoryPolicyRule>, Report<RepositoryPolicyError>> {
    let rules = rules.unwrap_or_default();
    if rules.len() > MAX_RULES {
        return Err(Report::new(RepositoryPolicyError::RuleLimit { kind }));
    }
    rules
        .iter()
        .map(|rule| {
            let pattern = rule.pattern.to_string();
            let matcher = compile_pattern(&pattern, kind)?;
            Ok(RepositoryPolicyRule {
                pattern,
                matcher,
                policy: parse_policy(Some(rule.policy.as_ref()))?,
            })
        })
        .collect()
}

fn parse_policy(
    policy: Option<&str>,
) -> Result<RepositoryPushPolicy, Report<RepositoryPolicyError>> {
    match policy.unwrap_or("require-approval") {
        "allow" => Ok(RepositoryPushPolicy::Allow),
        "deny" => Ok(RepositoryPushPolicy::Deny),
        "require-approval" => Ok(RepositoryPushPolicy::RequireApproval),
        policy => Err(Report::new(RepositoryPolicyError::InvalidPolicy(
            policy.to_owned(),
        ))),
    }
}

fn compile_pattern(
    pattern: &str,
    kind: &'static str,
) -> Result<GlobMatcher, Report<RepositoryPolicyError>> {
    let invalid = || {
        Report::new(RepositoryPolicyError::InvalidPattern {
            kind,
            pattern: pattern.to_owned(),
        })
    };
    if pattern.is_empty() || pattern.len() > MAX_PATTERN_BYTES {
        return Err(invalid());
    }
    let candidate = pattern.replace('*', "x");
    let reference = match kind {
        "branch" => format!("refs/heads/{candidate}"),
        "tag" => format!("refs/tags/{candidate}"),
        _ => return Err(invalid()),
    };
    if ReferenceName::new(reference).is_err() {
        return Err(invalid());
    }

    let mut glob = String::new();
    let mut literal_start = 0;
    let bytes = pattern.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'*' {
            index += 1;
            continue;
        }
        glob.push_str(&globset::escape(&pattern[literal_start..index]));
        let star_start = index;
        while index < bytes.len() && bytes[index] == b'*' {
            index += 1;
        }
        let stars = index - star_start;
        let component_start = star_start == 0 || bytes[star_start - 1] == b'/';
        let component_end = index == bytes.len() || bytes[index] == b'/';
        match stars {
            1 => glob.push('*'),
            2 if component_start && component_end => glob.push_str("**"),
            _ => return Err(invalid()),
        }
        literal_start = index;
    }
    glob.push_str(&globset::escape(&pattern[literal_start..]));

    let mut builder = GlobBuilder::new(&glob);
    builder.literal_separator(true).backslash_escape(false);
    builder
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use tascarrel_api::ArcStr;

    use super::*;

    /// The absent policy retains the existing approval workflow.
    #[test]
    fn default_policy_requires_approval() {
        let policy = RepositoryPolicy::from_config(None).expect("compile default policy");

        assert_eq!(
            policy.reference_policy(&reference("refs/heads/main")),
            RepositoryPushPolicy::RequireApproval
        );
        assert_eq!(
            policy.reference_policy(&reference("refs/tags/v1")),
            RepositoryPushPolicy::RequireApproval
        );
    }

    /// Ordered rules distinguish branch and tag names and support globstars.
    #[test]
    fn ordered_branch_and_tag_rules_override_the_default() {
        let config = config_api::WorkspaceGitConfig {
            default_policy: Some(ArcStr::from("allow")),
            branches: Some(
                vec![
                    rule("automation/protected/**", "deny"),
                    rule("automation/**", "allow"),
                ]
                .into(),
            ),
            tags: Some(vec![rule("**", "require-approval")].into()),
        };
        let policy = RepositoryPolicy::from_config(Some(&config)).expect("compile policy");

        assert_eq!(
            policy.reference_policy(&reference("refs/heads/automation/protected/main")),
            RepositoryPushPolicy::Deny
        );
        assert_eq!(
            policy.reference_policy(&reference("refs/heads/automation/topic/nested")),
            RepositoryPushPolicy::Allow
        );
        assert_eq!(
            policy.reference_policy(&reference("refs/heads/main")),
            RepositoryPushPolicy::Allow
        );
        assert_eq!(
            policy.reference_policy(&reference("refs/tags/v1")),
            RepositoryPushPolicy::RequireApproval
        );
    }

    /// A single star does not cross a slash while a globstar does.
    #[test]
    fn pattern_components_have_path_glob_semantics() {
        let single = compile_pattern("automation/*", "branch").expect("compile single star");
        let recursive = compile_pattern("automation/**", "branch").expect("compile globstar");

        assert!(single.is_match("automation/topic"));
        assert!(!single.is_match("automation/team/topic"));
        assert!(recursive.is_match("automation/team/topic"));
    }

    /// The most restrictive matching rule controls one atomic multi-ref push.
    #[test]
    fn atomic_push_uses_the_most_restrictive_policy() {
        let config = config_api::WorkspaceGitConfig {
            default_policy: Some(ArcStr::from("allow")),
            branches: Some(vec![rule("main", "require-approval")].into()),
            tags: Some(vec![rule("blocked", "deny")].into()),
        };
        let policy = RepositoryPolicy::from_config(Some(&config)).expect("compile policy");
        let allowed = reference("refs/heads/topic");
        let protected = reference("refs/heads/main");
        let denied = reference("refs/tags/blocked");

        assert_eq!(
            policy.updates_policy([&allowed]),
            RepositoryPushPolicy::Allow
        );
        assert_eq!(
            policy.updates_policy([&allowed, &protected]),
            RepositoryPushPolicy::RequireApproval
        );
        assert_eq!(
            policy.updates_policy([&allowed, &protected, &denied]),
            RepositoryPushPolicy::Deny
        );
    }

    /// Invalid actions and malformed globstar placement fail configuration.
    #[test]
    fn invalid_policy_configuration_is_rejected() {
        assert!(parse_policy(Some("prompt")).is_err());
        assert!(compile_pattern("topic/**suffix", "branch").is_err());
        assert!(compile_pattern("../topic", "branch").is_err());
    }

    fn rule(pattern: &str, policy: &str) -> config_api::WorkspaceGitRuleConfig {
        config_api::WorkspaceGitRuleConfig {
            pattern: ArcStr::from(pattern),
            policy: ArcStr::from(policy),
        }
    }

    fn reference(value: &str) -> ReferenceName {
        ReferenceName::new(value).expect("create reference")
    }
}
