mod files;
mod node;

pub(crate) use files::{load_directory, load_file, load_str};
use node::Node;

#[cfg(test)]
mod loader_tests;
