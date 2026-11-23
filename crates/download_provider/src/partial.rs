use std::path::{Path, PathBuf};

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
