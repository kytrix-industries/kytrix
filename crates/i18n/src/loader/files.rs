use std::{fs, path::Path};

use ahash::AHashMap;

use super::Node;
use crate::{
    error::types::Error, language::codes::LanguageCode, store::MessageStore,
    template::interpolate::Template,
};

/// Loads every `.yaml` and `.yml` file in `directory`, using each file's stem as
/// its language code.
///
/// # Errors
///
/// Fails when the directory cannot be listed, when a file is not valid YAML or
/// not valid UTF-8, when a file name is not a
/// [Google Workspace language code](LanguageCode), when a message is not a valid
/// template, and when a message key is defined twice.
pub(crate) fn load_directory(directory: &Path) -> Result<Vec<(LanguageCode, MessageStore)>, Error> {
    let entries = fs::read_dir(directory).map_err(|source| Error::ReadLocaleDirectory {
        path: directory.to_path_buf(),
        source: Box::new(source),
    })?;

    let mut paths = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|source| Error::ReadLocaleDirectory {
                path: directory.to_path_buf(),
                source: Box::new(source),
            })?
            .path();

        if is_locale_file(&path) {
            paths.push(path);
        }
    }
    // A stable order keeps error reporting deterministic when several files are
    // broken at once.
    paths.sort_unstable();

    let mut locales = Vec::with_capacity(paths.len());
    for path in paths {
        locales.push(load_file(&path)?);
    }

    Ok(locales)
}

/// Loads one locale file.
///
/// # Errors
///
/// See [`load_directory`].
pub(crate) fn load_file(path: &Path) -> Result<(LanguageCode, MessageStore), Error> {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();

    let code = LanguageCode::parse(&stem).ok_or_else(|| Error::UnsupportedLanguageCode {
        path: path.to_path_buf(),
        code: stem,
    })?;

    let text = fs::read_to_string(path).map_err(|source| Error::ReadLocaleFile {
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;

    Ok((code, load_str(&text, path)?))
}

/// Parses one locale document into a message store.
///
/// # Errors
///
/// See [`load_directory`]. `path` only points at the offending file in error
/// messages.
pub(crate) fn load_str(text: &str, path: &Path) -> Result<MessageStore, Error> {
    let document: AHashMap<Box<str>, Node> =
        serde_saphyr::from_str(text).map_err(|source| Error::ParseLocaleFile {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;

    let mut store = MessageStore::new(document.len());

    for (key, node) in document {
        flatten(node, &key, path, &mut store)?;
    }

    if store.len() == 0 {
        return Err(Error::EmptyLocaleFile {
            path: path.to_path_buf(),
        });
    }

    Ok(store)
}

/// Adds `node` and everything below it to `store`, prefixing keys with `prefix`.
fn flatten(node: Node, prefix: &str, path: &Path, store: &mut MessageStore) -> Result<(), Error> {
    match node {
        Node::Text(text) => {
            let template = Template::parse(&text).map_err(|source| Error::InvalidTemplate {
                path: path.to_path_buf(),
                key: prefix.to_owned(),
                source,
            })?;

            if !store.insert(prefix.into(), template) {
                return Err(Error::DuplicateKey {
                    path: path.to_path_buf(),
                    key: prefix.to_owned(),
                });
            }
        }
        Node::Section(section) => {
            // `a.b` in the file is the same message as `b` under section `a`, so
            // there is no separator to add for a top-level leaf.
            for (key, child) in section {
                let child_prefix: Box<str> = if prefix.is_empty() {
                    key
                } else {
                    format!("{prefix}.{key}").into()
                };

                flatten(child, &child_prefix, path, store)?;
            }
        }
        Node::Unsupported => {
            return Err(Error::NotAMessage {
                path: path.to_path_buf(),
                key: prefix.to_owned(),
            });
        }
    }

    Ok(())
}

/// Returns `true` for files the loader accepts.
fn is_locale_file(path: &Path) -> bool {
    let is_yaml = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "yaml" | "yml"));

    is_yaml && path.is_file()
}
