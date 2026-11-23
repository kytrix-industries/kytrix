use std::{fs, path::Path};

pub(crate) fn test_temp(name: &str) -> tempfile::TempDir {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("this crate lives two directories below the workspace root");
    let root = workspace.join("target").join("test-temp").join("config");
    fs::create_dir_all(&root).expect("the test temporary directory should be created");
    tempfile::Builder::new()
        .prefix(name)
        .tempdir_in(&root)
        .expect("a temporary directory should be created")
}
