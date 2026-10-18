use gpui_kit::{base::StyledExt, prelude::FluentBuilder, *};

/// Which icon to draw.
///
/// It is a name rather than a picture: everything that holds one — a menu entry, a
/// button, a panel built again on every frame — keeps the name and resolves it to a
/// path when the icon is drawn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IconKind {
    X,
    XBold,
    Circle,
    CircleX,
    RadioButton,
    Square,
    SquareBold,
    CheckSquare,
    Check,
    CheckCircle,
    CircleNotch,
    CaretDown,
    CaretLeft,
    ArrowUp,
    ArrowDown,
    Eye,
    EyeClosed,
    Minus,
    MinusBold,
    CopySimpleBold,
    Plus,
    NotePencil,
    Trash,
    PuzzlePiece,
    SquareHalf,
    SquaresFour,
    Table,
    ColumnsPlusLeft,
    ColumnsPlusRight,
    RowsPlusTop,
    RowsPlusBottom,
    ArrowsClockwise,
    Repeat,
    MagnifyingGlass,
    At,
    Play,
    GearSix,
    WarningCircle,
    User,
    Faders,
    Graph,
    Plugs,
    PlugsConnected,
    SpeakerSimpleHigh,
    SpeakerSimpleSlash,
}

impl IconKind {
    pub const fn path(&self) -> &'static str {
        match self {
            IconKind::X => "icons/phosphoricons/x.svg",
            IconKind::XBold => "icons/phosphoricons/x-bold.svg",
            IconKind::Circle => "icons/phosphoricons/circle.svg",
            IconKind::CircleX => "icons/phosphoricons/x-circle.svg",
            IconKind::RadioButton => "icons/phosphoricons/radio-button.svg",
            IconKind::Square => "icons/phosphoricons/square.svg",
            IconKind::SquareBold => "icons/phosphoricons/square-bold.svg",
            IconKind::CheckSquare => "icons/phosphoricons/check-square.svg",
            IconKind::Check => "icons/phosphoricons/check.svg",
            IconKind::CheckCircle => "icons/phosphoricons/check-circle.svg",
            IconKind::CircleNotch => "icons/phosphoricons/circle-notch.svg",
            IconKind::CaretDown => "icons/phosphoricons/caret-down.svg",
            IconKind::CaretLeft => "icons/phosphoricons/caret-left.svg",
            IconKind::ArrowUp => "icons/phosphoricons/arrow-up.svg",
            IconKind::ArrowDown => "icons/phosphoricons/arrow-down.svg",
            IconKind::Eye => "icons/phosphoricons/eye.svg",
            IconKind::EyeClosed => "icons/phosphoricons/eye-closed.svg",
            IconKind::Minus => "icons/phosphoricons/minus.svg",
            IconKind::MinusBold => "icons/phosphoricons/minus-bold.svg",
            IconKind::CopySimpleBold => "icons/phosphoricons/copy-simple-bold.svg",
            IconKind::Plus => "icons/phosphoricons/plus.svg",
            IconKind::NotePencil => "icons/phosphoricons/note-pencil.svg",
            IconKind::Trash => "icons/phosphoricons/trash.svg",
            IconKind::PuzzlePiece => "icons/phosphoricons/puzzle-piece.svg",
            IconKind::SquareHalf => "icons/phosphoricons/square-half.svg",
            IconKind::SquaresFour => "icons/phosphoricons/squares-four.svg",
            IconKind::Table => "icons/phosphoricons/table.svg",
            IconKind::ColumnsPlusLeft => "icons/phosphoricons/columns-plus-left.svg",
            IconKind::ColumnsPlusRight => "icons/phosphoricons/columns-plus-right.svg",
            IconKind::RowsPlusTop => "icons/phosphoricons/rows-plus-top.svg",
            IconKind::RowsPlusBottom => "icons/phosphoricons/rows-plus-bottom.svg",
            IconKind::ArrowsClockwise => "icons/phosphoricons/arrows-clockwise.svg",
            IconKind::Repeat => "icons/phosphoricons/repeat.svg",
            IconKind::MagnifyingGlass => "icons/phosphoricons/magnifying-glass.svg",
            IconKind::At => "icons/phosphoricons/at.svg",
            IconKind::Play => "icons/phosphoricons/play.svg",
            IconKind::GearSix => "icons/phosphoricons/gear-six.svg",
            IconKind::WarningCircle => "icons/phosphoricons/warning-circle.svg",
            IconKind::User => "icons/phosphoricons/user.svg",
            IconKind::Faders => "icons/phosphoricons/faders.svg",
            IconKind::Graph => "icons/phosphoricons/graph.svg",
            IconKind::Plugs => "icons/phosphoricons/plugs.svg",
            IconKind::PlugsConnected => "icons/phosphoricons/plugs-connected.svg",
            IconKind::SpeakerSimpleHigh => "icons/phosphoricons/speaker-simple-high.svg",
            IconKind::SpeakerSimpleSlash => "icons/phosphoricons/speaker-simple-slash.svg",
        }
    }
}

#[derive(IntoElement)]
pub struct Icon {
    icon_kind: IconKind,
    style: StyleRefinement,
    transformation: Option<Transformation>,
}

impl Icon {
    pub fn new(icon_kind: IconKind) -> Self {
        Self {
            icon_kind,
            style: StyleRefinement::default(),
            transformation: None,
        }
    }

    pub fn transformation(mut self, transformation: Transformation) -> Self {
        self.transformation = Some(transformation);
        self
    }
}

impl Styled for Icon {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Icon {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let Self {
            icon_kind,
            style,
            transformation,
        } = self;

        svg()
            .path(icon_kind.path())
            .text_color(window.text_style().color)
            .when_some(transformation, |this, transformation| {
                this.with_transformation(transformation)
            })
            .refine_style(&style)
    }
}

#[cfg(test)]
mod tests {
    use super::IconKind;

    /// 全部变体，顺序与 [`IconKind::path`] 一致。
    const KINDS: [IconKind; 45] = [
        IconKind::X,
        IconKind::XBold,
        IconKind::Circle,
        IconKind::CircleX,
        IconKind::RadioButton,
        IconKind::Square,
        IconKind::SquareBold,
        IconKind::CheckSquare,
        IconKind::Check,
        IconKind::CheckCircle,
        IconKind::CircleNotch,
        IconKind::CaretDown,
        IconKind::CaretLeft,
        IconKind::ArrowUp,
        IconKind::ArrowDown,
        IconKind::Eye,
        IconKind::EyeClosed,
        IconKind::Minus,
        IconKind::MinusBold,
        IconKind::CopySimpleBold,
        IconKind::Plus,
        IconKind::NotePencil,
        IconKind::Trash,
        IconKind::PuzzlePiece,
        IconKind::SquareHalf,
        IconKind::SquaresFour,
        IconKind::Table,
        IconKind::ColumnsPlusLeft,
        IconKind::ColumnsPlusRight,
        IconKind::RowsPlusTop,
        IconKind::RowsPlusBottom,
        IconKind::ArrowsClockwise,
        IconKind::Repeat,
        IconKind::MagnifyingGlass,
        IconKind::At,
        IconKind::Play,
        IconKind::GearSix,
        IconKind::WarningCircle,
        IconKind::User,
        IconKind::Faders,
        IconKind::Graph,
        IconKind::Plugs,
        IconKind::PlugsConnected,
        IconKind::SpeakerSimpleHigh,
        IconKind::SpeakerSimpleSlash,
    ];

    /// 每个变体的 `path()` 都必须指向一个真实存在于 `assets/` 里的文件，否则 `svg()` 画不出东西。
    #[test]
    fn every_variant_should_point_at_an_embedded_asset() {
        for kind in KINDS {
            let path = kind.path();
            assert!(
                crate::assets::Assets::get(path).is_some(),
                "{kind:?} points at `{path}`, which is not in the embedded assets"
            );
        }
    }
}
