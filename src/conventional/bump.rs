use conventional_commit_parser::commit::CommitType;
use semver::{BuildMetadata, Prerelease, Version};
use crate::conventional::error::BumpError;
use crate::{Commit, Repository, RevspecPattern, Tag, VersionIncrement};
use git2::Commit as Git2Commit;

pub trait Bump {
    fn manual_bump(&self, version: &str) -> Result<Self, semver::Error>  where Self: Sized;
    fn major_bump(&self) -> Self;
    fn minor_bump(&self) -> Self;
    fn patch_bump(&self) -> Self;
    fn auto_bump(&self, repository: &Repository) -> Self;
}

impl Bump for Tag {
    fn manual_bump(&self, version: &str) -> Result<Self, semver::Error> {
        let mut next = self.clone();
        next.version = Version::parse(version)?;
        Ok(next)
    }

    fn major_bump(&self) -> Self {
        let mut next = self.clone();
        next.version.major += 1;
        next.version.minor = 0;
        next.version.patch = 0;
        next.reset_metadata()
    }

    fn minor_bump(&self) -> Self {
        let mut next = self.clone();
        next.version.minor += 1;
        next.version.patch = 0;
        next.reset_metadata()
    }

    fn patch_bump(&self) -> Self {
        let mut next = self.clone();
        next.version.patch += 1;
        next.reset_metadata()
    }

    fn auto_bump(&self, repository: &Repository) -> Self {
        todo!()
    }
}

impl Tag {
    pub fn bump(&self,increment: VersionIncrement, repository: &Repository) -> Result<Self, BumpError> {
        match increment {
            VersionIncrement::Major => Ok(self.major_bump()),
            VersionIncrement::Minor => Ok(self.minor_bump()),
            VersionIncrement::Patch => Ok(self.patch_bump()),
            VersionIncrement::Auto => self.create_version_from_commit_history(
                repository
            ),
            VersionIncrement::Manual(version) => self.manual_bump(&version)
                .map_err(Into::into)
        }
    }

    fn reset_metadata(mut self) -> Self {
        self.version.build = BuildMetadata::EMPTY;
        self.version.pre = Prerelease::EMPTY;
        self.oid = None;
        self
    }

    fn create_version_from_commit_history(
        &self,
        repository: &Repository,
    ) -> Result<Tag, BumpError> {
        let changelog_start_oid = match &self.package {
            None => repository
                .get_latest_tag_oid()?,
            Some(package) => {
                repository
                    .get_latest_package_tag(&package)
                    .ok()
                    .and_then(|tag| tag.oid)
            }.unwrap_or_else(|| repository.get_first_commit().unwrap()),
        };

        let changelog_start_oid = changelog_start_oid.to_string();
        let changelog_start_oid = Some(changelog_start_oid.as_str());

        let pattern = changelog_start_oid
            .map(|oid| format!("{}..", oid))
            .unwrap_or_else(|| "..".to_string());
        let pattern = pattern.as_str();
        let pattern = RevspecPattern::from(pattern);
        let commits = repository.get_commit_range(&pattern)?;

        let commits: Vec<&Git2Commit> = commits
            .commits
            .iter()
            .filter(|commit| !commit.message().unwrap_or("").starts_with("Merge "))
            .collect();

        VersionIncrement::display_history(&commits)?;

        let conventional_commits: Vec<Commit> = commits
            .iter()
            .map(|commit| Commit::from_git_commit(commit))
            .filter_map(Result::ok)
            .collect();

        let increment_type =self.version_increment_from_commit_history(
            &conventional_commits,
        )?;

        Ok(match increment_type {
            VersionIncrement::Major => self.major_bump(),
            VersionIncrement::Minor => self.minor_bump(),
            VersionIncrement::Patch => self.patch_bump(),
            _ => unreachable!()
        })


    }

    fn version_increment_from_commit_history(
        &self,
        commits: &[Commit],
    ) -> Result<VersionIncrement, BumpError> {
        let is_major_bump = || {
            self.version.major != 0
                && commits
                .iter()
                .any(|commit| commit.message.is_breaking_change)
        };

        let is_minor_bump = || {
            commits
                .iter()
                .any(|commit| commit.message.commit_type == CommitType::Feature)
        };

        let is_patch_bump = || {
            commits
                .iter()
                .any(|commit| commit.message.commit_type == CommitType::BugFix)
        };

        if is_major_bump() {
            Ok(VersionIncrement::Major)
        } else if is_minor_bump() {
            Ok(VersionIncrement::Minor)
        } else if is_patch_bump() {
            Ok(VersionIncrement::Patch)
        } else {
            Err(BumpError::NoCommitFound)
        }
    }
}