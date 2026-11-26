use std::{borrow::Cow, sync::Arc};

use smol_str::SmolStr;
use ui_core::{
    app::App,
    error::EngineError,
    math::{LogicalPx, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{color::Color, style::StylePatch, style_builder::StyleBuilder},
    window::WindowId,
};

use crate::theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role};

mod assets;
mod build;
mod html;
mod inline;
mod lines;
mod plain;
mod table;
#[cfg(test)]
mod tests;

const LINE_HEIGHT: f32 = 1.4;
const DECORATION_THICKNESS: f32 = 1.0;
const BACKGROUND_PADDING_X: f32 = 2.0;
const BACKGROUND_RADIUS: f32 = 2.0;

pub trait MarkdownAssets {
    fn load(&self, path: &str) -> Option<Cow<'static, [u8]>>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownStyle {
    pub body_size: LogicalPx,
    pub line_height: f32,
    pub foreground: Color,
    pub muted: Color,
    pub link: Color,
    pub code_foreground: Color,
    pub code_background: Color,
    pub quote_border: Color,
    pub divider: Color,
    pub image_placeholder: Color,
    pub mono_family: SmolStr,
    pub task_border: Color,
    pub task_fill: Color,
    pub decoration_thickness: LogicalPx,
    pub background_padding_x: LogicalPx,
    pub background_radius: LogicalPx,
}

impl MarkdownStyle {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        let tokens = theme.tokens();
        Self {
            body_size: tokens.medium.font_size,
            line_height: LINE_HEIGHT,
            foreground: theme.role(Role::FgDefault),
            muted: theme.role(Role::FgMuted),
            link: theme.role(Role::BrandBase),
            code_foreground: theme.role(Role::FgDefault),
            code_background: theme.role(Role::SurfaceSunken),
            quote_border: theme.role(Role::LineBorder),
            divider: theme.role(Role::LineDivider),
            image_placeholder: theme.role(Role::SurfaceControl),
            mono_family: SmolStr::new_static(inline::MONO_FAMILY),
            task_border: theme.role(Role::LineBorder),
            task_fill: theme.role(Role::BrandBase),
            decoration_thickness: px(DECORATION_THICKNESS),
            background_padding_x: px(BACKGROUND_PADDING_X),
            background_radius: px(BACKGROUND_RADIUS),
        }
    }

    pub(super) fn line_height(&self) -> LogicalPx {
        self.body_size * self.line_height
    }
}

pub struct MarkdownNode {
    root: NodeId,
    source: Arc<str>,
    assets: Box<dyn MarkdownAssets>,
    theme: ThemeHandle,
    appearance: Appearance,
}

impl MarkdownNode {
    pub fn mount<A: MarkdownAssets + 'static>(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        theme: ThemeHandle,
        source: &str,
        assets: A,
    ) -> Result<Self, EngineError> {
        let appearance = theme.current(app);
        let dependency_theme = theme.clone();
        let root = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width_full()
                    .min_width_0()
                    .build(),
            )
            .id(build::ROOT_ID)
            .bind_style(move |cx| {
                let _ = dependency_theme.appearance(cx);
                StylePatch::default()
            })
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )?;
        let node = Self {
            root,
            source: Arc::from(source),
            assets: Box::new(assets),
            theme,
            appearance,
        };
        node.build(app, appearance)?;
        Ok(node)
    }

    pub fn sync(&mut self, app: &mut App) -> bool {
        let appearance = self.theme.current(app);
        if appearance == self.appearance {
            return false;
        }
        let Some(window) = self.root.window() else {
            return false;
        };
        let Some(target) = app.window_mut(window) else {
            return false;
        };
        let tree = target.tree_mut();
        let Ok(children) = tree.get(self.root).map(|node| node.children().to_vec()) else {
            return false;
        };
        for child in children {
            if let Ok(removed) = tree.remove(child) {
                debug_assert!(
                    removed.is_empty(),
                    "markdown 子树不得挂 bind_*：重建会留下未释放的依赖边"
                );
            }
        }
        if self.build(app, appearance).is_err() {
            return false;
        }
        self.appearance = appearance;
        true
    }

    #[must_use]
    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn release(&mut self, app: &mut App) -> bool {
        let Some(window) = self.root.window() else {
            return false;
        };
        app.remove_node(window, self.root).is_ok()
    }

    fn build(&self, app: &mut App, appearance: Appearance) -> Result<(), EngineError> {
        let window = self
            .root
            .window()
            .ok_or(EngineError::StaleNode { node: self.root })?;
        let style = MarkdownStyle::from_theme(self.theme.theme_for(appearance));
        build::Builder::new(app, window, &style, self.assets.as_ref(), self.root)
            .build(self.source.as_ref())
    }
}

pub fn plain_text(source: &str) -> SmolStr {
    plain::plain_text(source)
}
