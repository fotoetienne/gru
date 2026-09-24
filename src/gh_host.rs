//! Resolves the `GH_HOST` value handed to interactive agent sessions.
//!
//! The autonomous paths (`gru do`, `gru review`) already know their host: it
//! came from the issue/PR URL or from the Minion's own remote. The user-facing
//! REPLs (`gru chat`, `gru pm`, `gru tpm`) don't — they start from a working
//! directory and an optional `--repo owner/repo`. This module turns that into
//! a host so `gh` inside the session targets the same GitHub instance the user
//! is actually working against instead of defaulting to github.com.

use crate::config::{self, HostMatch, HostRegistry, LabConfig};
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
/// 1. **A `daemon.repos` entry naming this exact `owner/repo`.** The user wrote
///    this repo's host down, so it beats anything inferred — including when it
///    says `github.com` and the checkout's remote points somewhere else.
/// 2. **A remote belonging to the same owner.** When the session targets
///    `acme/widgets` and a remote is `https://ghe.example.com/acme/widgets`,
///    that remote identifies the host. Remotes for *other* owners are not
///    consulted at this step — an unrelated checkout must not decide the host
///    for a `--repo` the user named explicitly.
/// 3. **A `daemon.repos` entry naming only this owner.** Weaker than the
///    repo's own remote: an entry for `acme/widgets` says nothing definite
///    about where `acme/tools` lives, and a multi-repo owner can legitimately
///    straddle github.com and a GHES instance.
/// 4. **An inherited `GH_HOST`.** If the user already exported one, honour it
///    rather than overriding it with a guess.
/// 5. **Any recognized remote**, but only when no owner is known (so there was
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
    // Steps 1-3 all require an owner to match against; step 4 does not, and
    // step 5 applies only in its absence.
    if let Some(owner) = owner {
        let configured = config::configured_host_for_repo(config, owner, repo);

        // 1. An entry naming this exact repo.
        if let Some((host, HostMatch::Exact)) = &configured {
            return Some(ResolvedHost {
                host: host.clone(),
                source: HostSource::Config,
            });
        }

        // 2. A remote for this same owner.
        if let Some(resolved) = owner_remote_host(host_registry, owner, remotes) {
            return Some(resolved);
        }

        // 3. An entry naming only this owner.
        if let Some((host, HostMatch::OwnerOnly)) = configured {
            return Some(ResolvedHost {
                host,
                source: HostSource::Config,
            });
        }
    }

    // 4. Whatever the environment already said.
    if let Some(inherited) = inherited {
        let trimmed = inherited.trim();
        if !trimmed.is_empty() {
            return Some(ResolvedHost {
                host: trimmed.to_string(),
                source: HostSource::Inherited,
            });
        }
    }

    // 5. With no owner there was nothing to filter remotes by, so any
    //    recognized one is the best available evidence.
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

/// Finds the first remote belonging to `owner` and returns its host.
///
/// Each URL is parsed twice — once by [`git::parse_github_remote`] for the
/// owner, once by [`git::remote_gh_host`] for the host with its port intact.
/// Deliberate: the two answers have different port semantics, and merging them
/// into one return type would leak that distinction into every other caller.
fn owner_remote_host(
    host_registry: &HostRegistry,
    owner: &str,
    remotes: &[String],
) -> Option<ResolvedHost> {
    for url in remotes {
        let Ok((_host, remote_owner, _repo)) = git::parse_github_remote(url, host_registry) else {
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
            if let Some(message) = unresolved_warning(owner, !all_remotes.is_empty(), &remotes) {
                log::warn!("{}", message);
            }
            None
        }
    }
}

/// Builds the warning for an unresolved host, or `None` when staying quiet.
///
/// A repo with no remotes at all isn't worth a warning — the user is somewhere
/// `gh` was never going to work anyway. The two cases that are worth one read
/// very differently, and saying "no recognized git remote" when there is a
/// perfectly recognized one sends the user hunting for a `[github_hosts.*]`
/// problem they don't have.
fn unresolved_warning(
    owner: Option<&str>,
    has_remotes: bool,
    recognized: &[String],
) -> Option<String> {
    if !has_remotes {
        return None;
    }
    // A recognized remote with no owner to filter by would have resolved in
    // step 5, so reaching here with recognized remotes means we know the owner
    // and none of them belong to it.
    match (owner, recognized.is_empty()) {
        (Some(owner), false) => Some(format!(
            "Could not determine a GitHub host for {owner}: this checkout's \
             remotes are recognized but none belong to {owner}, there is no \
             [daemon].repos entry for it, and no GH_HOST is set. `gh` will use \
             its own default. Add an entry for {owner} to [daemon].repos in \
             ~/.gru/config.toml to route it explicitly."
        )),
        _ => Some(
            "Could not determine a GitHub host for this session: no \
             [daemon].repos entry, no recognized git remote, and no GH_HOST in \
             the environment. `gh` will use its own default. Add a \
             [github_hosts.*] section to ~/.gru/config.toml if this repo lives \
             on GitHub Enterprise."
                .to_string(),
        ),
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
    fn owner_remote_beats_owner_only_config_entry() {
        // The config names a *sibling* repo on github.com; the checkout we are
        // actually in is this owner's GHES repo. The remote is the stronger
        // evidence.
        let mut config = ghe_config();
        config.daemon.repos = vec!["acme/widgets".to_string()];
        let registry = HostRegistry::from_config(&config);

        let resolved = resolve_gh_host(
            &config,
            &registry,
            Some("acme"),
            Some("tools"),
            &["https://ghe.example.com/acme/tools.git".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(resolved.host, "ghe.example.com");
        assert_eq!(resolved.source, HostSource::Remote);
    }

    #[test]
    fn exact_config_entry_beats_owner_remote() {
        // Same shape as above, but now the user named this exact repo.
        let mut config = ghe_config();
        config.daemon.repos = vec!["acme/tools".to_string()];
        let registry = HostRegistry::from_config(&config);

        let resolved = resolve_gh_host(
            &config,
            &registry,
            Some("acme"),
            Some("tools"),
            &["https://ghe.example.com/acme/tools.git".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(resolved.host, "github.com");
        assert_eq!(resolved.source, HostSource::Config);
    }

    #[test]
    fn owner_only_config_entry_beats_unrelated_remote_and_inherited() {
        let mut config = ghe_config();
        config.daemon.repos = vec!["ghe:acme/widgets".to_string()];
        let registry = HostRegistry::from_config(&config);

        let resolved = resolve_gh_host(
            &config,
            &registry,
            Some("acme"),
            Some("tools"),
            &["https://github.com/someone/else.git".to_string()],
            Some("github.com"),
        )
        .unwrap();
        assert_eq!(resolved.host, "ghe.example.com");
        assert_eq!(resolved.source, HostSource::Config);
    }

    #[test]
    fn unresolved_warning_is_silent_without_remotes() {
        assert!(unresolved_warning(Some("acme"), false, &[]).is_none());
    }

    #[test]
    fn unresolved_warning_names_the_owner_when_remotes_are_recognized() {
        let message = unresolved_warning(
            Some("acme"),
            true,
            &["https://github.com/someone/else.git".to_string()],
        )
        .unwrap();
        assert!(message.contains("acme"), "{message}");
        assert!(!message.contains("no recognized git remote"), "{message}");
    }

    #[test]
    fn unresolved_warning_reports_unrecognized_remotes() {
        let message = unresolved_warning(Some("acme"), true, &[]).unwrap();
        assert!(message.contains("no recognized git remote"), "{message}");
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
