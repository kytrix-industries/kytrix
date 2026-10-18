//! The translation macros.

/// Expands `name = value` to `value` and a bare `name` to the local variable of
/// that name.
#[doc(hidden)]
#[macro_export]
macro_rules! __i18n_value {
    ($name:ident = $value:expr) => {
        $value
    };
    ($name:ident) => {
        $name
    };
}

/// Translates a message of the translator installed in GPUI global state.
///
/// The first argument is any GPUI context, so the macro works inside `render`,
/// inside an entity update, and in application startup code. The message is
/// rendered in the language of the
/// [`LanguageCode`](crate::language::codes::LanguageCode) global, which
/// [`install`](crate::global::embedded::install) selects next to the translator.
///
/// ```no_run
/// # use i18n::{global::embedded, language::codes::LanguageCode, t};
/// # fn render(cx: &mut gpui_kit::App) -> gpui_kit::SharedString {
/// embedded::install(cx).expect("embedded locales should load");
///
/// assert_eq!(t!(cx, "menu.market").as_ref(), "Market");
///
/// let name = "Ada";
/// assert_eq!(t!(cx, "greeting", name = name).as_ref(), "Hello, Ada!");
/// # t!(cx, "menu.market")
/// # }
/// ```
///
/// A placeholder can be bound to an expression or to a local variable of the
/// same name:
///
/// ```text
/// t!(cx, "menu.market")             // a message without placeholders
/// t!(cx, "greeting", name = name)   // a placeholder bound to an expression
/// t!(cx, "greeting", name)          // a placeholder bound to a local variable
/// ```
///
/// A message that is missing from every loaded locale translates to an empty
/// string, so a broken key shows up as a gap in the interface rather than as a
/// crash.
///
/// # Panics
///
/// Panics when no translator or no language was installed, which means the
/// composition root did not call [`install`](crate::global::embedded::install)
/// before the first frame.
#[macro_export]
macro_rules! t {
    ($cx:expr, $key:expr $(,)?) => {
        $cx.global::<$crate::translator::types::I18n>()
            .translate(*$cx.global::<$crate::language::codes::LanguageCode>(), $key)
    };
    ($cx:expr, $key:expr, $($argument:ident $(= $value:expr)?),+ $(,)?) => {
        $cx.global::<$crate::translator::types::I18n>()
            .translate_with(
                *$cx.global::<$crate::language::codes::LanguageCode>(),
                $key,
                &[$(
                    (
                        stringify!($argument),
                        &$crate::__i18n_value!($argument $(= $value)?) as &dyn ::std::fmt::Display,
                    ),
                )+],
            )
    };
}
