use std::fmt::Display;

use smallvec::SmallVec;

use crate::template::interpolate::Template;

/// Inline capacity for the arguments of one message.
const INLINE_VALUES: usize = 4;

/// The arguments of a message, in the order the template mentions them.
pub(crate) type Arguments<'a> = SmallVec<[(&'a str, &'a dyn Display); INLINE_VALUES]>;

/// Resolves the arguments a template needs from the arguments a caller passed.
///
/// Keeping only the placeholders a template actually mentions makes the cache
/// key independent of unused arguments.
pub(crate) fn bind_arguments<'a>(
    template: &'a Template,
    arguments: &'a [(&'a str, &'a dyn Display)],
) -> Arguments<'a> {
    template
        .placeholder_names()
        .map(|name| {
            let value = arguments
                .iter()
                .find_map(|(candidate, value)| (*candidate == name).then_some(*value))
                .unwrap_or(&"");

            (name, value)
        })
        .collect()
}
