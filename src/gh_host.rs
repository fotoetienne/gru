//! Resolves the `GH_HOST` value handed to interactive agent sessions.
//!
//! The autonomous paths (`gru do`, `gru review`) already know their host: it
//! came from the issue/PR URL or from the Minion's own remote. The user-facing
//! REPLs (`gru chat`, `gru pm`, `gru tpm`) don't — they start from a working
//! directory and an optional `--repo owner/repo`. This module turns that into
//! a host so `gh` inside the session targets the same GitHub instance the user
//! is actually working against instead of defaulting to github.com.

use crate::config::{self, HostRegistry, LabConfig};
use crate::git;

/// Where a resolved host came from. Used for the diagnostic message only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostSource {
    /// A `daemon.repos` entry naming this owner (or owner/repo).
    Config,
    /// A git remote in the checkout.
    Remote,
    /// The `GH_HOST` this process inherited.
    Inherited,
}

/// A resolved `GH_HOST`, or `None` when nothing in the environment identified
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedHost {
    pub(crate) host: String,
    pub(crate) source: HostSource,
}

/// Resolves the host for an interactive session from already-gathered inputs.
///
/// Precedence, highest first:
///
/// 1. **Config.** A `daemon.repos` entry for `owner` (preferring an exact
///    `owner/repo` match). The user wrote it down, so it beats anything
///    guessed — including when it resolves to `github.com` and the checkout's
///    remote points somewhere else entirely.
/// 2. **A remote belonging to the same owner.** When the session targets
///    `acme/widgets` and a remote is `https://ghe.example.com/acme/widgets`,
///    that remote identifies the host. Remotes for *other* owners are not
///    consulted at this step — an unrelated checkout must not decide the host
///    for a `--repo` the user named explicitly.
/// 3. **An inherited `GH_HOST`.** If the user already exported one, honour it
///    rather than overriding it with a guess.
/// 4. **Any recognized remote**, but only when no owner is known (so there was
///    nothing to filter by in step 2).
///
/// `remotes` are remote URLs in preference order (origin first), as produced
/// by [`git::github_remote_urls`]. `inherited` is the ambient `GH_HOST`.
pub(crate) fn resolve_gh_host(
    config: &LabConfig,
    host_registry: &HostRegistry,
    owner: Option<&str>,
    repo: Option<&str>,
    remotes: &[String],
    inherited: Option<&str>,
) -> Option<ResolvedHost> {
    if let Some(owner) = owner {
        if let Some(host) = config::configured_host_for_repo(config, owner, repo) {
            return Some(ResolvedHost {
                host,
                source: HostSource::Config,
            });
        }
    }

    if let Some(owner) = owner {
        for url in remotes {
            let Ok((_host, remote_owner, _repo)) = git::parse_github_remote(url, host_registry)
            else {
                continue;
            };
            if !remote_owner.eq_ignore_ascii_case(owner) {
                continue;
            }
            if let Some(host) = git::remote_gh_host(url, host_registry) {
                return Some(ResolvedHost {
                    host,
                    source: HostSource::Remote,
                });
            }
        }
    }

    if let Some(inherited) = inherited {
        let trimmed = inherited.trim();
        if !trimmed.is_empty() {
            return Some(ResolvedHost {
                host: trimmed.to_string(),
                source: HostSource::Inherited,
            });
        }
    }

    if owner.is_none() {
        for url in remotes {
            if let Some(host) = git::remote_gh_host(url, host_registry) {
                return Some(ResolvedHost {
                    host,
                    source: HostSource::Remote,
                });
            }
        }
    }

    None
}

/// Resolves the host for an interactive session, reading the current
/// repository's remotes and the ambient `GH_HOST`.
///
/// Returns `None` when no host could be identified; callers should leave
/// `GH_HOST` unset in that case so `gh` applies its own configuration rather
/// than being pinned to a guess. A warning is logged only once every fallback
/// is exhausted — an unrecognized remote is unremarkable when config or an
/// inherited value already answered the question.
pub(crate) async fn resolve_interactive_gh_host(
    owner: Option<&str>,
    repo: Option<&str>,
) -> Option<String> {
    let config = config::try_load_config().unwrap_or_default();
    let host_registry = HostRegistry::from_config(&config);

    let all_remotes = git::list_remotes().await.unwrap_or_default();
    let remotes = git::github_remote_urls(&all_remotes, &host_registry);
    let inherited = std::env::var("GH_HOST").ok();

    let resolved = resolve_gh_host(
        &config,
        &host_registry,
        owner,
        repo,
        &remotes,
        inherited.as_deref(),
    );

    match resolved {
        Some(resolved) => {
            log::debug!(
                "Resolved GH_HOST={} from {:?}",
                resolved.host,
                resolved.source
            );
            Some(resolved.host)
        }
        None => {
            if !all_remotes.is_empty() {
                log::warn!(
                    "Could not determine a GitHub host for this session: no \
                     [daemon].repos entry, no recognized git remote, and no \
                     GH_HOST in the environment. `gh` will use its own default. \
                     Add a [github_hosts.*] section to ~/.gru/config.toml if \
                     this repo lives on GitHub Enterprise."
                );
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GhHostConfig;

    fn ghe_config() -> LabConfig {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "ghe".to_string(),
            GhHostConfig {
                host: "ghe.example.com".to_string(),
                web_url: None,
            },
        );
        config
    }

    #[test]
    fn config_owner_beats_unrelated_remote_and_inherited_host() {
        let mut config = ghe_config();
        // An explicit github.com owner.
        config.daemon.repos = vec!["acme/widgets".to_string()];
        let registry = HostRegistry::from_config(&config);

        let resolved = resolve_gh_host(
            &config,
            &registry,
            Some("acme"),
            Some("widgets"),
            &["https://ghe.example.com/other/thing.git".to_string()],
            Some("ghe.example.com"),
        )
        .unwrap();

        assert_eq!(resolved.host, "github.com");
        assert_eq!(resolved.source, HostSource::Config);
    }

    #[test]
    fn config_named_host_entry_resolves_to_its_host() {
        let mut config = ghe_config();
        config.daemon.repos = vec!["ghe:corp/service".to_string()];
        let registry = HostRegistry::from_config(&config);

        let resolved =
            resolve_gh_host(&config, &registry, Some("corp"), Some("service"), &[], None).unwrap();
        assert_eq!(resolved.host, "ghe.example.com");
        assert_eq!(resolved.source, HostSource::Config);
    }

    #[test]
    fn exact_repo_match_wins_over_owner_only_match() {
        let mut config = ghe_config();
        config.daemon.repos = vec!["ghe:acme/tools".to_string(), "acme/widgets".to_string()];
        let registry = HostRegistry::from_config(&config);

        let widgets =
            resolve_gh_host(&config, &registry, Some("acme"), Some("widgets"), &[], None).unwrap();
        assert_eq!(widgets.host, "github.com");
        let tools =
            resolve_gh_host(&config, &registry, Some("acme"), Some("tools"), &[], None).unwrap();
        assert_eq!(tools.host, "ghe.example.com");
    }

    #[test]
    fn owner_matching_remote_is_used_when_config_is_silent() {
        let config = ghe_config();
        let registry = HostRegistry::from_config(&config);

        let resolved = resolve_gh_host(
            &config,
            &registry,
            Some("acme"),
            Some("widgets"),
            &["https://ghe.example.com/acme/widgets.git".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(resolved.host, "ghe.example.com");
        assert_eq!(resolved.source, HostSource::Remote);
    }

    #[test]
    fn remote_port_reaches_gh_host() {
        let config = ghe_config();
        let registry = HostRegistry::from_config(&config);

        let resolved = resolve_gh_host(
            &config,
            &registry,
            Some("acme"),
            Some("widgets"),
            &["https://ghe.example.com:8443/acme/widgets.git".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(resolved.host, "ghe.example.com:8443");
    }

    #[test]
    fn unrelated_remote_does_not_outrank_inherited_host() {
        let config = ghe_config();
        let registry = HostRegistry::from_config(&config);

        let resolved = resolve_gh_host(
            &config,
            &registry,
            Some("acme"),
            Some("widgets"),
            &["https://github.com/someone/else.git".to_string()],
            Some("ghe.example.com"),
        )
        .unwrap();
        assert_eq!(resolved.host, "ghe.example.com");
        assert_eq!(resolved.source, HostSource::Inherited);
    }

    #[test]
    fn remote_used_without_owner_filter_when_owner_unknown() {
        let config = ghe_config();
        let registry = HostRegistry::from_config(&config);

        let resolved = resolve_gh_host(
            &config,
            &registry,
            None,
            None,
            &["https://ghe.example.com/any/thing.git".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(resolved.host, "ghe.example.com");
        assert_eq!(resolved.source, HostSource::Remote);
    }

    #[test]
    fn host_matching_is_case_insensitive() {
        let config = ghe_config();
        let registry = HostRegistry::from_config(&config);

        let resolved = resolve_gh_host(
            &config,
            &registry,
            Some("AcMe"),
            Some("Widgets"),
            &["https://GHE.Example.COM/acme/widgets.git".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(resolved.host, "ghe.example.com");
    }

    #[test]
    fn blank_inherited_host_is_ignored() {
        let config = ghe_config();
        let registry = HostRegistry::from_config(&config);

        assert!(resolve_gh_host(
            &config,
            &registry,
            Some("acme"),
            Some("widgets"),
            &[],
            Some("   "),
        )
        .is_none());
    }

    #[test]
    fn nothing_resolves_to_none() {
        let config = ghe_config();
        let registry = HostRegistry::from_config(&config);

        assert!(
            resolve_gh_host(&config, &registry, Some("acme"), Some("widgets"), &[], None).is_none()
        );
    }
}
