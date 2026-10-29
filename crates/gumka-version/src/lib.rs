// SPDX-License-Identifier: GPL-3.0-or-later

//! Gumka's version, from git: the newest `release-X.Y.Z` tag at or before the commit (see
//! docs/packaging.md). `Cargo.toml`'s version isn't the app's. Read by `cargo xtask bundle`,
//! which must get it right, and by the app's build script, which shows it.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Version {
    /// `X.Y.Z` on a release tag, `X.Y.Z+N~gHASH` N commits after one, `0.0.0+…` before any.
    pub full: String,
    /// How many commits lead to this one: a build number that only grows from one build of
    /// `main` to the next (macOS's `CFBundleVersion`, which picks the newest of several copies).
    pub build: u32,
}

impl Version {
    /// `X.Y.Z` alone, for what takes numbers only (macOS's `CFBundleShortVersionString`).
    pub fn release(&self) -> &str {
        self.full
            .split_once('+')
            .map_or(&self.full, |(release, _)| release)
    }
}

/// The version of the checkout at `root`, the repository's top folder. A source archive (no
/// `.git` there) is 0.0.0, and a commit with no release tag at or before it 0.0.0 and its
/// commits. A shallow clone, which has neither the tags nor the history, and a git that fails
/// are errors: no version is guessed.
pub fn read(root: &Path) -> Result<Version, String> {
    // Its own `.git` only: a checkout around a source archive isn't Gumka's.
    if !root.join(".git").exists() {
        return Ok(Version {
            full: "0.0.0".into(),
            build: 0,
        });
    }
    from_git(|args| git(root, args))
}

/// The version from what `git` prints when run with the given arguments.
fn from_git(git: impl Fn(&[&str]) -> Result<String, String>) -> Result<Version, String> {
    if git(&["rev-parse", "--is-shallow-repository"])? == "true" {
        return Err(
            "this clone is shallow, without the tags and history the version comes \
                    from; fetch them with `git fetch --unshallow --tags`"
                .into(),
        );
    }
    let build = git(&["rev-list", "--count", "HEAD"])?
        .parse()
        .map_err(|e| format!("git's count of commits: {e}"))?;
    // Tags such as release-notes aren't versions.
    let full = match git(&["describe", "--tags", "--long", "--match", "release-[0-9]*"]) {
        Ok(described) => version_from_describe(&described)?,
        // No release yet, as if one were tagged before the first commit. Only then: why
        // describe failed is told by the release tags, not by git's (translated) message.
        Err(failed) => {
            if !git(&["tag", "--list", "--merged", "HEAD", "release-[0-9]*"])?.is_empty() {
                return Err(failed);
            }
            format!("0.0.0+{build}~g{}", git(&["rev-parse", "--short", "HEAD"])?)
        }
    };
    Ok(Version { full, build })
}

/// The git files that change when the version can, for a build script to read the version
/// again only then: `HEAD`, the branch it names, the tags (loose, packed or in a reftable), and
/// `shallow`, which goes when the clone gets its history; not, as far as git's files allow,
/// other branches and worktrees. Only files that exist: Cargo runs a build script every time
/// when a path it watches is missing, and once when one goes.
pub fn watched(root: &Path) -> Vec<PathBuf> {
    let dot_git = root.join(".git");
    if !dot_git.exists() {
        return Vec::new();
    }
    let dirs = git(
        root,
        &["rev-parse", "--absolute-git-dir", "--git-common-dir"],
    )
    .unwrap_or_default();
    match dirs.lines().collect::<Vec<_>>()[..] {
        // A worktree has its own HEAD; the branches and tags are the repository's.
        [own, common] => {
            let common = root.join(common);
            // None when detached: HEAD itself changes then.
            let branch = git(root, &["symbolic-ref", "-q", "HEAD"])
                .ok()
                .and_then(|branch| branch_file(&common, &branch));
            git_files(Path::new(own), &common, branch)
        }
        // Git fails (missing, or refusing a checkout another user owns): the usual files and
        // every branch, so that a commit reads the version again, and tries git again.
        _ if dot_git.is_dir() => git_files(&dot_git, &dot_git, Some(dot_git.join("refs/heads"))),
        _ => Vec::new(),
    }
}

/// The files `watched` names, in the git folders of the checkout (`own`, a worktree's) and of
/// the repository, with `branch`, what a commit on the checked-out branch changes.
fn git_files(own: &Path, common: &Path, branch: Option<PathBuf>) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = [
        own.join("HEAD"),
        common.join("refs/tags"),
        common.join("packed-refs"),
        // Every ref of a reftable repository, but a worktree's HEAD, which is in its own.
        common.join("reftable"),
        own.join("reftable"),
        common.join("shallow"),
    ]
    .into_iter()
    .chain(branch)
    .filter(|path| path.exists())
    .collect();
    files.dedup();
    files
}

/// What a commit on `branch` (`refs/heads/…`) changes: its loose file, or, while it is packed,
/// the folder its next commit writes one to (a missing file would have Cargo run the build
/// script every time). Nothing in a reftable repository, whose tables are watched.
fn branch_file(common: &Path, branch: &str) -> Option<PathBuf> {
    if common.join("reftable").is_dir() {
        return None;
    }
    Path::new(branch)
        .ancestors()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| common.join(path))
        .find(|path| path.exists())
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// The version from `git describe --tags --long`'s `release-X.Y.Z-N-gHASH`.
fn version_from_describe(described: &str) -> Result<String, String> {
    let mut fields = described.rsplitn(3, '-');
    let (Some(hash), Some(commits), Some(tag)) = (fields.next(), fields.next(), fields.next())
    else {
        return Err(format!("can't read the version from {described:?}"));
    };
    let release = tag.strip_prefix("release-").unwrap_or(tag);
    let numbers: Vec<&str> = release.split('.').collect();
    if numbers.len() != 3
        || !numbers
            .iter()
            .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(format!(
            "the tag {tag} isn't release-X.Y.Z; delete it or tag the commit properly"
        ));
    }
    Ok(match commits {
        "0" => release.to_owned(),
        _ => format!("{release}+{commits}~{hash}"),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn the_version_comes_from_the_newest_release_tag() {
        let version = |described| version_from_describe(described).unwrap();
        assert_eq!(version("release-0.2.0-0-gabc1234"), "0.2.0", "on the tag");
        assert_eq!(
            version("release-0.2.0-12-gabc1234"),
            "0.2.0+12~gabc1234",
            "12 commits later"
        );
        assert_eq!(version("release-10.20.30-3-g0f"), "10.20.30+3~g0f");
        for tag in [
            "release-0.2-0-gabc1234",
            "release-0.2.0.1-0-gabc1234",
            "release-0.2.0-beta.1-0-gabc1234",
            "release-v0.2.0-0-gabc1234",
            "release-0..2-0-gabc1234",
            "abc1234",
        ] {
            assert!(version_from_describe(tag).is_err(), "{tag}");
        }
    }

    #[test]
    fn the_release_is_the_numbers_alone() {
        let version = |full: &str| Version {
            full: full.into(),
            build: 1,
        };
        assert_eq!(version("0.2.0").release(), "0.2.0");
        assert_eq!(version("0.2.0+12~gabc1234").release(), "0.2.0");
    }

    #[test]
    fn only_a_missing_release_tag_is_0_0_0() {
        // A git whose describe fails, with these release tags at or before the commit.
        let git = |tags: &'static str| {
            move |args: &[&str]| match args {
                ["rev-parse", "--is-shallow-repository"] => Ok("false".to_owned()),
                ["rev-list", "--count", "HEAD"] => Ok("5".to_owned()),
                ["describe", ..] => Err("git describe failed".to_owned()),
                ["tag", "--list", "--merged", "HEAD", _] => Ok(tags.to_owned()),
                ["rev-parse", "--short", "HEAD"] => Ok("abc1234".to_owned()),
                _ => panic!("git {args:?}"),
            }
        };
        assert_eq!(from_git(git("")).unwrap().full, "0.0.0+5~gabc1234");
        assert_eq!(
            from_git(git("release-0.1.0")).err().as_deref(),
            Some("git describe failed"),
            "no version is guessed"
        );
    }

    /// Runs git in `dir` with a fixed identity, no signing and repositories in the files
    /// format, whatever the user's settings.
    fn git_in(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .env_remove("GIT_DEFAULT_REF_FORMAT")
            .args([
                "-c",
                "user.name=Gumka",
                "-c",
                "user.email=gumka@example.invalid",
            ])
            .args(["-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false"])
            .args(["-c", "init.defaultRefFormat=files"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// A new, empty folder for the test `name`.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gumka-version-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A repository in `dir` with one commit.
    fn repository(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git_in(dir, &["init", "-q"]);
        git_in(dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
    }

    fn version(dir: &Path) -> (String, u32) {
        let version = read(dir).unwrap();
        (version.full, version.build)
    }

    /// `watched`, checked to name only files that exist.
    fn watched_in(dir: &Path) -> Vec<PathBuf> {
        let watched = watched(dir);
        assert!(watched.iter().all(|path| path.exists()), "{watched:?}");
        watched
    }

    /// Whether `watched` holds `path`, however each is spelled: the temporary folder has more
    /// than one name on macOS and Windows.
    fn holds(watched: &[PathBuf], path: &Path) -> bool {
        let path = path.canonicalize().ok();
        path.is_some() && watched.iter().any(|w| w.canonicalize().ok() == path)
    }

    #[test]
    fn the_version_follows_the_repository() {
        let dir = scratch("follows");
        assert_eq!(version(&dir), ("0.0.0".into(), 0), "a source archive");
        assert!(watched(&dir).is_empty());

        repository(&dir);
        git_in(&dir, &["tag", "release-notes"]);
        let hash = git_in(&dir, &["rev-parse", "--short", "HEAD"]);
        assert_eq!(
            version(&dir),
            (format!("0.0.0+1~g{hash}"), 1),
            "no release yet: a tag that's no version is ignored"
        );
        git_in(&dir, &["checkout", "-q", "-b", "other"]);
        git_in(&dir, &["commit", "-q", "--allow-empty", "-m", "elsewhere"]);
        git_in(&dir, &["tag", "release-9.0.0"]);
        git_in(&dir, &["checkout", "-q", "-"]);
        assert_eq!(
            version(&dir),
            (format!("0.0.0+1~g{hash}"), 1),
            "a release on another branch isn't this commit's"
        );

        git_in(&dir, &["tag", "release-0.1.0"]);
        assert_eq!(version(&dir), ("0.1.0".into(), 1));
        git_in(&dir, &["commit", "-q", "--allow-empty", "-m", "two"]);
        let hash = git_in(&dir, &["rev-parse", "--short", "HEAD"]);
        assert_eq!(version(&dir), (format!("0.1.0+1~g{hash}"), 2));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_checked_out_branch_and_the_tags_are_watched() {
        let dir = scratch("watched");
        repository(&dir);
        git_in(&dir, &["branch", "other"]);
        let git_dir = dir.join(".git");
        let branch = git_dir.join(git_in(&dir, &["symbolic-ref", "HEAD"]));
        let watched = watched_in(&dir);
        for path in [&git_dir.join("HEAD"), &branch, &git_dir.join("refs/tags")] {
            assert!(holds(&watched, path), "{path:?} in {watched:?}");
        }
        assert!(
            !holds(&watched, &git_dir.join("refs/heads"))
                && !holds(&watched, &git_dir.join("refs/heads/other")),
            "not other branches: {watched:?}"
        );

        git_in(&dir, &["pack-refs", "--all"]);
        assert!(!branch.exists());
        let watched = watched_in(&dir);
        assert!(
            holds(&watched, &git_dir.join("refs/heads")),
            "where its next commit writes the branch: {watched:?}"
        );
        assert!(holds(&watched, &git_dir.join("packed-refs")));

        git_in(&dir, &["checkout", "-q", "--detach"]);
        let watched = watched_in(&dir);
        assert!(holds(&watched, &git_dir.join("HEAD")));
        assert!(
            !holds(&watched, &git_dir.join("refs/heads")),
            "no branch: {watched:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_worktree_has_its_own_head() {
        let dir = scratch("worktree");
        let repo = dir.join("repo");
        repository(&repo);
        git_in(&repo, &["tag", "release-0.1.0"]);
        git_in(&repo, &["worktree", "add", "-q", "-b", "side", "../side"]);
        let side = dir.join("side");
        let git_dir = repo.join(".git");

        let watched = watched_in(&side);
        for path in ["worktrees/side/HEAD", "refs/heads/side", "refs/tags"] {
            assert!(
                holds(&watched, &git_dir.join(path)),
                "{path} in {watched:?}"
            );
        }
        assert!(
            !holds(&watched, &git_dir.join("HEAD")),
            "not the main checkout's HEAD: {watched:?}"
        );

        assert_eq!(version(&side), ("0.1.0".into(), 1));
        git_in(&side, &["commit", "-q", "--allow-empty", "-m", "two"]);
        let hash = git_in(&side, &["rev-parse", "--short", "HEAD"]);
        assert_eq!(version(&side), (format!("0.1.0+1~g{hash}"), 2));
        assert_eq!(version(&repo), ("0.1.0".into(), 1), "the main checkout's");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_shallow_clone_is_read_again_once_it_has_its_history() {
        let dir = scratch("shallow");
        let repo = dir.join("repo");
        repository(&repo);
        git_in(&repo, &["tag", "release-0.1.0"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "two"]);
        // `--no-local`, as a local clone copies everything and ignores `--depth`; a relative path
        // rather than a file URL, which Windows spells differently.
        git_in(
            &dir,
            &["clone", "-q", "--no-local", "--depth", "1", "repo", "clone"],
        );
        let clone = dir.join("clone");
        let refused = read(&clone).err().unwrap_or_default();
        assert!(refused.contains("shallow"), "{refused}");
        let shallow = clone.join(".git/shallow");
        let watched = watched_in(&clone);
        assert!(holds(&watched, &shallow), "{watched:?}");

        git_in(&clone, &["fetch", "-q", "--unshallow", "--tags"]);
        assert!(!shallow.exists(), "gone, which Cargo notices");
        assert_eq!(version(&clone), version(&repo));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reftable_repository_is_watched() {
        let dir = scratch("reftable");
        let repo = dir.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let made = Command::new("git")
            .current_dir(&repo)
            .args(["init", "-q", "--ref-format=reftable"])
            .output()
            .unwrap();
        // A git before 2.45 (Apple's 2.39 among them) can't make one, nor read one.
        if !made.status.success() {
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git_in(&repo, &["tag", "release-0.1.0"]);
        git_in(&repo, &["worktree", "add", "-q", "-b", "side", "../side"]);
        let side = dir.join("side");
        let git_dir = repo.join(".git");

        let watched = watched_in(&repo);
        assert!(holds(&watched, &git_dir.join("reftable")), "{watched:?}");
        assert!(
            !holds(&watched, &git_dir.join("refs/heads")),
            "not the stand-in for older gits: {watched:?}"
        );
        let watched = watched_in(&side);
        for path in ["reftable", "worktrees/side/reftable"] {
            assert!(
                holds(&watched, &git_dir.join(path)),
                "{path} in {watched:?}"
            );
        }
        assert_eq!(version(&repo), ("0.1.0".into(), 1));
        assert_eq!(version(&side), ("0.1.0".into(), 1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_git_watches_the_usual_files() {
        let dir = scratch("failing");
        repository(&dir);
        // An extension this git doesn't know, as when it is older than the repository: every
        // command fails, as when git is missing or refuses a checkout another user owns.
        git_in(&dir, &["config", "core.repositoryformatversion", "1"]);
        git_in(&dir, &["config", "extensions.gumkaTest", "true"]);
        assert!(read(&dir).is_err());
        let git_dir = dir.join(".git");
        let watched = watched_in(&dir);
        for path in ["HEAD", "refs/heads", "refs/tags"] {
            assert!(
                holds(&watched, &git_dir.join(path)),
                "{path} in {watched:?}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
