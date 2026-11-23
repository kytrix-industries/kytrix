use std::fmt::Display;

use smallvec::SmallVec;

use crate::template::interpolate::Template;

const INLINE_VALUES: usize = 4;

pub(crate) type Arguments<'a> = SmallVec<[(&'a str, &'a dyn Display); INLINE_VALUES]>;

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
