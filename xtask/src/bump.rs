use std::{
    fs, io,
    path::{Path, PathBuf},
};

use semver::Version;
use thiserror::Error;

/// The semver component a bump applies to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BumpKind {
    Major,
    Minor,
    Patch,
}

impl BumpKind {
    /// Parses the CLI spelling of a bump kind.
    pub fn from_arg(value: &str) -> Option<Self> {
        match value {
            "major" => Some(Self::Major),
            "minor" => Some(Self::Minor),
            "patch" => Some(Self::Patch),
            _ => None,
        }
    }

    /// Applies the bump to `version`, zeroing the lower components.
    fn apply(self, version: &Version) -> Result<Version, BumpError> {
        match self {
            Self::Major => Ok(Version::new(next_component(version.major, version)?, 0, 0)),
            Self::Minor => Ok(Version::new(
                version.major,
                next_component(version.minor, version)?,
                0,
            )),
            Self::Patch => Ok(Version::new(
                version.major,
                version.minor,
                next_component(version.patch, version)?,
            )),
        }
    }
}

/// The result of a successful bump.
#[derive(Debug)]
pub struct BumpOutcome {
    pub previous: Version,
    pub next: Version,
    pub notes_created: bool,
}

/// The languages every release publishes notes in.
///
/// `en.md` is the GitHub release body and both files are what `latest.json`
/// publishes, so a version is only releasable once it has both.
const NOTE_LANGUAGES: [&str; 2] = ["en", "zh-CN"];

/// Bumps the workspace and packager versions, then creates the release notes.
///
/// The version is read from `[workspace.package]` in the root `Cargo.toml` and
/// checked against the top-level version in `Packager.toml`; a mismatch is an
/// error because guessing which one is authoritative would silently desynchronise
/// the release pipeline. The notes are created before either manifest is written,
/// so a note failure cannot leave the versions bumped without release notes.
pub fn run(root: &Path, kind: BumpKind) -> Result<BumpOutcome, BumpError> {
    let cargo_path = root.join("Cargo.toml");
    let packager_path = root.join("Packager.toml");

    let cargo = read_manifest(&cargo_path)?;
    let packager = read_manifest(&packager_path)?;

    let VersionLocation {
        line_index: cargo_line,
        version: cargo_version,
    } = locate_workspace_version(&cargo)?;
    let VersionLocation {
        line_index: packager_line,
        version: packager_version,
    } = locate_packager_version(&packager)?;

    if cargo_version != packager_version {
        return Err(BumpError::VersionMismatch {
            cargo: cargo_version.to_string(),
            packager: packager_version.to_string(),
        });
    }
    ensure_plain_version(&cargo_version, &cargo_path)?;

    let next = kind.apply(&cargo_version)?;
    let notes_created = create_release_notes(root, &next)?;

    let cargo_updated = replace_version(&cargo, cargo_line, &next)?;
    let packager_updated = replace_version(&packager, packager_line, &next)?;

    write_manifest(&cargo_path, &cargo_updated)?;
    write_manifest(&packager_path, &packager_updated)?;

    Ok(BumpOutcome {
        previous: cargo_version,
        next,
        notes_created,
    })
}

/// Finds the checkout root from any directory inside it.
///
/// The tool is normally run through `cargo xtask`, which leaves the working
/// directory at the caller's directory rather than at the workspace root, so
/// walking up is what lets it work from a crate subdirectory.
pub fn find_repository_root(start: &Path) -> Result<PathBuf, BumpError> {
    for ancestor in start.ancestors() {
        let cargo_path = ancestor.join("Cargo.toml");
        if !cargo_path.is_file() || !ancestor.join("Packager.toml").is_file() {
            continue;
        }

        let cargo = read_manifest(&cargo_path)?;
        if cargo.lines().any(|line| line.trim() == "[workspace]") {
            return Ok(ancestor.to_path_buf());
        }
    }

    Err(BumpError::RepositoryRootNotFound(start.to_path_buf()))
}

fn next_component(component: u64, version: &Version) -> Result<u64, BumpError> {
    component
        .checked_add(1)
        .ok_or_else(|| BumpError::VersionOverflow {
            version: version.to_string(),
        })
}

fn read_manifest(path: &Path) -> Result<String, BumpError> {
    fs::read_to_string(path).map_err(|source| BumpError::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn write_manifest(path: &Path, contents: &str) -> Result<(), BumpError> {
    fs::write(path, contents).map_err(|source| BumpError::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// Creates `release_notes/vX.Y.Z/{en,zh-CN}.md`, or checks that both already exist.
///
/// An existing version directory is not completed: a release with only one of its
/// two languages is refused, because the missing half has to be written by hand
/// rather than generated as an empty heading.
fn create_release_notes(root: &Path, version: &Version) -> Result<bool, BumpError> {
    let notes_dir = root.join("release_notes");
    let directory = notes_dir.join(format!("v{version}"));

    fs::create_dir_all(&notes_dir).map_err(|source| BumpError::Write {
        path: notes_dir,
        source,
    })?;

    match fs::create_dir(&directory) {
        Ok(()) => {
            for language in NOTE_LANGUAGES {
                let path = directory.join(format!("{language}.md"));
                fs::write(&path, format!("# Kytrix v{version}\n")).map_err(|source| {
                    BumpError::Write {
                        path: path.clone(),
                        source,
                    }
                })?;
            }
            Ok(true)
        }
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            require_release_notes(&directory)?;
            Ok(false)
        }
        Err(source) => Err(BumpError::Write {
            path: directory,
            source,
        }),
    }
}

/// Checks that `directory` holds a note for every language a release publishes.
fn require_release_notes(directory: &Path) -> Result<(), BumpError> {
    if !directory.is_dir() {
        return Err(BumpError::NoteNotADirectory(directory.to_path_buf()));
    }

    for language in NOTE_LANGUAGES {
        let path = directory.join(format!("{language}.md"));
        if path.is_file() {
            continue;
        }
        if path.exists() {
            return Err(BumpError::NoteNotAFile(path));
        }
        return Err(BumpError::MissingNote { path });
    }

    Ok(())
}

#[derive(Debug)]
struct VersionLocation {
    line_index: usize,
    version: Version,
}

fn locate_workspace_version(contents: &str) -> Result<VersionLocation, BumpError> {
    locate_section_version(contents, "workspace.package")
}

fn locate_section_version(contents: &str, section: &str) -> Result<VersionLocation, BumpError> {
    let mut found_section = false;
    let mut found: Option<VersionLocation> = None;

    for (line_index, line) in contents.split_inclusive('\n').enumerate() {
        if is_section_header(line) {
            if found_section {
                break;
            }
            if section_name(line) == Some(section) {
                found_section = true;
            }
            continue;
        }

        if !found_section {
            continue;
        }

        let Some(version) = version_from_line(line)? else {
            continue;
        };

        if found.is_some() {
            return Err(BumpError::DuplicateVersion {
                section: section.to_owned(),
            });
        }
        found = Some(VersionLocation {
            line_index,
            version,
        });
    }

    if !found_section {
        return Err(BumpError::MissingSection {
            section: section.to_owned(),
        });
    }

    found.ok_or_else(|| BumpError::MissingVersion {
        section: section.to_owned(),
    })
}

fn locate_packager_version(contents: &str) -> Result<VersionLocation, BumpError> {
    let mut found: Option<VersionLocation> = None;

    for (line_index, line) in contents.split_inclusive('\n').enumerate() {
        if is_section_header(line) {
            break;
        }

        let Some(version) = version_from_line(line)? else {
            continue;
        };

        if found.is_some() {
            return Err(BumpError::DuplicatePackagerVersion);
        }
        found = Some(VersionLocation {
            line_index,
            version,
        });
    }

    found.ok_or(BumpError::MissingPackagerVersion)
}

fn is_section_header(line: &str) -> bool {
    line.trim_start().starts_with('[')
}

fn section_name(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix('[')?;
    let end = rest.find(']')?;
    Some(rest[..end].trim())
}

fn version_from_line(line: &str) -> Result<Option<Version>, BumpError> {
    let code = line.trim_start();
    if code.starts_with('#') {
        return Ok(None);
    }

    let Some((key, _)) = code.split_once('=') else {
        return Ok(None);
    };
    if key.trim() != "version" {
        return Ok(None);
    }

    let raw = quoted_value(line).ok_or_else(|| malformed_version_line(line))?;
    let version = Version::parse(raw).map_err(|source| BumpError::InvalidVersion {
        value: raw.to_owned(),
        source,
    })?;
    Ok(Some(version))
}

fn replace_version(
    contents: &str,
    line_index: usize,
    version: &Version,
) -> Result<String, BumpError> {
    let mut updated = String::with_capacity(contents.len() + 8);

    for (index, line) in contents.split_inclusive('\n').enumerate() {
        if index == line_index {
            updated.push_str(&replace_quoted_value(line, version)?);
        } else {
            updated.push_str(line);
        }
    }

    Ok(updated)
}

fn replace_quoted_value(line: &str, version: &Version) -> Result<String, BumpError> {
    let value = version.to_string();
    let equals = line.find('=').ok_or_else(|| malformed_version_line(line))?;
    let after_equals = &line[equals + 1..];
    let quote_offset = after_equals
        .find(['"', '\''])
        .ok_or_else(|| malformed_version_line(line))?;
    let quote = after_equals[quote_offset..]
        .chars()
        .next()
        .ok_or_else(|| malformed_version_line(line))?;
    let value_start = equals + 1 + quote_offset + quote.len_utf8();
    let value_end = line[value_start..]
        .find(quote)
        .ok_or_else(|| malformed_version_line(line))?
        + value_start;

    let mut updated = String::with_capacity(line.len() + value.len());
    updated.push_str(&line[..value_start]);
    updated.push_str(&value);
    updated.push_str(&line[value_end..]);
    Ok(updated)
}

fn quoted_value(line: &str) -> Option<&str> {
    let equals = line.find('=')?;
    let after_equals = &line[equals + 1..];
    let quote_offset = after_equals.find(['"', '\''])?;
    let quote = after_equals[quote_offset..].chars().next()?;
    let value_start = equals + 1 + quote_offset + quote.len_utf8();
    let value_end = line[value_start..].find(quote)?;
    Some(&line[value_start..value_start + value_end])
}

fn malformed_version_line(line: &str) -> BumpError {
    BumpError::MalformedVersionLine {
        line: line.trim().to_owned(),
    }
}

fn ensure_plain_version(version: &Version, path: &Path) -> Result<(), BumpError> {
    if version.pre.as_str().is_empty() && version.build.as_str().is_empty() {
        Ok(())
    } else {
        Err(BumpError::UnsupportedVersion {
            path: path.to_path_buf(),
            version: version.to_string(),
        })
    }
}

/// Failure to read, validate or write the files a bump touches.
#[derive(Debug, Error)]
pub enum BumpError {
    #[error("failed to read {path:?}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write {path:?}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("no ancestor of {0:?} contains a [workspace] Cargo.toml and a Packager.toml")]
    RepositoryRootNotFound(PathBuf),
    #[error("Cargo.toml has no [{section}] section")]
    MissingSection { section: String },
    #[error("[{section}] has no version key")]
    MissingVersion { section: String },
    #[error("Packager.toml has no top-level version key")]
    MissingPackagerVersion,
    #[error("[{section}] defines more than one version key")]
    DuplicateVersion { section: String },
    #[error("Packager.toml defines more than one top-level version key")]
    DuplicatePackagerVersion,
    #[error("version line is malformed: {line}")]
    MalformedVersionLine { line: String },
    #[error("`{value}` is not a valid semantic version: {source}")]
    InvalidVersion {
        value: String,
        #[source]
        source: semver::Error,
    },
    #[error("{path:?} uses version {version}, which is not a plain MAJOR.MINOR.PATCH version")]
    UnsupportedVersion { path: PathBuf, version: String },
    #[error("Cargo.toml version {cargo} does not match Packager.toml version {packager}")]
    VersionMismatch { cargo: String, packager: String },
    #[error("version {version} cannot be incremented because a component is at its maximum")]
    VersionOverflow { version: String },
    #[error("{0:?} exists but is not a directory")]
    NoteNotADirectory(PathBuf),
    #[error("{0:?} exists but is not a file")]
    NoteNotAFile(PathBuf),
    #[error("{path:?} is missing; every release needs en.md and zh-CN.md")]
    MissingNote { path: PathBuf },
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::TempDir;

    use super::*;

    const CARGO_MANIFEST: &str = concat!(
        "[workspace]\n",
        "resolver = \"3\"\n",
        "members = []\n",
        "\n",
        "[workspace.package]\n",
        "version = \"0.1.0\"\n",
        "edition = \"2024\"\n",
        "\n",
        "[workspace.dependencies]\n",
        "semver = \"1\"\n",
    );

    const PACKAGER_MANIFEST: &str = concat!(
        "# Kytrix packaging config.\n",
        "name = \"kytrix\"\n",
        "version = \"0.1.0\"\n",
        "\n",
        "[nsis]\n",
        "installMode = \"both\"\n",
    );

    fn fixture() -> TempDir {
        let dir = tempfile::tempdir().expect("create fixture");
        fs::create_dir(dir.path().join("release_notes")).expect("create release notes directory");
        fs::write(dir.path().join("Cargo.toml"), CARGO_MANIFEST).expect("write Cargo.toml");
        fs::write(dir.path().join("Packager.toml"), PACKAGER_MANIFEST)
            .expect("write Packager.toml");
        dir
    }

    fn with_version(manifest: &str, version: &str) -> String {
        manifest.replacen(
            "version = \"0.1.0\"",
            &format!("version = \"{version}\""),
            1,
        )
    }

    fn read(path: impl AsRef<Path>) -> String {
        fs::read_to_string(path).expect("read file")
    }

    #[test]
    fn major_bump_resets_minor_and_patch() {
        let version = Version::parse("1.2.3").expect("parse version");
        assert_eq!(
            BumpKind::Major.apply(&version).expect("bump version"),
            Version::new(2, 0, 0)
        );
    }

    #[test]
    fn minor_bump_resets_patch() {
        let version = Version::parse("1.2.3").expect("parse version");
        assert_eq!(
            BumpKind::Minor.apply(&version).expect("bump version"),
            Version::new(1, 3, 0)
        );
    }

    #[test]
    fn patch_bump_increments_patch() {
        let version = Version::parse("1.2.3").expect("parse version");
        assert_eq!(
            BumpKind::Patch.apply(&version).expect("bump version"),
            Version::new(1, 2, 4)
        );
    }

    #[test]
    fn bump_kinds_are_parsed_from_their_cli_names() {
        assert_eq!(BumpKind::from_arg("major"), Some(BumpKind::Major));
        assert_eq!(BumpKind::from_arg("minor"), Some(BumpKind::Minor));
        assert_eq!(BumpKind::from_arg("patch"), Some(BumpKind::Patch));
        assert_eq!(BumpKind::from_arg("Minor"), None);
    }

    #[test]
    fn a_component_overflow_is_reported() {
        let version = Version::new(u64::MAX, 0, 0);
        assert!(matches!(
            BumpKind::Major.apply(&version),
            Err(BumpError::VersionOverflow { .. })
        ));
    }

    #[test]
    fn the_workspace_version_is_read_from_its_section() {
        let location = locate_workspace_version(CARGO_MANIFEST).expect("locate version");
        assert_eq!(location.version, Version::new(0, 1, 0));
    }

    #[test]
    fn dependency_versions_are_not_the_workspace_version() {
        let manifest = concat!(
            "[workspace]\n",
            "[workspace.package]\n",
            "version = \"1.2.3\"\n",
            "\n",
            "[workspace.dependencies]\n",
            "version = \"9.9.9\"\n",
        );
        let location = locate_workspace_version(manifest).expect("locate version");
        assert_eq!(location.version, Version::new(1, 2, 3));
    }

    #[test]
    fn the_packager_version_is_read_before_the_first_table() {
        let manifest = concat!(
            "# comment\n",
            "name = \"kytrix\"\n",
            "version = \"1.2.3\"\n",
            "\n",
            "[nsis]\n",
            "version = \"9.9.9\"\n",
        );
        let location = locate_packager_version(manifest).expect("locate version");
        assert_eq!(location.version, Version::new(1, 2, 3));
    }

    #[test]
    fn replacing_the_version_preserves_comments_spacing_and_line_endings() {
        let manifest = concat!(
            "[workspace.package]\r\n",
            "# release version\r\n",
            "version = '0.1.0'  # bumped by xtask\r\n",
            "\r\n",
            "[other]\r\n",
        );
        let location = locate_workspace_version(manifest).expect("locate version");
        let updated = replace_version(manifest, location.line_index, &Version::new(0, 2, 0))
            .expect("replace version");

        assert_eq!(updated, manifest.replace("0.1.0", "0.2.0"));
        assert!(updated.contains("version = '0.2.0'  # bumped by xtask\r\n"));
    }

    #[test]
    fn a_missing_section_is_reported() {
        assert!(matches!(
            locate_workspace_version("[workspace]\n"),
            Err(BumpError::MissingSection { .. })
        ));
    }

    #[test]
    fn a_missing_version_is_reported() {
        assert!(matches!(
            locate_workspace_version("[workspace.package]\nedition = \"2024\"\n"),
            Err(BumpError::MissingVersion { .. })
        ));
    }

    #[test]
    fn a_missing_packager_version_is_reported() {
        assert!(matches!(
            locate_packager_version("[nsis]\ninstallMode = \"both\"\n"),
            Err(BumpError::MissingPackagerVersion)
        ));
    }

    #[test]
    fn duplicate_versions_are_reported() {
        let manifest = concat!(
            "[workspace.package]\n",
            "version = \"0.1.0\"\n",
            "version = \"0.2.0\"\n",
        );
        assert!(matches!(
            locate_workspace_version(manifest),
            Err(BumpError::DuplicateVersion { .. })
        ));
    }

    #[test]
    fn duplicate_packager_versions_are_reported() {
        let manifest = concat!("version = \"0.1.0\"\n", "version = \"0.2.0\"\n",);
        assert!(matches!(
            locate_packager_version(manifest),
            Err(BumpError::DuplicatePackagerVersion)
        ));
    }

    #[test]
    fn a_malformed_version_line_is_reported() {
        assert!(matches!(
            locate_workspace_version("[workspace.package]\nversion = 1\n"),
            Err(BumpError::MalformedVersionLine { .. })
        ));
    }

    #[test]
    fn an_invalid_semver_is_reported() {
        assert!(matches!(
            locate_workspace_version("[workspace.package]\nversion = \"1.2\"\n"),
            Err(BumpError::InvalidVersion { .. })
        ));
    }

    #[test]
    fn a_patch_bump_updates_both_manifests_and_creates_the_notes() {
        let dir = fixture();
        let outcome = run(dir.path(), BumpKind::Patch).expect("bump version");

        assert_eq!(outcome.previous, Version::new(0, 1, 0));
        assert_eq!(outcome.next, Version::new(0, 1, 1));
        assert!(outcome.notes_created);
        assert_eq!(
            read(dir.path().join("Cargo.toml")),
            with_version(CARGO_MANIFEST, "0.1.1")
        );
        assert_eq!(
            read(dir.path().join("Packager.toml")),
            with_version(PACKAGER_MANIFEST, "0.1.1")
        );
        assert_eq!(
            read(dir.path().join("release_notes/v0.1.1/en.md")),
            "# Kytrix v0.1.1\n"
        );
        assert_eq!(
            read(dir.path().join("release_notes/v0.1.1/zh-CN.md")),
            "# Kytrix v0.1.1\n"
        );
    }

    #[test]
    fn major_and_minor_bumps_zero_the_lower_components() {
        let major_dir = fixture();
        let major = run(major_dir.path(), BumpKind::Major).expect("major bump");
        assert_eq!(major.next, Version::new(1, 0, 0));
        assert!(
            major_dir
                .path()
                .join("release_notes/v1.0.0/en.md")
                .is_file()
        );
        assert!(
            major_dir
                .path()
                .join("release_notes/v1.0.0/zh-CN.md")
                .is_file()
        );
        assert!(read(major_dir.path().join("Cargo.toml")).contains("version = \"1.0.0\""));

        let minor_dir = fixture();
        let minor = run(minor_dir.path(), BumpKind::Minor).expect("minor bump");
        assert_eq!(minor.next, Version::new(0, 2, 0));
        assert!(
            minor_dir
                .path()
                .join("release_notes/v0.2.0/zh-CN.md")
                .is_file()
        );
        assert!(read(minor_dir.path().join("Packager.toml")).contains("version = \"0.2.0\""));
    }

    #[test]
    fn existing_release_notes_are_left_unchanged() {
        let dir = fixture();
        let directory = dir.path().join("release_notes/v0.1.1");
        fs::create_dir(&directory).expect("create version directory");
        let english = directory.join("en.md");
        let chinese = directory.join("zh-CN.md");
        fs::write(&english, "# Hand-written English notes\n").expect("write English note");
        fs::write(&chinese, "# 手写中文说明\n").expect("write Chinese note");

        let outcome = run(dir.path(), BumpKind::Patch).expect("bump version");

        assert!(!outcome.notes_created);
        assert_eq!(read(english), "# Hand-written English notes\n");
        assert_eq!(read(chinese), "# 手写中文说明\n");
        assert!(read(dir.path().join("Cargo.toml")).contains("version = \"0.1.1\""));
    }

    #[test]
    fn a_missing_release_notes_directory_is_created() {
        let dir = fixture();
        fs::remove_dir(dir.path().join("release_notes")).expect("remove release notes directory");

        let outcome = run(dir.path(), BumpKind::Patch).expect("bump version");

        assert!(outcome.notes_created);
        assert!(dir.path().join("release_notes/v0.1.1/en.md").is_file());
        assert!(dir.path().join("release_notes/v0.1.1/zh-CN.md").is_file());
    }

    #[test]
    fn mismatched_manifests_are_rejected_without_changes() {
        let dir = fixture();
        fs::write(
            dir.path().join("Packager.toml"),
            with_version(PACKAGER_MANIFEST, "0.2.0"),
        )
        .expect("write Packager.toml");

        let error = run(dir.path(), BumpKind::Patch).expect_err("version mismatch");

        assert!(matches!(error, BumpError::VersionMismatch { .. }));
        assert_eq!(read(dir.path().join("Cargo.toml")), CARGO_MANIFEST);
        assert_eq!(
            read(dir.path().join("Packager.toml")),
            with_version(PACKAGER_MANIFEST, "0.2.0")
        );
        assert!(!dir.path().join("release_notes/v0.1.1").exists());
    }

    #[test]
    fn a_prerelease_version_is_rejected_before_any_write() {
        let dir = fixture();
        fs::write(
            dir.path().join("Cargo.toml"),
            with_version(CARGO_MANIFEST, "0.1.0-beta.1"),
        )
        .expect("write Cargo.toml");
        fs::write(
            dir.path().join("Packager.toml"),
            with_version(PACKAGER_MANIFEST, "0.1.0-beta.1"),
        )
        .expect("write Packager.toml");

        let error = run(dir.path(), BumpKind::Patch).expect_err("prerelease version");

        assert!(matches!(error, BumpError::UnsupportedVersion { .. }));
        assert!(!dir.path().join("release_notes/v0.1.1").exists());
    }

    #[test]
    fn an_existing_version_path_that_is_a_file_is_rejected() {
        let dir = fixture();
        fs::write(dir.path().join("release_notes/v0.1.1"), "not a directory")
            .expect("write version file");

        let error = run(dir.path(), BumpKind::Patch).expect_err("version path is a file");

        assert!(matches!(error, BumpError::NoteNotADirectory(_)));
        assert_eq!(read(dir.path().join("Cargo.toml")), CARGO_MANIFEST);
    }

    #[test]
    fn a_version_directory_missing_a_language_is_rejected() {
        let dir = fixture();
        let directory = dir.path().join("release_notes/v0.1.1");
        fs::create_dir(&directory).expect("create version directory");
        fs::write(directory.join("en.md"), "# Hand-written English notes\n")
            .expect("write English note");

        let error = run(dir.path(), BumpKind::Patch).expect_err("the Chinese note is missing");

        assert!(matches!(error, BumpError::MissingNote { .. }));
        assert_eq!(read(dir.path().join("Cargo.toml")), CARGO_MANIFEST);
    }

    #[test]
    fn a_language_path_that_is_a_directory_is_rejected() {
        let dir = fixture();
        let directory = dir.path().join("release_notes/v0.1.1");
        fs::create_dir_all(directory.join("zh-CN.md")).expect("create language directory");
        fs::write(directory.join("en.md"), "# Hand-written English notes\n")
            .expect("write English note");

        let error = run(dir.path(), BumpKind::Patch).expect_err("the Chinese path is a directory");

        assert!(matches!(error, BumpError::NoteNotAFile(_)));
        assert_eq!(read(dir.path().join("Cargo.toml")), CARGO_MANIFEST);
    }

    #[test]
    fn the_repository_root_is_found_from_a_nested_directory() {
        let dir = fixture();
        let nested = dir.path().join("crates/nested/src");
        fs::create_dir_all(&nested).expect("create nested directory");
        fs::write(
            nested.parent().expect("crate directory").join("Cargo.toml"),
            "[package]\nname = \"nested\"\nversion = \"0.1.0\"\n",
        )
        .expect("write nested Cargo.toml");

        let root = find_repository_root(&nested).expect("find repository root");
        assert_eq!(root.as_path(), dir.path());
    }

    #[test]
    fn the_repository_root_is_found_in_a_crlf_manifest() {
        let dir = tempfile::tempdir().expect("create temp directory");
        fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\r\nmembers = []\r\n",
        )
        .expect("write Cargo.toml");
        fs::write(dir.path().join("Packager.toml"), "version = \"0.1.0\"\r\n")
            .expect("write Packager.toml");

        let root = find_repository_root(dir.path()).expect("find repository root");
        assert_eq!(root.as_path(), dir.path());
    }

    #[test]
    fn a_directory_without_the_manifests_has_no_repository_root() {
        let dir = tempfile::tempdir().expect("create temp directory");
        assert!(matches!(
            find_repository_root(dir.path()),
            Err(BumpError::RepositoryRootNotFound(_))
        ));
    }
}
