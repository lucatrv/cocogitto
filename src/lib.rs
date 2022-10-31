use ::log::{error, info, warn};
use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{exit, Command, Stdio};

use anyhow::{anyhow, bail, ensure, Context, Error, Result};
use colored::*;
use conventional_commit_parser::commit::{CommitType, ConventionalCommit};
use conventional_commit_parser::parse_footers;
use git2::{Oid, RebaseOptions};
use globset::Glob;
use itertools::Itertools;
use lazy_static::lazy_static;
use semver::{Prerelease, Version};
use tempfile::TempDir;

use crate::log::filter::CommitFilters;
use conventional::commit::{verify, Commit, CommitConfig};
use conventional::version::VersionIncrement;
use error::{CogCheckReport, PreHookError};
use git::repository::Repository;
use hook::Hook;
use settings::{HookType, Settings};

use crate::conventional::changelog::release::Release;
use crate::conventional::changelog::template::Template;
use crate::git::error::{Git2Error, TagError};
use crate::git::hook::Hooks;
use crate::git::oid::OidOf;
use crate::git::revspec::RevspecPattern;
use crate::git::tag::Tag;
use crate::hook::HookVersion;
use crate::settings::MonoRepoPackage;

pub mod conventional;
pub mod error;
pub mod git;
pub mod hook;
pub mod log;
pub mod settings;

pub type CommitsMetadata = HashMap<CommitType, CommitConfig>;

pub const CONFIG_PATH: &str = "cog.toml";

lazy_static! {
    pub static ref SETTINGS: Settings = {
        if let Ok(repo) = Repository::open(".") {
            Settings::get(&repo).unwrap_or_default()
        } else {
            Settings::default()
        }
    };

    // This cannot be carried by `Cocogitto` struct since we need it to be available in `Changelog`,
    // `Commit` etc. Be ensure that `CocoGitto::new` is called before using this  in order to bypass
    // unwrapping in case of error.
    pub static ref COMMITS_METADATA: CommitsMetadata = {
        SETTINGS.commit_types()
    };
}

pub fn init<S: AsRef<Path> + ?Sized>(path: &S) -> Result<()> {
    let path = path.as_ref();

    if !path.exists() {
        std::fs::create_dir(path)
            .map_err(|err| anyhow!("failed to create directory `{:?}` \n\ncause: {}", path, err))?;
    }

    let mut is_init_commit = false;
    let repository = match Repository::open(&path) {
        Ok(repo) => {
            info!(
                "Found git repository in {:?}, skipping initialisation",
                &path
            );
            repo
        }
        Err(_) => match Repository::init(&path) {
            Ok(repo) => {
                info!("Empty git repository initialized in {:?}", &path);
                is_init_commit = true;
                repo
            }
            Err(err) => panic!("Unable to init repository on {:?}: {}", &path, err),
        },
    };

    let settings = Settings::default();
    let settings_path = path.join(CONFIG_PATH);
    if settings_path.exists() {
        eprint!("Found {} in {:?}, Nothing to do", CONFIG_PATH, &path);
        exit(1);
    } else {
        std::fs::write(
            &settings_path,
            toml::to_string(&settings)
                .map_err(|err| anyhow!("failed to serialize {}\n\ncause: {}", CONFIG_PATH, err))?,
        )
        .map_err(|err| {
            anyhow!(
                "failed to write file `{:?}`\n\ncause: {}",
                settings_path,
                err
            )
        })?;
    }

    repository.add_all()?;

    if is_init_commit {
        let sign = repository.gpg_sign();
        repository.commit("chore: initial commit", sign)?;
    }

    Ok(())
}

#[derive(Debug)]
pub struct CocoGitto {
    repository: Repository,
}

impl CocoGitto {
    pub fn get() -> Result<Self> {
        let repository = Repository::open(&std::env::current_dir()?)?;
        let _settings = Settings::get(&repository)?;
        let _changelog_path = settings::changelog_path();

        Ok(CocoGitto { repository })
    }

    pub fn get_committer(&self) -> Result<String, Git2Error> {
        self.repository.get_author()
    }

    pub fn get_repo_tag_name(&self) -> Option<String> {
        let repo_path = self.repository.get_repo_dir()?.iter().last()?;
        let mut repo_tag_name = repo_path.to_str()?.to_string();

        if let Some(branch_shorthand) = self.repository.get_branch_shorthand() {
            write!(&mut repo_tag_name, " on {}", branch_shorthand).unwrap();
        }

        if let Ok(latest_tag) = self.repository.get_latest_tag() {
            write!(&mut repo_tag_name, " {}", latest_tag).unwrap();
        };

        Some(repo_tag_name)
    }

    pub fn check_and_edit(&self, from_latest_tag: bool) -> Result<()> {
        let commits = if from_latest_tag {
            self.repository
                .get_commit_range(&RevspecPattern::default())?
        } else {
            self.repository.all_commits()?
        };

        let editor = std::env::var("EDITOR")
            .map_err(|_err| anyhow!("the 'EDITOR' environment variable was not found"))?;

        let dir = TempDir::new()?;

        let errored_commits: Vec<Oid> = commits
            .commits
            .iter()
            .map(|commit| {
                let conv_commit = Commit::from_git_commit(commit);
                (commit.id(), conv_commit)
            })
            .filter(|commit| commit.1.is_err())
            .map(|commit| commit.0)
            .collect();

        // Get the last commit oid on the list as a starting point for our rebase
        let last_errored_commit = errored_commits.last();
        if let Some(last_errored_commit) = last_errored_commit {
            let commit = self
                .repository
                .0
                .find_commit(last_errored_commit.to_owned())?;

            let rebase_start = if commit.parent_count() == 0 {
                commit.id()
            } else {
                commit.parent_id(0)?
            };

            let commit = self.repository.0.find_annotated_commit(rebase_start)?;
            let mut options = RebaseOptions::new();

            let mut rebase =
                self.repository
                    .0
                    .rebase(None, Some(&commit), None, Some(&mut options))?;

            while let Some(op) = rebase.next() {
                if let Ok(rebase_operation) = op {
                    let oid = rebase_operation.id();
                    let original_commit = self.repository.0.find_commit(oid)?;
                    if errored_commits.contains(&oid) {
                        warn!("Found errored commits:{}", &oid.to_string()[0..7]);
                        let file_path = dir.path().join(&commit.id().to_string());
                        let mut file = File::create(&file_path)?;

                        let hint = format!(
                            "# Editing commit {}\
                        \n# Replace this message with a conventional commit compliant one\
                        \n# Save and exit to edit the next errored commit\n",
                            original_commit.id()
                        );

                        let mut message_bytes: Vec<u8> = hint.clone().into();
                        message_bytes.extend_from_slice(original_commit.message_bytes());
                        file.write_all(&message_bytes)?;

                        Command::new(&editor)
                            .arg(&file_path)
                            .stdout(Stdio::inherit())
                            .stdin(Stdio::inherit())
                            .stderr(Stdio::inherit())
                            .output()?;

                        let new_message: String = std::fs::read_to_string(&file_path)?
                            .lines()
                            .filter(|line| !line.starts_with('#'))
                            .filter(|line| !line.trim().is_empty())
                            .collect();

                        rebase.commit(None, &original_commit.committer(), Some(&new_message))?;
                        let ignore_merge_commit = SETTINGS.ignore_merge_commits;
                        match verify(
                            self.repository.get_author().ok(),
                            &new_message,
                            ignore_merge_commit,
                        ) {
                            Ok(_) => {
                                info!("Changed commit message to:\"{}\"", &new_message.trim_end())
                            }
                            Err(err) => error!(
                                "Error: {}\n\t{}",
                                "Edited message is still not compliant".red(),
                                err
                            ),
                        }
                    } else {
                        rebase.commit(None, &original_commit.committer(), None)?;
                    }
                } else {
                    error!("{:?}", op);
                }
            }

            rebase.finish(None)?;
        } else {
            info!("{}", "No errored commit, skipping rebase".green());
        }

        Ok(())
    }

    pub fn check(&self, check_from_latest_tag: bool, ignore_merge_commits: bool) -> Result<()> {
        let commit_range = if check_from_latest_tag {
            self.repository
                .get_commit_range(&RevspecPattern::default())?
        } else {
            self.repository.all_commits()?
        };

        let errors: Vec<_> = if ignore_merge_commits {
            commit_range
                .commits
                .iter()
                .filter(|commit| !commit.message().unwrap_or("").starts_with("Merge "))
                .map(Commit::from_git_commit)
                .filter_map(Result::err)
                .collect()
        } else {
            commit_range
                .commits
                .iter()
                .map(Commit::from_git_commit)
                .filter_map(Result::err)
                .collect()
        };

        if errors.is_empty() {
            let msg = "No errored commits".green();
            info!("{}", msg);
            Ok(())
        } else {
            let report = CogCheckReport {
                from: commit_range.from,
                errors: errors.into_iter().map(|err| *err).collect(),
            };
            Err(anyhow!("{}", report))
        }
    }

    pub fn get_log(&self, filters: CommitFilters) -> Result<String> {
        let commits = self.repository.all_commits()?;
        let logs = commits
            .commits
            .iter()
            // Remove merge commits
            .filter(|commit| !commit.message().unwrap_or("").starts_with("Merge"))
            .filter(|commit| filters.filter_git2_commit(commit))
            .map(Commit::from_git_commit)
            // Apply filters
            .filter(|commit| match commit {
                Ok(commit) => filters.filters(commit),
                Err(_) => filters.no_error(),
            })
            // Format
            .map(|commit| match commit {
                Ok(commit) => commit.get_log(),
                Err(err) => err.to_string(),
            })
            .collect::<Vec<String>>()
            .join("\n");

        Ok(logs)
    }

    /// Tries to get a commit message conforming to the Conventional Commit spec.
    /// If the commit message does _not_ conform, `None` is returned instead.
    pub fn get_conventional_message(
        commit_type: &str,
        scope: Option<String>,
        summary: String,
        body: Option<String>,
        footer: Option<String>,
        is_breaking_change: bool,
    ) -> Result<String> {
        // Ensure commit type is known
        let commit_type = CommitType::from(commit_type);

        // Ensure footers are correctly formatted
        let footers = match footer {
            Some(footers) => parse_footers(&footers)?,
            None => Vec::with_capacity(0),
        };

        let conventional_message = ConventionalCommit {
            commit_type,
            scope,
            body,
            footers,
            summary,
            is_breaking_change,
        }
        .to_string();

        // Validate the message
        conventional_commit_parser::parse(&conventional_message)?;

        Ok(conventional_message)
    }

    #[allow(clippy::too_many_arguments)] // FIXME
    pub fn conventional_commit(
        &self,
        commit_type: &str,
        scope: Option<String>,
        summary: String,
        body: Option<String>,
        footer: Option<String>,
        is_breaking_change: bool,
        sign: bool,
    ) -> Result<()> {
        // Ensure commit type is known
        let commit_type = CommitType::from(commit_type);

        // Ensure footers are correctly formatted
        let footers = match footer {
            Some(footers) => parse_footers(&footers)?,
            None => Vec::with_capacity(0),
        };

        let conventional_message = ConventionalCommit {
            commit_type,
            scope,
            body,
            footers,
            summary,
            is_breaking_change,
        }
        .to_string();

        // Validate the message
        conventional_commit_parser::parse(&conventional_message)?;

        // Git commit
        let sign = sign || self.repository.gpg_sign();
        let oid = self.repository.commit(&conventional_message, sign)?;

        // Pretty print a conventional commit summary
        let commit = self.repository.0.find_commit(oid)?;
        let commit = Commit::from_git_commit(&commit)?;
        info!("{}", commit);

        Ok(())
    }

    pub fn create_version(
        &mut self,
        increment: VersionIncrement,
        pre_release: Option<&str>,
        hooks_config: Option<&str>,
        dry_run: bool,
    ) -> Result<()> {
        self.pre_bump_checks()?;

        let current_tag = self.repository.get_latest_tag();
        let current_tag = match current_tag {
            Ok(ref tag) => tag,
            Err(ref err) if err == &TagError::NoTag => {
                warn!("Failed to get current version, falling back to 0.0.0");
                Tag::default()
            }
            Err(ref err) => bail!("{}", err),
        };

        let mut next_version = current_tag.bump(increment, &self.repository)?;

        if next_version.version.le(&current_tag.version) || next_version.eq(&current_version) {
            let comparison = format!("{} <= {}", current_version, next_version).red();
            let cause_key = "cause:".red();
            let cause = format!(
                "{} version MUST be greater than current one: {}",
                cause_key, comparison
            );

            bail!("{}:\n\t{}\n", "SemVer Error".red().to_string(), cause);
        };

        if let Some(pre_release) = pre_release {
            next_version.pre = Prerelease::new(pre_release)?;
        }

        let tag = Tag::create(next_version, None);

        if dry_run {
            print!("{}", tag);
            return Ok(());
        }

        let origin = if current_version == Version::new(0, 0, 0) {
            self.repository.get_first_commit()?.to_string()
        } else {
            current_tag?.oid_unchecked().to_string()
        };

        let target = self.repository.get_head_commit_oid()?.to_string();
        let pattern = (origin.as_str(), target.as_str());

        let pattern = RevspecPattern::from(pattern);
        let changelog = self.get_changelog_with_target_version(pattern, tag.clone())?;

        let path = settings::changelog_path();
        let template = SETTINGS.get_changelog_template()?;
        changelog.write_to_file(path, template)?;

        let current = self.repository.get_latest_tag().map(HookVersion::new).ok();

        let next_version = HookVersion::new(tag.clone());

        let hook_result = self.run_hooks(
            HookType::PreBump,
            current.as_ref(),
            &next_version,
            hooks_config,
            None,
        );

        self.repository.add_all()?;

        // Hook failed, we need to stop here and reset
        // the repository to a clean state
        if let Err(err) = hook_result {
            self.stash_failed_version(&tag, err)?;
        }

        self.repository.commit(
            &format!("chore(version): {}", next_version.prefixed_tag),
            false,
        )?;

        self.repository.create_tag(&tag)?;

        self.run_hooks(
            HookType::PostBump,
            current.as_ref(),
            &next_version,
            hooks_config,
            None,
        )?;

        let current = current
            .map(|current| current.prefixed_tag.to_string())
            .unwrap_or_else(|| "...".to_string());
        let bump = format!("{} -> {}", current, next_version.prefixed_tag).green();
        info!("Bumped version: {}", bump);

        Ok(())
    }

    pub fn create_monorepo_version(
        &mut self,
        pre_release: Option<&str>,
        hooks_config: Option<&str>,
        dry_run: bool,
    ) -> Result<()> {
        self.pre_bump_checks()?;
        let mut package_bumps = vec![];

        for (package_name, package) in &SETTINGS.packages {
            let current_tag = self.repository.get_latest_package_tag(package_name);
            let current_tag = match current_tag {
                Ok(ref tag) => tag.version.clone(),
                Err(ref err) if err == &TagError::NoTag => {
                    warn!("Failed to get current version, falling back to 0.0.0");
                    Tag::default()
                }
                Err(ref err) => bail!("{}", err),
            };

            let mut next_version =
                VersionIncrement::Auto.bump(&current_version, &self.repository)?;

            if next_version.le(&current_version) || next_version.eq(&current_version) {
                let comparison = format!("{} <= {}", current_version, next_version).red();
                let cause_key = "cause:".red();
                let cause = format!(
                    "{} version MUST be greater than current one: {}",
                    cause_key, comparison
                );

                bail!("{}:\n\t{}\n", "SemVer Error".red().to_string(), cause);
            };

            if let Some(pre_release) = pre_release {
                next_version.pre = Prerelease::new(pre_release)?;
            }

            let tag = Tag::create(next_version, Some(package_name.to_string()));

            if dry_run {
                print!("{}", tag);
                continue;
            }

            let origin = if current_version == Version::new(0, 0, 0) {
                self.repository.get_first_commit()?.to_string()
            } else {
                current_tag?.oid_unchecked().to_string()
            };

            let target = self.repository.get_head_commit_oid()?.to_string();
            let pattern = (origin.as_str(), target.as_str());

            let pattern = RevspecPattern::from(pattern);
            let changelog =
                self.get_changelog_with_target_package_version(pattern, tag.clone(), package)?;

            if changelog.is_none() {
                println!("No commit found to bump package {package_name}, skipping.");
                continue;
            }

            let changelog = changelog.unwrap();
            let path = package.changelog_path();
            let template = SETTINGS.get_changelog_template()?;
            changelog.write_to_file(path, template)?;

            let current = self
                .repository
                .get_latest_package_tag(package_name)
                .map(HookVersion::new)
                .ok();

            let next_version = HookVersion::new(tag.clone());

            let hook_result = self.run_hooks(
                HookType::PreBump,
                current.as_ref(),
                &next_version,
                hooks_config,
                Some(package),
            );

            self.repository.add_all()?;

            // Hook failed, we need to stop here and reset
            // the repository to a clean state
            if let Err(err) = hook_result {
                self.stash_failed_version(&tag, err)?;
            }

            package_bumps.push((package_name, package, current, next_version, tag));
        }

        // Todo: meta version
        self.repository.commit("chore(version): Bump", false)?;

        for (package_name, package, current, next_version, tag) in package_bumps {
            self.repository.create_tag(&tag)?;

            self.run_hooks(
                HookType::PostBump,
                current.as_ref(),
                &next_version,
                hooks_config,
                Some(package),
            )?;

            let current = current
                .map(|current| current.prefixed_tag.to_string())
                .unwrap_or_else(|| "...".to_string());
            let bump = format!("{} -> {}", current, next_version.prefixed_tag).green();
            info!("Bumped package {package_name} version: {}", bump);
        }

        Ok(())
    }

    pub fn create_package_version(
        &mut self,
        (package_name, package): (&str, &MonoRepoPackage),
        increment: VersionIncrement,
        pre_release: Option<&str>,
        hooks_config: Option<&str>,
        dry_run: bool,
    ) -> Result<()> {
        self.pre_bump_checks()?;

        let current_tag = self.repository.get_latest_package_tag(package_name);
        let current_tag = match current_tag {
            Ok(ref tag) => tag,
            Err(ref err) if err == &TagError::NoTag => {
                warn!("Failed to get current version, falling back to 0.0.0");
                Tag::default()
            }
            Err(ref err) => bail!("{}", err),
        };

        let mut next_version = current_tag.bump(increment, &self.repository)?;

        if next_version.le(&current_version) || next_version.eq(&current_version) {
            let comparison = format!("{} <= {}", current_version, &next_version).red();
            let cause_key = "cause:".red();
            let cause = format!(
                "{} version MUST be greater than current one: {}",
                cause_key, comparison
            );

            bail!("{}:\n\t{}\n", "SemVer Error".red().to_string(), cause);
        };

        if let Some(pre_release) = pre_release {
            next_version.pre = Prerelease::new(pre_release)?;
        }

        let tag = Tag::create(next_version.clone(), Some(package_name.to_string()));

        if dry_run {
            print!("{}", tag);
            return Ok(());
        }

        let origin = if current_version == Version::new(0, 0, 0) {
            self.repository.get_first_commit()?.to_string()
        } else {
            current_tag?.oid_unchecked().to_string()
        };

        let target = self.repository.get_head_commit_oid()?.to_string();
        let pattern = (origin.as_str(), target.as_str());

        let pattern = RevspecPattern::from(pattern);
        let changelog =
            self.get_changelog_with_target_package_version(pattern, tag.clone(), package)?;

        if changelog.is_none() {
            bail!("No commit matching package {package_name} path");
        }

        let changelog = changelog.unwrap();
        let path = package.changelog_path();
        let template = SETTINGS.get_changelog_template()?;
        changelog.write_to_file(path, template)?;

        let current = self
            .repository
            .get_latest_package_tag(package_name)
            .map(HookVersion::new)
            .ok();

        let next_version =
            HookVersion::new(Tag::create(next_version, Some(package_name.to_string())));

        let hook_result = self.run_hooks(
            HookType::PreBump,
            current.as_ref(),
            &next_version,
            hooks_config,
            Some(package),
        );

        self.repository.add_all()?;

        // Hook failed, we need to stop here and reset
        // the repository to a clean state
        if let Err(err) = hook_result {
            self.stash_failed_version(&tag, err)?;
        }

        self.repository
            .commit(&format!("chore(version): {}", tag), false)?;

        self.repository.create_tag(&tag)?;

        self.run_hooks(
            HookType::PostBump,
            current.as_ref(),
            &next_version,
            hooks_config,
            Some(package),
        )?;

        let current = current
            .map(|current| current.prefixed_tag.to_string())
            .unwrap_or_else(|| "...".to_string());
        let bump = format!("{} -> {}", current, next_version.prefixed_tag).green();
        info!("Bumped package {package_name} version: {}", bump);

        Ok(())
    }

    fn stash_failed_version(&mut self, tag: &Tag, err: Error) -> Result<()> {
        self.repository.stash_failed_version(tag.clone())?;
        error!(
            "{}",
            PreHookError {
                cause: err.to_string(),
                version: tag.to_string(),
                stash_number: 0,
            }
        );

        exit(1);
    }

    fn pre_bump_checks(&mut self) -> Result<()> {
        if *SETTINGS == Settings::default() {
            let part1 = "Warning: using".yellow();
            let part2 = "with the default configuration. \n".yellow();
            let part3 = "You may want to create a".yellow();
            let part4 = "file in your project root to configure bumps.\n".yellow();
            warn!(
                "{} 'cog bump' {}{} 'cog.toml' {}",
                part1, part2, part3, part4
            );
        }
        let statuses = self.repository.get_statuses()?;

        // Fail if repo contains un-staged or un-committed changes
        ensure!(statuses.0.is_empty(), "{}", self.repository.get_statuses()?);

        if !SETTINGS.branch_whitelist.is_empty() {
            if let Some(branch) = self.repository.get_branch_shorthand() {
                let whitelist = &SETTINGS.branch_whitelist;
                let is_match = whitelist.iter().any(|pattern| {
                    let glob = Glob::new(pattern)
                        .expect("invalid glob pattern")
                        .compile_matcher();
                    glob.is_match(&branch)
                });

                ensure!(
                    is_match,
                    "No patterns matched in {:?} for branch '{}', bump is not allowed",
                    whitelist,
                    branch
                )
            }
        };

        Ok(())
    }

    pub fn get_changelog_at_tag(&self, tag: &str, template: Template) -> Result<String> {
        let pattern = format!("..{}", tag);
        let pattern = RevspecPattern::from(pattern.as_str());
        let changelog = self.get_changelog(pattern, false)?;

        changelog
            .into_markdown(template)
            .map_err(|err| anyhow!(err))
    }

    /// Used for cog bump. the target version
    /// is not created yet when generating the changelog.
    pub fn get_changelog_with_target_version(
        &self,
        pattern: RevspecPattern,
        tag: Tag,
    ) -> Result<Release> {
        let commit_range = self.repository.get_commit_range(&pattern)?;

        let mut release = Release::from(commit_range);
        release.version = OidOf::Tag(tag);
        Ok(release)
    }

    /// Used for cog bump. the target version
    /// is not created yet when generating the changelog.
    pub fn get_changelog_with_target_package_version(
        &self,
        pattern: RevspecPattern,
        target_tag: Tag,
        package: &MonoRepoPackage,
    ) -> Result<Option<Release>> {
        let mut release = self
            .repository
            .get_commit_range_for_packages(package, &pattern)?
            .map(Release::from);

        if let Some(release) = &mut release {
            release.version = OidOf::Tag(target_tag);
        }

        Ok(release)
    }

    /// ## Get a changelog between two oids
    /// - `from` default value:latest tag or else first commit
    /// - `to` default value:`HEAD` or else first commit
    pub fn get_changelog(
        &self,
        pattern: RevspecPattern,
        with_child_releases: bool,
    ) -> Result<Release> {
        if with_child_releases {
            self.repository
                .get_release_range(pattern)
                .map_err(Into::into)
        } else {
            let commit_range = self.repository.get_commit_range(&pattern)?;

            Ok(Release::from(commit_range))
        }
    }

    fn run_hooks(
        &self,
        hook_type: HookType,
        current_tag: Option<&HookVersion>,
        next_version: &HookVersion,
        hook_profile: Option<&str>,
        package: Option<&MonoRepoPackage>,
    ) -> Result<()> {
        let settings = Settings::get(&self.repository)?;

        let hooks: Vec<Hook> = match (package, hook_profile) {
            (None, Some(profile)) => settings
                .get_profile_hooks(profile, hook_type)
                .iter()
                .map(|s| s.parse())
                .enumerate()
                .map(|(idx, result)| {
                    result.context(format!(
                        "Cannot parse bump profile {} hook at index {}",
                        profile, idx
                    ))
                })
                .try_collect()?,

            (Some(package), Some(profile)) => {
                let hooks = package.get_profile_hooks(profile, hook_type);

                hooks
                    .iter()
                    .map(|s| s.parse())
                    .enumerate()
                    .map(|(idx, result)| {
                        result.context(format!(
                            "Cannot parse bump profile {} hook at index {}",
                            profile, idx
                        ))
                    })
                    .try_collect()?
            }
            (Some(package), None) => package
                .get_hooks(hook_type)
                .iter()
                .map(|s| s.parse())
                .enumerate()
                .map(|(idx, result)| result.context(format!("Cannot parse hook at index {}", idx)))
                .try_collect()?,
            (None, None) => settings
                .get_hooks(hook_type)
                .iter()
                .map(|s| s.parse())
                .enumerate()
                .map(|(idx, result)| result.context(format!("Cannot parse hook at index {}", idx)))
                .try_collect()?,
        };

        for mut hook in hooks {
            hook.insert_versions(current_tag, next_version)?;
            hook.run().context(hook.to_string())?;
        }

        Ok(())
    }
}
