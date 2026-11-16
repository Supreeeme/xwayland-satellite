//! Named X cursors forwarded as `wp_cursor_shape_v1` shapes.
//!
//! Xwayland only ever gives the compositor a cursor *image*, drawn at whatever
//! size the X client picked. Many X clients ignore the DPI and draw the nominal
//! size, which then has to be upscaled on a HiDPI output and looks blurry.
//!
//! The X server does know which cursor it is, though: libXcursor registers the
//! theme name of every cursor it loads (`XFixesSetCursorName`), and XFixes
//! reports the name of the displayed cursor whenever it changes. When that name
//! has a cursor-shape equivalent, the compositor is asked for the shape instead,
//! and draws it from its own theme at the output's real scale. Unnamed cursors
//! (custom images, animation frames) keep using the image.
//!
//! The name arrives on the X connection and the image on the Wayland one, in no
//! guaranteed order. Each pointer therefore remembers the last cursor Xwayland
//! set on it, and the decision between shape and image is re-made whenever
//! either side changes; the later of the two always wins.

use hecs::Entity;
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Shape;

/// X cursor names and their cursor-shape equivalents.
///
/// Covers the CSS names (which cursor themes also ship under their own names)
/// and the traditional X names, following the aliases cursor themes and
/// toolkits use for the same images. Names whose image differs between themes
/// (`dnd-*`, `circle`, the mirrored `right_ptr`) are left to the image.
const X_CURSOR_SHAPES: &[(&str, Shape)] = &[
    ("default", Shape::Default),
    ("left_ptr", Shape::Default),
    ("arrow", Shape::Default),
    ("top_left_arrow", Shape::Default),
    ("context-menu", Shape::ContextMenu),
    ("help", Shape::Help),
    ("question_arrow", Shape::Help),
    ("whats_this", Shape::Help),
    ("left_ptr_help", Shape::Help),
    ("pointer", Shape::Pointer),
    ("hand", Shape::Pointer),
    ("hand1", Shape::Pointer),
    ("hand2", Shape::Pointer),
    ("pointing_hand", Shape::Pointer),
    ("progress", Shape::Progress),
    ("left_ptr_watch", Shape::Progress),
    ("half-busy", Shape::Progress),
    ("wait", Shape::Wait),
    ("watch", Shape::Wait),
    ("clock", Shape::Wait),
    ("cell", Shape::Cell),
    ("plus", Shape::Cell),
    ("crosshair", Shape::Crosshair),
    ("cross", Shape::Crosshair),
    ("tcross", Shape::Crosshair),
    ("text", Shape::Text),
    ("xterm", Shape::Text),
    ("ibeam", Shape::Text),
    ("vertical-text", Shape::VerticalText),
    ("alias", Shape::Alias),
    ("link", Shape::Alias),
    ("copy", Shape::Copy),
    ("move", Shape::Move),
    ("fleur", Shape::Move),
    ("size_all", Shape::Move),
    ("no-drop", Shape::NoDrop),
    ("not-allowed", Shape::NotAllowed),
    ("forbidden", Shape::NotAllowed),
    ("crossed_circle", Shape::NotAllowed),
    ("grab", Shape::Grab),
    ("openhand", Shape::Grab),
    ("grabbing", Shape::Grabbing),
    ("closedhand", Shape::Grabbing),
    ("e-resize", Shape::EResize),
    ("right_side", Shape::EResize),
    ("n-resize", Shape::NResize),
    ("top_side", Shape::NResize),
    ("ne-resize", Shape::NeResize),
    ("top_right_corner", Shape::NeResize),
    ("nw-resize", Shape::NwResize),
    ("top_left_corner", Shape::NwResize),
    ("s-resize", Shape::SResize),
    ("bottom_side", Shape::SResize),
    ("se-resize", Shape::SeResize),
    ("bottom_right_corner", Shape::SeResize),
    ("sw-resize", Shape::SwResize),
    ("bottom_left_corner", Shape::SwResize),
    ("w-resize", Shape::WResize),
    ("left_side", Shape::WResize),
    ("ew-resize", Shape::EwResize),
    ("sb_h_double_arrow", Shape::EwResize),
    ("h_double_arrow", Shape::EwResize),
    ("size_hor", Shape::EwResize),
    ("ns-resize", Shape::NsResize),
    ("sb_v_double_arrow", Shape::NsResize),
    ("v_double_arrow", Shape::NsResize),
    ("size_ver", Shape::NsResize),
    ("nesw-resize", Shape::NeswResize),
    ("fd_double_arrow", Shape::NeswResize),
    ("size_bdiag", Shape::NeswResize),
    ("nwse-resize", Shape::NwseResize),
    ("bd_double_arrow", Shape::NwseResize),
    ("size_fdiag", Shape::NwseResize),
    ("col-resize", Shape::ColResize),
    ("split_h", Shape::ColResize),
    ("row-resize", Shape::RowResize),
    ("split_v", Shape::RowResize),
    ("all-scroll", Shape::AllScroll),
    ("zoom-in", Shape::ZoomIn),
    ("zoom-out", Shape::ZoomOut),
];

/// The cursor-shape equivalent of an X cursor name.
pub(crate) fn shape_for_x_cursor_name(name: &str) -> Option<Shape> {
    X_CURSOR_SHAPES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, shape)| *shape)
}

/// Every X cursor name that has a cursor-shape equivalent.
pub(crate) fn known_x_cursor_names() -> impl Iterator<Item = &'static str> + Clone {
    X_CURSOR_SHAPES.iter().map(|(name, _)| *name)
}

/// What the compositor was last told for a pointer.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) enum SentCursor {
    Hidden,
    Shape(Shape),
    Image {
        surface: Entity,
        hotspot: (i32, i32),
    },
}

/// The cursor Xwayland last set on a pointer, and what was forwarded for it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct PointerCursor {
    pub serial: u32,
    /// The cursor surface, or `None` when Xwayland hid the cursor.
    pub surface: Option<Entity>,
    /// Hotspot in logical coordinates, as it would be forwarded with the image.
    pub hotspot: (i32, i32),
    pub sent: Option<(u32, SentCursor)>,
}

impl PointerCursor {
    /// What to send for this cursor, given the shape of the X cursor currently
    /// displayed and whether the compositor supports cursor shapes.
    pub fn wanted(&self, shape: Option<Shape>, can_shape: bool) -> SentCursor {
        match (self.surface, shape) {
            (None, _) => SentCursor::Hidden,
            (Some(_), Some(shape)) if can_shape => SentCursor::Shape(shape),
            (Some(surface), _) => SentCursor::Image {
                surface,
                hotspot: self.hotspot,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x_and_css_names_map_to_the_same_shape() {
        assert_eq!(shape_for_x_cursor_name("left_ptr"), Some(Shape::Default));
        assert_eq!(shape_for_x_cursor_name("default"), Some(Shape::Default));
        assert_eq!(shape_for_x_cursor_name("xterm"), Some(Shape::Text));
        assert_eq!(shape_for_x_cursor_name("text"), Some(Shape::Text));
        assert_eq!(shape_for_x_cursor_name("hand2"), Some(Shape::Pointer));
        assert_eq!(shape_for_x_cursor_name("watch"), Some(Shape::Wait));
        assert_eq!(
            shape_for_x_cursor_name("sb_h_double_arrow"),
            Some(Shape::EwResize)
        );
        assert_eq!(
            shape_for_x_cursor_name("sb_v_double_arrow"),
            Some(Shape::NsResize)
        );
        assert_eq!(
            shape_for_x_cursor_name("top_left_corner"),
            Some(Shape::NwResize)
        );
    }

    #[test]
    fn unknown_names_keep_the_image() {
        assert_eq!(shape_for_x_cursor_name(""), None);
        assert_eq!(shape_for_x_cursor_name("my-custom-crosshair"), None);
        assert_eq!(shape_for_x_cursor_name("right_ptr"), None);
    }
}
