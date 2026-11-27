pub(crate) mod anchor;
pub(crate) mod animation;
pub(crate) mod dismiss;
pub(crate) mod focus;
pub mod placement;
pub(crate) mod settle;
mod stack;
#[cfg(test)]
mod tests;

use std::time::Instant;

use smallvec::SmallVec;
use smol_str::SmolStr;

use crate::{
    a11y::A11yRole,
    app::App,
    error::EngineError,
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize},
    node::id::NodeId,
    window::WindowId,
};

pub const MAX_POPUP_DEPTH: usize = 32;

pub const MAX_POPUP_ENTRIES: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PopupId {
    window: WindowId,
    slot: u32,
    generation: u32,
}

impl PopupId {
    pub(crate) fn new(window: WindowId, slot: u32, generation: u32) -> Self {
        Self {
            window,
            slot,
            generation,
        }
    }

    #[must_use]
    pub fn window(self) -> WindowId {
        self.window
    }

    #[must_use]
    pub fn slot(self) -> u32 {
        self.slot
    }

    #[must_use]
    pub fn generation(self) -> u32 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PopupRect {
    pub hit: LogicalRect,
    pub paint: LogicalRect,
    pub a11y: LogicalRect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupTier {
    Auto,
    Hint,
    Modal,
    Scrim,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupState {
    Opening,
    Open,
    DismissPending,
    Closing,
    Closed,
}

impl PopupState {
    #[must_use]
    pub fn is_dismissable(self) -> bool {
        matches!(self, Self::Opening | Self::Open)
    }

    #[must_use]
    pub fn is_visible(self) -> bool {
        !matches!(self, Self::Closed)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PopupDirty {
    pub geometry: bool,
    pub order: bool,
}

impl PopupDirty {
    #[must_use]
    pub fn any(self) -> bool {
        self.geometry || self.order
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZPolicy {
    Newest,
    Above(PopupId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutsidePress {
    Consume,
    Replay,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscapePolicy {
    CloseTopmost,
    CloseChain,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DismissPolicy {
    Chain {
        outside: OutsidePress,
        escape: EscapePolicy,
        anchor_lost: bool,
        focus_lost: bool,
    },
    Explicit {
        outside: bool,
        escape: bool,
    },
    Hint {
        escape: bool,
    },
}

impl DismissPolicy {
    #[must_use]
    pub fn escape(self) -> EscapePolicy {
        match self {
            Self::Chain { escape, .. } => escape,
            Self::Explicit { escape: true, .. } => EscapePolicy::CloseTopmost,
            Self::Explicit { escape: false, .. } => EscapePolicy::Ignore,
            Self::Hint { escape: true } => EscapePolicy::CloseTopmost,
            Self::Hint { escape: false } => EscapePolicy::Ignore,
        }
    }
}

impl Default for DismissPolicy {
    fn default() -> Self {
        Self::Chain {
            outside: OutsidePress::Replay,
            escape: EscapePolicy::CloseTopmost,
            anchor_lost: true,
            focus_lost: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreFocus {
    Opener,
    Previous,
    RootFallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitialFocus {
    DefaultButton,
    FirstFocusable,
    Container,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusPolicy {
    None,
    Take {
        restore: RestoreFocus,
    },
    Trap {
        restore: RestoreFocus,
        initial: InitialFocus,
    },
}

impl FocusPolicy {
    #[must_use]
    pub fn is_modal(self) -> bool {
        matches!(self, Self::Trap { .. })
    }

    #[must_use]
    pub fn restore(self) -> RestoreFocus {
        match self {
            Self::None => RestoreFocus::RootFallback,
            Self::Take { restore } | Self::Trap { restore, .. } => restore,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupAnimation {
    None,
    Fade,
    FadeSlide,
    FadeScale,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HasPopup {
    #[default]
    Menu,
    ListBox,
    Tree,
    Grid,
    Dialog,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PopupA11y {
    pub role: Option<A11yRole>,
    pub label: Option<SmolStr>,
    pub expanded_of: Option<NodeId>,
    pub owns: SmallVec<[NodeId; 2]>,
    pub has_popup: Option<HasPopup>,
    pub described_by: Option<NodeId>,
    pub a11y_parent: Option<NodeId>,
    pub active_descendant: Option<NodeId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrimSpec {
    pub density: f32,
    pub color: crate::paint::color::Color,
}

impl Default for ScrimSpec {
    fn default() -> Self {
        Self {
            density: 0.0,
            color: crate::paint::color::Color::BLACK,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PopupSpec {
    pub parent: Option<PopupId>,
    pub tier: PopupTier,
    pub anchor: Anchor,
    pub placement: PlacementSpec,
    pub dismiss: DismissPolicy,
    pub focus: FocusPolicy,
    pub opener: Option<NodeId>,
    pub pointer_transparent: bool,
    pub animation: PopupAnimation,
    pub a11y: PopupA11y,
    pub z: ZPolicy,
    pub scrim: Option<ScrimSpec>,
}

impl PopupSpec {
    #[must_use]
    pub fn new(anchor: Anchor, placement: PlacementSpec) -> Self {
        Self {
            parent: None,
            tier: PopupTier::Auto,
            anchor,
            placement,
            dismiss: DismissPolicy::default(),
            focus: FocusPolicy::Take {
                restore: RestoreFocus::Opener,
            },
            opener: None,
            pointer_transparent: false,
            animation: PopupAnimation::Fade,
            a11y: PopupA11y::default(),
            z: ZPolicy::Newest,
            scrim: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopupDismiss {
    pub id: PopupId,
    pub reason: DismissReason,
    pub pointer: Option<LogicalPoint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DismissReason {
    OutsidePress,
    Escape,
    Toggle,
    AnchorLost,
    FocusLost,
    ParentClosed,
    A11yDismiss,
    Programmatic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DismissOutcome {
    Close,
    CloseChain,
    Ignore,
}

#[derive(Debug, PartialEq)]
pub struct PopupStackSnapshot {
    pub window: WindowId,
    pub root: NodeId,
    pub entries: Vec<PopupSnapshotEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopupSnapshotEntry {
    pub id: PopupId,
    pub node: NodeId,
    pub tier: PopupTier,
    pub parent: Option<PopupId>,
    pub z: u64,
    pub state: PopupState,
    pub hit_rect: LogicalRect,
    pub paint_rect: LogicalRect,
    pub owner_modal: Option<PopupId>,
    pub focus: FocusPolicy,
    pub paint_offset: LogicalPoint,
    pub paint_scale: f32,
    pub constrained_size: Option<LogicalSize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Top,
    Bottom,
    Left,
    Right,
}

impl Side {
    #[must_use]
    pub fn opposite(self) -> Self {
        match self {
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }

    #[must_use]
    pub fn is_vertical(self) -> bool {
        matches!(self, Self::Top | Self::Bottom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Alignment {
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Placement {
    pub side: Side,
    pub align: Alignment,
}

impl Placement {
    pub const fn new(side: Side, align: Alignment) -> Self {
        Self { side, align }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShiftPolicy {
    None,
    Limit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaxSize {
    Viewport,
    Width,
    Height,
    WidthAndHeight,
}

impl MaxSize {
    #[must_use]
    pub fn shrinks_width(self) -> bool {
        matches!(self, Self::Viewport | Self::Width | Self::WidthAndHeight)
    }

    #[must_use]
    pub fn shrinks_height(self) -> bool {
        matches!(self, Self::Viewport | Self::Height | Self::WidthAndHeight)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anchor {
    Node { node: NodeId },
    WindowRect(LogicalRect),
    NodeRect { node: NodeId, rect: LogicalRect },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlacementSpec {
    pub side: Side,
    pub align: Alignment,
    pub gap: LogicalPx,
    pub fallbacks: SmallVec<[Placement; 4]>,
    pub viewport_margin: LogicalPx,
    pub shift: ShiftPolicy,
    pub max_size: MaxSize,
}

impl PlacementSpec {
    #[must_use]
    pub fn submenu() -> Self {
        Self {
            side: Side::Right,
            align: Alignment::Start,
            fallbacks: smallvec::smallvec![Placement::new(Side::Left, Alignment::Start)],
            ..Self::default()
        }
    }
}

impl Default for PlacementSpec {
    fn default() -> Self {
        Self {
            side: Side::Bottom,
            align: Alignment::Start,
            gap: crate::math::px(4.0),
            fallbacks: smallvec::smallvec![Placement::new(Side::Top, Alignment::Start)],
            viewport_margin: crate::math::px(4.0),
            shift: ShiftPolicy::Limit,
            max_size: MaxSize::Viewport,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlacementInput {
    pub anchor: LogicalRect,
    pub size: LogicalSize,
    pub viewport: LogicalSize,
    pub avoid: Option<LogicalRect>,
    pub spec: PlacementSpec,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacementResult {
    pub rect: LogicalRect,
    pub side: Side,
    pub resized: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PopupError {
    #[error("popup stack window {window:?} does not exist")]
    UnknownWindow { window: WindowId },

    #[error("popup {id:?} is not in its stack")]
    UnknownPopup { id: PopupId },

    #[error("popup {id:?} belongs to a closed allocation")]
    StalePopup { id: PopupId },

    #[error(
        "invalid popup geometry: anchor {anchor:?}, size {size:?}, viewport {viewport:?}, avoid \
         {avoid:?}; all values must be finite and non-negative"
    )]
    InvalidGeometry {
        anchor: LogicalRect,
        size: LogicalSize,
        viewport: LogicalSize,
        avoid: Option<LogicalRect>,
    },

    #[error(
        "popup scrim requires tier Modal and FocusPolicy::Trap; got tier {tier:?} and focus {focus:?}"
    )]
    InvalidScrim { tier: PopupTier, focus: FocusPolicy },

    #[error(
        "invalid popup placement: gap {gap} and viewport margin {margin} must be finite and \
         non-negative"
    )]
    InvalidPlacement { gap: LogicalPx, margin: LogicalPx },

    #[error("popup parent chain for {id:?} is cyclic")]
    Cycle { id: PopupId },

    #[error("popup depth {depth} exceeds the supported maximum")]
    DepthExceeded { depth: usize },

    #[error("anchor node {node:?} does not exist in this window")]
    AnchorNodeMissing { node: NodeId },

    #[error("hint popup cannot open while modal window {window:?} is active")]
    HintBlockedByModal { window: WindowId },

    #[error("popup stack synchronization failed: {source}")]
    StackSync {
        #[source]
        source: EngineError,
    },

    #[error("popup content build failed at node {node:?}: {source}")]
    ContentBuild {
        node: NodeId,
        #[source]
        source: EngineError,
    },

    #[error("popup stack limit of {max} entries reached")]
    LimitReached { max: usize },
}

pub(crate) type PopupDismissHandler =
    Box<dyn Fn(&mut App, PopupDismiss) -> DismissOutcome + 'static>;

pub struct PopupEntry {
    pub id: PopupId,
    pub node: NodeId,
    pub owner_modal: Option<PopupId>,
    pub parent: Option<PopupId>,
    pub child: Option<PopupId>,
    pub tier: PopupTier,
    pub scrim_for: Option<PopupId>,
    pub state: PopupState,
    pub anchor: Anchor,
    pub placement: PlacementSpec,
    pub dismiss: DismissPolicy,
    pub focus: FocusPolicy,
    pub opener: Option<NodeId>,
    pub pointer_transparent: bool,
    pub animation: PopupAnimation,
    pub a11y: PopupA11y,
    pub z: u64,
    pub hit_rect: LogicalRect,
    pub paint_rect: LogicalRect,
    pub a11y_rect: LogicalRect,
    pub paint_offset: LogicalPoint,
    pub constrained_size: Option<LogicalSize>,
}

impl PopupEntry {
    #[must_use]
    pub fn is_modal(&self) -> bool {
        self.focus.is_modal()
    }

    #[must_use]
    pub(crate) fn is_fade_only(&self) -> bool {
        self.is_modal() || matches!(self.tier, PopupTier::Modal | PopupTier::Scrim)
    }

    #[must_use]
    pub(crate) fn paint_center(&self) -> LogicalPoint {
        let rect = if self.paint_rect.is_empty() {
            self.hit_rect
        } else {
            self.paint_rect
        };
        LogicalPoint::new(
            rect.origin.x + rect.size.width / 2.0,
            rect.origin.y + rect.size.height / 2.0,
        )
    }
}

pub struct PopupStack {
    window: WindowId,
    root: NodeId,
    next_z: u64,
    entries: Vec<Option<PopupEntry>>,
    generations: Vec<u32>,
    free_slots: Vec<u32>,
    order: Vec<PopupId>,
    handlers: Vec<Option<PopupDismissHandler>>,
    dirty: PopupDirty,
    viewport: LogicalSize,
    animations: Vec<PopupAnimationState>,
    focus: PopupFocusState,
    diagnostics: SmallVec<[PopupDiagnostic; 4]>,
    max_sizes: SmallVec<[(PopupId, crate::node::tree::MaxSize); 8]>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PopupAnimationState {
    pub id: PopupId,
    pub kind: PopupAnimationKind,
    pub started: Instant,
    pub paint_offset: LogicalPoint,
    pub opacity: f32,
    pub scale_from: f32,
    pub scale_to: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PopupAnimationKind {
    Opening,
    Closing,
}

pub(crate) const POPUP_OPEN_DURATION: std::time::Duration = std::time::Duration::from_millis(120);

pub(crate) const POPUP_CLOSE_DURATION: std::time::Duration = std::time::Duration::from_millis(90);

pub(crate) const POPUP_SLIDE_DISTANCE: LogicalPx = LogicalPx::new(4.0);

pub(crate) const POPUP_SCALE_FROM: f32 = 0.98;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PopupDiagnostic {
    Unsettled { id: PopupId },
    AnchorLost { id: PopupId },
    Resized { id: PopupId, size: LogicalSize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FocusSnapshot {
    pub node: Option<NodeId>,
    pub owner: Option<PopupId>,
    pub opener: Option<NodeId>,
    pub restore: RestoreFocus,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PopupFocusState {
    pub records: SmallVec<[(PopupId, FocusSnapshot); 4]>,
    pub owner: Option<(PopupId, Option<NodeId>)>,
}

pub(crate) struct SettleOutcome {
    pub rounds: usize,
    pub layout: Option<crate::layout::LayoutReport>,
    pub text_layout: Option<crate::text::TextLayoutReport>,
    pub unsettled: bool,
}
