//! Setup shared by the crate's unit-test modules.

use std::{fs, path::Path};

/// A temporary directory of the test's own, under the workspace's build output.
///
/// The system temporary directory is not usable here: a test that runs behind a restricted
/// integrity token cannot create directories in `%TEMP%`, which leaves every test about a file on
/// disk failing for a reason that has nothing to do with the code under test. The build output is
/// the workspace's own writable scratch space, so the directory goes there.
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
