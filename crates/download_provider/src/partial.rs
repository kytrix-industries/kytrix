//! Where a download streams before it is renamed into place.
//!
//! The partial file sits beside its destination: the download manager, the
//! transfer and the views that remove an unfinished download all have to agree
//! on that one path, so it is named here once.

use std::path::{Path, PathBuf};

/// The partial file a download to `destination` streams into.
///
/// The name is the destination's own with `.part` appended, in the same
/// directory. A destination with no file name has no partial file beside it.
pub fn path(destination: &Path) -> Option<PathBuf> {
    let mut file_name = destination.file_name()?.to_os_string();
    file_name.push(".part");

    Some(destination.with_file_name(file_name))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::path;

    #[test]
    fn a_partial_file_sits_beside_its_destination() {
        assert_eq!(
            path(Path::new("models").join("model.bin").as_path()),
            Some(PathBuf::from("models").join("model.bin.part")),
        );
    }

    #[test]
    fn a_destination_without_a_file_name_has_no_partial_file() {
        assert_eq!(path(Path::new("")), None);
        assert_eq!(path(Path::new("models").join("..").as_path()), None);
    }
}
