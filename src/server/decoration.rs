use crate::color_scheme::{current_color_scheme, ColorScheme};
use crate::server::clientside::MyWorld;
use crate::server::{InnerServerState, ServerState, SurfaceRole, WindowData};
use crate::{ToplevelCapabilities, X11Selection, XConnection};

use ab_glyph::{Font, FontRef, Glyph, PxScaleFont, ScaleFont};
use hecs::{CommandBuffer, Entity, World};
use log::{error, warn};
use smithay_client_toolkit::registry::SimpleGlobal;
use smithay_client_toolkit::shm::slot::SlotPool;
use std::borrow::Cow;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tiny_skia::{BlendMode, Color, FillRule, Paint, PathBuilder, Pixmap, Point, Transform};
use tiny_skia::{ColorU8, PremultipliedColorU8, Rect};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::Proxy;
use wayland_client::QueueHandle;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1;
use wayland_protocols::xdg::shell::client::xdg_toplevel::{self};
use xcb::x;

pub const RESIZE_HANDLE_SIZE: i32 = 12;
const WINDOW_SHADOW_EXTENT: i32 = 16;
const WINDOW_SHADOW_BLUR_PASSES: usize = 3;
const BUTTON_HOVER_TRANSITION_DURATION: Duration = Duration::from_millis(500);
const BUTTON_ACTIVE_TRANSITION_DURATION: Duration = Duration::from_millis(200);
const UNFOCUS_TRANSITION_DURATION: Duration = Duration::from_millis(200);

static DARK_DECORATION_THEME: LazyLock<DecorationTheme> =
    LazyLock::new(DecorationTheme::dark);
static LIGHT_DECORATION_THEME: LazyLock<DecorationTheme> =
    LazyLock::new(DecorationTheme::light);

fn decoration_theme() -> &'static DecorationTheme {
    match current_color_scheme() {
        ColorScheme::Light => &LIGHT_DECORATION_THEME,
        ColorScheme::Dark => &DARK_DECORATION_THEME,
    }
}

#[derive(Debug, Clone, Copy)]
struct Insets {
    top: f32,
    right: f32,
    bottom: f32,
    left: f32,
}

impl Insets {
    const fn all(value: f32) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }

    const fn symmetric(vertical: f32, horizontal: f32) -> Self {
        Self {
            top: vertical,
            right: horizontal,
            bottom: vertical,
            left: horizontal,
        }
    }

    fn horizontal(self) -> f32 {
        self.left + self.right
    }

    fn vertical(self) -> f32 {
        self.top + self.bottom
    }
}

#[derive(Debug, Clone, Copy)]
enum TimingFunction {
    EaseOut,
    EaseInOut,
}

impl TimingFunction {
    fn apply(self, progress: f32) -> f32 {
        let progress = progress.clamp(0.0, 1.0);
        match self {
            Self::EaseOut => 1.0 - (1.0 - progress).powi(3),
            Self::EaseInOut => {
                if progress < 0.5 {
                    4.0 * progress * progress * progress
                } else {
                    1.0 - (-2.0 * progress + 2.0).powi(3) / 2.0
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TransitionSpec {
    duration: Duration,
    easing: TimingFunction,
}

impl TransitionSpec {
    fn progress(self, started_at: Instant, now: Instant) -> (f32, bool) {
        if self.duration.is_zero() {
            return (1.0, false);
        }

        let elapsed = now.duration_since(started_at);
        if elapsed >= self.duration {
            return (1.0, false);
        }

        (
            self.easing
                .apply(elapsed.as_secs_f32() / self.duration.as_secs_f32()),
            true,
        )
    }
}

trait ColorExt {
    fn with_coverage(self, coverage: f32) -> PremultipliedColorU8;
    fn lerp_color(self, other: Self, progress: f32) -> Self;
}

impl ColorExt for Color {
    fn with_coverage(self, coverage: f32) -> PremultipliedColorU8 {
        let mut color = self;
        color.apply_opacity(coverage);
        color.premultiply().to_color_u8()
    }

    fn lerp_color(self, other: Self, progress: f32) -> Self {
        let progress = progress.clamp(0.0, 1.0);
        Color::from_rgba(
            self.red() + (other.red() - self.red()) * progress,
            self.green() + (other.green() - self.green()) * progress,
            self.blue() + (other.blue() - self.blue()) * progress,
            self.alpha() + (other.alpha() - self.alpha()) * progress,
        )
        .unwrap()
    }
}

#[derive(Debug, Clone, Copy)]
struct WindowShadow {
    offset_x: f32,
    offset_y: f32,
    blur: f32,
    spread: f32,
    color: Color,
}

#[derive(Debug)]
struct DecorationTheme {
    foreground: Color,
    titlebar_background: Color,
    titlebar_backdrop_background: Color,
    title_backdrop_opacity: f32,
    titlebar_border_color: Color,
    titlebar_top_highlight_color: Color,
    window_ring_color: Color,
    window_backdrop_ring_color: Color,
    window_tiled_ring_color: Color,
    window_shadow: WindowShadow,
    window_backdrop_shadow: WindowShadow,
    titlebar_border_bottom_width: f32,
    window_corner_radius: f32,
    default_decoration_padding: Insets,
    title_padding: Insets,
    default_decoration_min_height: f32,
    controls_spacing: f32,
    controls_leading_margin: f32,
    controls_edge_padding: f32,
    button_circle_diameter: f32,
    button_glyph_size: f32,
    button_backdrop_foreground: Color,
    button_background: Color,
    button_hover_background: Color,
    button_active_background: Color,
    unfocus_transition: TransitionSpec,
    button_hover_transition: TransitionSpec,
    button_active_transition: TransitionSpec,
}

impl DecorationTheme {
    fn base() -> Self {
        Self {
            foreground: Color::BLACK,
            titlebar_background: Color::BLACK,
            titlebar_backdrop_background: Color::BLACK,
            title_backdrop_opacity: 0.5,
            titlebar_border_color: Color::BLACK,
            titlebar_top_highlight_color: Color::TRANSPARENT,
            window_ring_color: Color::BLACK,
            window_backdrop_ring_color: Color::BLACK,
            window_tiled_ring_color: Color::BLACK,
            window_shadow: WindowShadow {
                offset_x: 0.0,
                offset_y: 3.0,
                blur: 9.0,
                spread: 1.0,
                color: Color::from_rgba8(0, 0, 0, 128),
            },
            window_backdrop_shadow: WindowShadow {
                offset_x: 0.0,
                offset_y: 2.0,
                blur: 6.0,
                spread: 2.0,
                color: Color::from_rgba8(0, 0, 0, 51),
            },
            titlebar_border_bottom_width: 1.0,
            window_corner_radius: 12.0,
            default_decoration_padding: Insets::all(4.0),
            title_padding: Insets::symmetric(0.0, 12.0),
            default_decoration_min_height: 28.0,
            controls_spacing: 16.0,
            controls_leading_margin: 0.0,
            controls_edge_padding: 11.0,
            button_circle_diameter: 24.0,
            button_glyph_size: 16.0,
            button_backdrop_foreground: Color::BLACK,
            button_background: Color::TRANSPARENT,
            button_hover_background: Color::TRANSPARENT,
            button_active_background: Color::TRANSPARENT,
            unfocus_transition: TransitionSpec {
                duration: UNFOCUS_TRANSITION_DURATION,
                easing: TimingFunction::EaseOut,
            },
            button_hover_transition: TransitionSpec {
                duration: BUTTON_HOVER_TRANSITION_DURATION,
                easing: TimingFunction::EaseInOut,
            },
            button_active_transition: TransitionSpec {
                duration: BUTTON_ACTIVE_TRANSITION_DURATION,
                easing: TimingFunction::EaseInOut,
            },
        }
    }

    fn dark() -> Self {
        Self {
            foreground: Color::from_rgba8(247, 247, 247, 255),
            titlebar_background: Color::from_rgba8(34, 34, 34, 255),
            titlebar_backdrop_background: Color::from_rgba8(44, 44, 44, 255),
            titlebar_border_color: Color::BLACK,
            titlebar_top_highlight_color: Color::from_rgba8(247, 247, 247, 18),
            window_ring_color: Color::from_rgba8(0, 0, 0, 128),
            window_backdrop_ring_color: Color::from_rgba8(0, 0, 0, 128),
            window_tiled_ring_color: Color::from_rgba8(0, 0, 0, 128),
            button_backdrop_foreground: Color::from_rgba8(255, 255, 255, 166),
            button_background: Color::from_rgba8(247, 247, 247, 26),
            button_hover_background: Color::from_rgba8(244, 244, 244, 38),
            button_active_background: Color::from_rgba8(234, 234, 234, 64),
            ..Self::base()
        }
    }

    fn light() -> Self {
        Self {
            foreground: Color::from_rgba8(61, 61, 61, 255),
            titlebar_background: Color::from_rgba8(235, 235, 235, 255),
            titlebar_backdrop_background: Color::from_rgba8(250, 250, 250, 255),
            titlebar_border_color: Color::from_rgba8(189, 189, 189, 255),
            titlebar_top_highlight_color: Color::from_rgba8(255, 255, 255, 204),
            window_ring_color: Color::from_rgba8(0, 0, 0, 59),
            window_backdrop_ring_color: Color::from_rgba8(0, 0, 0, 46),
            window_tiled_ring_color: Color::from_rgba8(0, 0, 0, 46),
            button_backdrop_foreground: Color::from_rgba8(99, 99, 99, 255),
            button_background: Color::from_rgba8(61, 61, 61, 26),
            button_hover_background: Color::from_rgba8(25, 25, 25, 38),
            button_active_background: Color::from_rgba8(10, 10, 10, 64),
            ..Self::base()
        }
    }

    fn effective_titlebar_height(&self) -> i32 {
        (self.default_decoration_min_height + self.default_decoration_padding.vertical())
            .ceil()
            .max(1.0) as i32
    }

    fn button_size(&self) -> f32 {
        self.button_circle_diameter
    }

    fn button_total_width(&self, count: u32) -> f32 {
        if count == 0 {
            return 0.0;
        }

        self.controls_leading_margin
            + self.controls_edge_padding
            + self.button_size() * count as f32
            + self.controls_spacing * count.saturating_sub(1) as f32
    }

    fn button_background_for_state(&self, hovered: bool, active: bool) -> Color {
        if active {
            self.button_active_background
        } else if hovered {
            self.button_hover_background
        } else {
            self.button_background
        }
    }

    fn title_color_for_focus(&self, focus_progress: f32) -> Color {
        let mut color = self.foreground;
        color.apply_opacity(
            self.title_backdrop_opacity
                + (1.0 - self.title_backdrop_opacity) * focus_progress.clamp(0.0, 1.0),
        );
        color
    }

    fn button_foreground_for_focus(&self, focus_progress: f32) -> Color {
        self.button_backdrop_foreground
            .lerp_color(self.foreground, focus_progress.clamp(0.0, 1.0))
    }

    fn window_ring_color_for_focus(&self, focused: bool) -> Color {
        if focused {
            self.window_ring_color
        } else {
            self.window_backdrop_ring_color
        }
    }

    fn window_shadow_for_focus(&self, focused: bool) -> WindowShadow {
        if focused {
            self.window_shadow
        } else {
            self.window_backdrop_shadow
        }
    }
}

fn resize_edge_for_frame_rect(
    width: i32,
    height: i32,
    frame_border: i32,
    resize_border: i32,
    x: f64,
    y: f64,
    allow_top: bool,
    allow_bottom: bool,
) -> Option<xdg_toplevel::ResizeEdge> {
    if width <= 0 || height <= 0 || resize_border <= 0 || frame_border < resize_border {
        return None;
    }

    let inactive_outer = f64::from(frame_border - resize_border);
    let visible_left = f64::from(frame_border);
    let visible_top = f64::from(frame_border);
    let visible_right = f64::from(width - frame_border);
    let visible_bottom = f64::from(height - frame_border);
    let outer_right = f64::from(width) - inactive_outer;
    let outer_bottom = f64::from(height) - inactive_outer;

    let left = x >= inactive_outer && x <= visible_left;
    let right = x >= visible_right && x <= outer_right;
    let top = allow_top && y >= inactive_outer && y <= visible_top;
    let bottom = allow_bottom && y >= visible_bottom && y <= outer_bottom;

    match (left, right, top, bottom) {
        (true, false, true, false) => Some(xdg_toplevel::ResizeEdge::TopLeft),
        (false, false, true, false) => Some(xdg_toplevel::ResizeEdge::Top),
        (false, true, true, false) => Some(xdg_toplevel::ResizeEdge::TopRight),
        (false, true, false, false) => Some(xdg_toplevel::ResizeEdge::Right),
        (false, true, false, true) => Some(xdg_toplevel::ResizeEdge::BottomRight),
        (false, false, false, true) => Some(xdg_toplevel::ResizeEdge::Bottom),
        (true, false, false, true) => Some(xdg_toplevel::ResizeEdge::BottomLeft),
        (true, false, false, false) => Some(xdg_toplevel::ResizeEdge::Left),
        _ => None,
    }
}

#[derive(Debug)]
pub struct DecorationsData {
    pub wl: Option<ZxdgToplevelDecorationV1>,
    pub satellite: Option<Box<DecorationsDataSatellite>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecorationPart {
    Frame,
    Titlebar,
}

pub struct DecorationMarker {
    pub parent: Entity,
    pub part: DecorationPart,
}

#[derive(Debug, Clone, Copy)]
pub struct DecorationFrameCallback {
    pub parent: Entity,
    pub generation: u64,
}

#[derive(Debug)]
struct DecorationSurface {
    surface: WlSurface,
    subsurface: WlSubsurface,
    viewport: WpViewport,
    pixmap: Pixmap,
}

impl Drop for DecorationSurface {
    fn drop(&mut self) {
        self.subsurface.destroy();
        self.surface.destroy();
        self.viewport.destroy();
    }
}

#[derive(Debug)]
pub struct DecorationsDataSatellite {
    compositor: WlCompositor,
    qh: QueueHandle<MyWorld>,
    frame_surface: DecorationSurface,
    titlebar_surface: DecorationSurface,
    pool: Entity,
    scale: f32,
    buttons: DecorationsButtons,
    title: Option<String>,
    width: i32,
    height: i32,
    focused: bool,
    focus_progress: f32,
    maximized: bool,
    tiled: bool,
    resizable: bool,
    resize_input_enabled: bool,
    resize_interaction_active: bool,
    draw_titlebar: bool,
    pressed_button: Option<DecorationButtonKind>,
    capabilities: ToplevelCapabilities,
    hovered_resize_edge: Option<xdg_toplevel::ResizeEdge>,
    focus_transition: Option<FocusTransition>,
    parent: Entity,
    animation_frame_generation: u64,
    pending_animation_frame: Option<u64>,
    pending_resize_frame: Option<u64>,
    resize_redraw_pending: bool,
    should_draw: bool,
    remove_buffer: bool,
}

impl DecorationsDataSatellite {
    fn titlebar_height_for(draw_titlebar: bool) -> i32 {
        if draw_titlebar {
            decoration_theme().effective_titlebar_height()
        } else {
            0
        }
    }

    fn resize_border_for(maximized: bool, resizable: bool) -> i32 {
        if !maximized && resizable {
            RESIZE_HANDLE_SIZE
        } else {
            0
        }
    }

    fn shadow_border_for(maximized: bool, tiled: bool) -> i32 {
        if maximized || tiled {
            0
        } else {
            WINDOW_SHADOW_EXTENT
        }
    }

    fn frame_border_for(maximized: bool, tiled: bool, resizable: bool) -> i32 {
        Self::resize_border_for(maximized, resizable)
            .max(Self::shadow_border_for(maximized, tiled))
    }

    fn geometry_border_for(maximized: bool, resizable: bool) -> i32 {
        Self::resize_border_for(maximized, resizable)
    }

    fn frame_width_for(width: i32, maximized: bool, tiled: bool, resizable: bool) -> i32 {
        width + Self::frame_border_for(maximized, tiled, resizable) * 2
    }

    fn frame_height_for(
        height: i32,
        maximized: bool,
        tiled: bool,
        resizable: bool,
        draw_titlebar: bool,
    ) -> i32 {
        Self::titlebar_height_for(draw_titlebar)
            + height
            + Self::frame_border_for(maximized, tiled, resizable) * 2
    }

    fn titlebar_offset_for(maximized: bool, tiled: bool, resizable: bool) -> i32 {
        Self::frame_border_for(maximized, tiled, resizable)
    }

    fn frame_border(&self) -> i32 {
        Self::frame_border_for(self.maximized, self.tiled, self.resizable)
    }

    fn resize_border(&self) -> i32 {
        Self::resize_border_for(self.maximized, self.resizable)
    }

    fn frame_width(&self) -> i32 {
        Self::frame_width_for(self.width, self.maximized, self.tiled, self.resizable)
    }

    fn frame_height(&self) -> i32 {
        Self::frame_height_for(
            self.height,
            self.maximized,
            self.tiled,
            self.resizable,
            self.draw_titlebar,
        )
    }

    pub(crate) fn titlebar_height(&self) -> i32 {
        Self::titlebar_height_for(self.draw_titlebar)
    }

    pub(crate) fn window_geometry_for(
        width: i32,
        height: i32,
        maximized: bool,
        resizable: bool,
        draw_titlebar: bool,
    ) -> (i32, i32, i32, i32) {
        let border = Self::geometry_border_for(maximized, resizable);
        let titlebar_height = Self::titlebar_height_for(draw_titlebar);
        (
            -border,
            -(titlebar_height + border),
            width + border * 2,
            titlebar_height + height + border * 2,
        )
    }

    pub(crate) fn content_size_for_window_geometry(
        width: i32,
        height: i32,
        maximized: bool,
        resizable: bool,
        draw_titlebar: bool,
    ) -> (i32, i32) {
        let border = Self::geometry_border_for(maximized, resizable);
        let titlebar_height = Self::titlebar_height_for(draw_titlebar);
        (
            (width - border * 2).max(1),
            (height - (titlebar_height + border * 2)).max(1),
        )
    }

    fn set_empty_input_region(&self, surface: &WlSurface) {
        let region = self.compositor.create_region(&self.qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
    }

    fn update_input_region(&self) {
        let frame_width = self.frame_width();
        let frame_height = self.frame_height();
        if frame_width <= 0 || frame_height <= 0 {
            self.set_empty_input_region(&self.frame_surface.surface);
            self.set_empty_input_region(&self.titlebar_surface.surface);
            return;
        }

        let border = self.frame_border();
        let resize_border = self.resize_border();
        let titlebar_offset =
            Self::titlebar_offset_for(self.maximized, self.tiled, self.resizable);
        let titlebar_height = self.titlebar_height();
        let region = self.compositor.create_region(&self.qh, ());
        if resize_border > 0 && self.resize_input_enabled {
            let inset = border - resize_border;
            let visible_height = titlebar_height + self.height;
            let hit_width = self.width + resize_border * 2;
            region.add(inset, inset, hit_width, resize_border);
            region.add(inset, titlebar_offset, resize_border, visible_height);
            region.add(
                titlebar_offset + self.width,
                titlebar_offset,
                resize_border,
                visible_height,
            );
            region.add(inset, titlebar_offset + visible_height, hit_width, resize_border);
        }
        self.frame_surface.surface.set_input_region(Some(&region));
        region.destroy();

        let region = self.compositor.create_region(&self.qh, ());
        if self.draw_titlebar && titlebar_height > 0 {
            region.add(0, 0, self.width, titlebar_height);
        }
        self.titlebar_surface.surface.set_input_region(Some(&region));
        region.destroy();
    }

    fn surface_point_to_frame_point(
        &self,
        part: DecorationPart,
        surface_x: f64,
        surface_y: f64,
    ) -> (f64, f64) {
        match part {
            DecorationPart::Frame => (surface_x, surface_y),
            DecorationPart::Titlebar => {
                let border = f64::from(self.frame_border());
                (surface_x + border, surface_y + border)
            }
        }
    }

    fn create_surface(
        state: &InnerServerState<impl X11Selection>,
        parent: &WlSurface,
        parent_entity: Entity,
        part: DecorationPart,
        x: i32,
        y: i32,
    ) -> DecorationSurface {
        let surface = state
            .compositor
            .create_surface(&state.qh, DecorationMarker {
                parent: parent_entity,
                part,
            });
        let subsurface = state
            .subcompositor
            .get_subsurface(&surface, parent, &state.qh, ());
        subsurface.set_desync();
        subsurface.set_position(x, y);
        let viewport = state.viewporter.get_viewport(&surface, &state.qh, ());

        DecorationSurface {
            surface,
            subsurface,
            viewport,
            pixmap: Pixmap::new(1, 1).unwrap(),
        }
    }

    pub fn try_new(
        state: &InnerServerState<impl X11Selection>,
        parent: &WlSurface,
        title: Option<&str>,
        draw_titlebar: bool,
    ) -> Option<(Box<Self>, Option<CommandBuffer>)> {
        let mut new_pool = None;
        let mut query = state.world.query::<&SlotPool>();
        let pool_entity = if let Some((pool_entity, _)) = query.into_iter().next() {
            pool_entity
        } else {
            new_pool = Some(
                SlotPool::new(1, &SimpleGlobal::from_bound(state.shm.clone()))
                    .inspect_err(|e| {
                        warn!("Couldn't create slot pool for decorations: {e:?}");
                    })
                    .ok()?,
            );
            state.world.reserve_entity()
        };

        let (resizable, resize_input_enabled) =
            parent_resize_state(&state.world, parent.data().copied().unwrap());
        let border = Self::frame_border_for(false, false, resizable);
        let titlebar_height = Self::titlebar_height_for(draw_titlebar);
        let parent_entity = parent.data().copied().unwrap();
        let titlebar_surface = Self::create_surface(
            state,
            parent,
            parent_entity,
            DecorationPart::Titlebar,
            0,
            -titlebar_height,
        );
        let frame_surface = Self::create_surface(
            state,
            parent,
            parent_entity,
            DecorationPart::Frame,
            -border,
            -(titlebar_height + border),
        );

        Some((
            Self {
                compositor: state.compositor.clone(),
                qh: state.qh.clone(),
                frame_surface,
                titlebar_surface,
                pool: pool_entity,
                buttons: DecorationsButtons::default(),
                scale: 1.0,
                title: title.map(str::to_string),
                width: 0,
                height: 0,
                focused: false,
                focus_progress: 0.0,
                maximized: false,
                tiled: false,
                resizable,
                resize_input_enabled,
                resize_interaction_active: false,
                draw_titlebar,
                pressed_button: None,
                capabilities: ToplevelCapabilities::all(),
                hovered_resize_edge: None,
                focus_transition: None,
                parent: parent_entity,
                animation_frame_generation: 0,
                pending_animation_frame: None,
                pending_resize_frame: None,
                resize_redraw_pending: false,
                should_draw: true,
                remove_buffer: false,
            }
            .into(),
            new_pool.map(|pool| {
                let mut buf = CommandBuffer::new();
                buf.insert_one(pool_entity, pool);
                buf
            }),
        ))
    }

    fn remove_surface_buffer(surface: &DecorationSurface) {
        surface.surface.attach(None, 0, 0);
        surface.surface.commit();
    }

    fn update_surface_buffer(world: &World, pool: Entity, target: &mut DecorationSurface) {
        let mut pool = world.get::<&mut SlotPool>(pool).unwrap();
        let (buffer, data) = match pool.create_buffer(
            target.pixmap.width() as i32,
            target.pixmap.height() as i32,
            target.pixmap.width() as i32 * 4,
            wl_shm::Format::Argb8888,
        ) {
            Ok(buffer) => buffer,
            Err(err) => {
                error!("Failed to create buffer for decorations: {err:?}");
                return;
            }
        };

        draw_pixmap_to_buffer(&target.pixmap, data);
        buffer.attach_to(&target.surface).unwrap();
        target.surface.damage_buffer(
            0,
            0,
            target.pixmap.width() as i32,
            target.pixmap.height() as i32,
        );
        target.surface.commit();
    }

    fn request_animation_frame(&mut self) {
        if self.pending_animation_frame.is_some()
            || !self.has_active_animation()
            || !self.draw_titlebar
            || !self.should_draw
            || self.width <= 0
        {
            return;
        }

        self.animation_frame_generation = self.animation_frame_generation.wrapping_add(1);
        let generation = self.animation_frame_generation;
        self.titlebar_surface.surface.frame(
            &self.qh,
            DecorationFrameCallback {
                parent: self.parent,
                generation,
            },
        );
        self.pending_animation_frame = Some(generation);
    }

    fn request_resize_frame(&mut self) {
        if self.pending_resize_frame.is_some()
            || !self.resize_interaction_active
            || !self.should_draw
            || self.width <= 0
        {
            return;
        }

        self.animation_frame_generation = self.animation_frame_generation.wrapping_add(1);
        let generation = self.animation_frame_generation;
        self.frame_surface.surface.frame(
            &self.qh,
            DecorationFrameCallback {
                parent: self.parent,
                generation,
            },
        );
        self.pending_resize_frame = Some(generation);
    }

    #[must_use]
    pub fn will_draw_decorations(&self, width: i32) -> bool {
        width > 0 && self.should_draw
    }

    pub fn draw_decorations(
        &mut self,
        world: &World,
        width: i32,
        height: i32,
        parent_scale_factor: f32,
    ) {
        if !self.will_draw_decorations(width) {
            if self.remove_buffer {
                self.pending_animation_frame = None;
                self.pending_resize_frame = None;
                self.resize_redraw_pending = false;
                Self::remove_surface_buffer(&self.frame_surface);
                Self::remove_surface_buffer(&self.titlebar_surface);
                self.update_input_region();
                self.remove_buffer = false;
            }
            return;
        }

        self.width = width;
        self.height = height;
        self.scale = parent_scale_factor;

        if self.resize_interaction_active {
            self.resize_redraw_pending = true;
            if self.pending_resize_frame.is_none() {
                self.request_resize_frame();
                self.draw_resize_frame(world);
            }
            return;
        }

        self.pending_resize_frame = None;
        self.resize_redraw_pending = false;
        self.draw_frame(world);
        self.draw_titlebar(world);
        self.update_input_region();
    }

    pub(crate) fn redraw_for_color_scheme(&mut self, world: &World) {
        if self.width > 0 && self.should_draw {
            self.draw_decorations(world, self.width, self.height, self.scale);
        }
    }

    fn draw_resize_frame(&mut self, world: &World) {
        self.resize_redraw_pending = false;
        self.draw_frame(world);
        self.draw_titlebar(world);
        self.update_input_region();
    }

    fn draw_frame(&mut self, world: &World) {
        let theme = decoration_theme();
        let titlebar_height = self.titlebar_height();
        let frame_border = self.frame_border();
        let titlebar_offset =
            Self::titlebar_offset_for(self.maximized, self.tiled, self.resizable);
        let frame_width = self.frame_width();
        let frame_height = self.frame_height();
        let drawn_frame_width = (frame_width as f32 * self.scale).ceil() as i32;
        let drawn_frame_height = (frame_height as f32 * self.scale).ceil() as i32;
        let corner_radius = if self.maximized || self.tiled {
            0.0
        } else {
            theme.window_corner_radius * self.scale
        };

        self.frame_surface
            .subsurface
            .set_position(-frame_border, -(titlebar_height + frame_border));
        if frame_width <= 0
            || frame_height <= 0
            || drawn_frame_width <= 0
            || drawn_frame_height <= 0
        {
            Self::remove_surface_buffer(&self.frame_surface);
            return;
        }
        self.frame_surface
            .viewport
            .set_destination(frame_width, frame_height);

        let mut frame =
            Pixmap::new(drawn_frame_width as u32, drawn_frame_height as u32).unwrap();
        frame.fill(Color::TRANSPARENT);

        if !self.maximized {
            let Some(visible_rect) = scaled_rect(
                titlebar_offset,
                titlebar_offset,
                self.width,
                titlebar_height + self.height,
                self.scale,
            )
            else {
                return;
            };
            if self.tiled {
                draw_window_ring(
                    &mut frame,
                    visible_rect,
                    corner_radius,
                    theme.window_tiled_ring_color,
                    self.scale.max(1.0),
                    RingPlacement::Inside,
                );
            } else {
                if !self.resize_interaction_active {
                    // Full-frame shadow blur is too expensive for every interactive resize step.
                    draw_window_shadow(
                        &mut frame,
                        visible_rect,
                        corner_radius,
                        theme.window_shadow_for_focus(self.focused),
                        self.scale,
                    );
                    clear_window_shape(&mut frame, visible_rect, corner_radius);
                }
                let ring_width = self.scale.max(1.0);
                let ring_rect = inset_positive_edges(visible_rect, ring_width / 2.0)
                    .unwrap_or(visible_rect);
                draw_window_ring(
                    &mut frame,
                    ring_rect,
                    corner_radius,
                    theme.window_ring_color_for_focus(self.focused),
                    ring_width,
                    RingPlacement::Outside,
                );
            }
        }

        self.frame_surface.pixmap = frame;
        Self::update_surface_buffer(world, self.pool, &mut self.frame_surface);
    }

    fn draw_titlebar(&mut self, world: &World) {
        let theme = decoration_theme();
        let now = Instant::now();
        let titlebar_height = self.titlebar_height();
        if !self.draw_titlebar || titlebar_height <= 0 {
            self.buttons.clear_hovered(theme, now);
            self.pending_animation_frame = None;
            Self::remove_surface_buffer(&self.titlebar_surface);
            return;
        }

        let drawn_width = (self.width as f32 * self.scale).ceil() as i32;
        let drawn_titlebar_height = (titlebar_height as f32 * self.scale).ceil() as i32;
        if self.width <= 0 || drawn_width <= 0 || drawn_titlebar_height <= 0 {
            self.pending_animation_frame = None;
            Self::remove_surface_buffer(&self.titlebar_surface);
            return;
        }

        self.titlebar_surface
            .subsurface
            .set_position(0, -titlebar_height);
        self.titlebar_surface
            .viewport
            .set_destination(self.width, titlebar_height);

        let focus_progress = self.resolved_focus_progress(now, theme);
        let titlebar_color = theme
            .titlebar_backdrop_background
            .lerp_color(theme.titlebar_background, focus_progress);
        let title_color = theme.title_color_for_focus(focus_progress);
        let button_foreground = theme.button_foreground_for_focus(focus_progress);
        let corner_radius = if self.maximized || self.tiled {
            0.0
        } else {
            theme.window_corner_radius * self.scale
        };

        let buttons_width = self.buttons.total_width_pixels(theme, self.capabilities) as i32;
        let title_side_reservation = buttons_width
            .max((theme.title_padding.horizontal() * self.scale).ceil() as i32);

        let title = self.title.as_deref().and_then(|title| {
            let width = (drawn_width - title_side_reservation * 2).max(0) as u32;
            if width > 0 {
                title_pixmap(
                    title,
                    width,
                    drawn_titlebar_height as u32,
                    self.scale,
                    theme,
                    title_color,
                )
            } else {
                None
            }
        });

        let mut bar = Pixmap::new(drawn_width as u32, drawn_titlebar_height as u32).unwrap();
        bar.fill(Color::TRANSPARENT);

        let titlebar_rect = Rect::from_xywh(
            0.0,
            0.0,
            drawn_width as f32,
            drawn_titlebar_height as f32,
        )
        .unwrap();
        fill_rounded_titlebar(&mut bar, titlebar_rect, corner_radius, titlebar_color);
        draw_titlebar_top_highlight(
            &mut bar,
            titlebar_rect,
            corner_radius,
            theme.titlebar_top_highlight_color,
            self.scale.max(1.0),
        );

        if theme.titlebar_border_bottom_width > 0.0 {
            let mut border_paint = Paint::default();
            border_paint.set_color(theme.titlebar_border_color);
            let border_height = (theme.titlebar_border_bottom_width * self.scale).max(1.0);
            let border_rect = Rect::from_xywh(
                0.0,
                drawn_titlebar_height as f32 - border_height,
                drawn_width as f32,
                border_height,
            )
            .unwrap();
            bar.fill_rect(border_rect, &border_paint, Transform::identity(), None);
        }

        if let Some(title) = title {
            let title_area_width =
                (drawn_width - title_side_reservation * 2).max(title.width() as i32);
            let title_x =
                title_side_reservation + ((title_area_width - title.width() as i32) / 2).max(0);
            bar.draw_pixmap(
                title_x,
                0,
                title.as_ref(),
                &Default::default(),
                Transform::identity(),
                None,
            );
        }

        self.buttons
            .layout(self.width as f32, titlebar_height as f32, theme, self.capabilities);

        self.buttons.for_each_visible_button(self.capabilities, |button| {
            let mut button_background = button.resolved_background(now, theme);
            button_background.apply_opacity(focus_progress);
            let pixmap = button_pixmap(
                button.kind,
                (button.rect.width() * self.scale).round().max(1.0) as u32,
                (button.rect.height() * self.scale).round().max(1.0) as u32,
                self.scale,
                button_background,
                button_foreground,
                self.maximized,
                theme,
            );
            bar.draw_pixmap(
                (button.rect.left() * self.scale).round() as i32,
                (button.rect.top() * self.scale).round() as i32,
                pixmap.as_ref(),
                &Default::default(),
                Transform::identity(),
                None,
            );
        });

        self.titlebar_surface.pixmap = bar;
        self.request_animation_frame();
        Self::update_surface_buffer(world, self.pool, &mut self.titlebar_surface);
    }

    pub fn set_title(&mut self, world: &World, title: &str) {
        self.title = Some(title.to_string());
        if !self.should_draw || self.width <= 0 {
            return;
        }

        self.draw_titlebar(world);
    }

    pub fn handle_fullscreen(&mut self, fullscreen: bool) {
        if self.should_draw == fullscreen {
            self.should_draw = !fullscreen;
            self.remove_buffer = fullscreen;
        }
    }

    pub fn set_maximized(&mut self, world: &World, maximized: bool) {
        if self.maximized == maximized {
            return;
        }

        self.maximized = maximized;
        if self.width > 0 && self.should_draw {
            self.draw_decorations(world, self.width, self.height, self.scale);
        }
    }

    pub fn set_tiled(&mut self, tiled: bool) {
        self.tiled = tiled;
    }

    pub fn set_capabilities(&mut self, world: &World, capabilities: ToplevelCapabilities) {
        if self.capabilities == capabilities {
            return;
        }

        self.capabilities = capabilities;
        if self.width > 0 && self.should_draw {
            self.draw_decorations(world, self.width, self.height, self.scale);
        }
    }

    pub(crate) fn is_maximized(&self) -> bool {
        self.maximized
    }

    pub(crate) fn is_resizable(&self) -> bool {
        self.resizable
    }

    pub(crate) fn draws_titlebar(&self) -> bool {
        self.draw_titlebar
    }

    pub(crate) fn set_focused(
        &mut self,
        world: &World,
        _window: x::Window,
        focused: bool,
        _reason: &str,
    ) {
        if self.focused == focused {
            return;
        }

        let theme = decoration_theme();
        let now = Instant::now();
        let from = self.resolved_focus_progress(now, theme);
        let to = if focused { 1.0 } else { 0.0 };

        self.focus_progress = from;
        if (from - to).abs() <= f32::EPSILON {
            self.focus_progress = to;
            self.focus_transition = None;
        } else {
            self.focus_transition = Some(FocusTransition {
                from,
                to,
                started_at: now,
            });
        }

        self.buttons.apply_focus_state(theme, focused, now);
        self.focused = focused;
        if self.width > 0 && self.should_draw {
            self.draw_frame(world);
            self.draw_titlebar(world);
        }
    }

    pub(crate) fn set_resizable(&mut self, resizable: bool) {
        self.resizable = resizable;
    }

    pub(crate) fn set_resize_input_enabled(&mut self, enabled: bool) {
        if self.resize_input_enabled == enabled {
            return;
        }

        self.resize_input_enabled = enabled;
        self.update_input_region();
    }

    pub(crate) fn set_resize_interaction_active(&mut self, active: bool) {
        self.resize_interaction_active = active;
    }

    pub(crate) fn handle_animation_tick(&mut self, world: &World, _window: Option<x::Window>) {
        if self.width <= 0 || !self.should_draw {
            return;
        }

        let theme = decoration_theme();
        let now = Instant::now();
        let (focus_changed, focus_animating) = self.advance_focus_animation(now, theme);
        let (button_changed, button_animating) = self.buttons.advance_animations(theme, now);
        if focus_changed || focus_animating || button_changed || button_animating {
            self.draw_titlebar(world);
        }
    }

    pub(crate) fn handle_animation_frame(
        &mut self,
        world: &World,
        generation: u64,
        window: Option<x::Window>,
    ) {
        let animation_frame = self.pending_animation_frame == Some(generation);
        let resize_frame = self.pending_resize_frame == Some(generation);
        if !animation_frame && !resize_frame {
            return;
        }
        if animation_frame {
            self.pending_animation_frame = None;
        }
        if resize_frame {
            self.pending_resize_frame = None;
        }

        if resize_frame && self.resize_interaction_active && self.resize_redraw_pending {
            self.request_resize_frame();
            self.draw_resize_frame(world);
        }

        if animation_frame && self.has_active_animation() {
            self.handle_animation_tick(world, window);
        }
    }

    pub(crate) fn has_active_animation(&self) -> bool {
        self.focus_transition.is_some() || self.buttons.has_active_animation()
    }

    fn resolved_focus_progress(&self, now: Instant, theme: &DecorationTheme) -> f32 {
        let Some(transition) = self.focus_transition else {
            return self.focus_progress;
        };

        let (progress, _) = theme.unfocus_transition.progress(transition.started_at, now);
        transition.from + (transition.to - transition.from) * progress
    }

    fn advance_focus_animation(
        &mut self,
        now: Instant,
        theme: &DecorationTheme,
    ) -> (bool, bool) {
        let Some(transition) = self.focus_transition else {
            return (false, false);
        };

        let (progress, animating) = theme.unfocus_transition.progress(transition.started_at, now);
        let next = transition.from + (transition.to - transition.from) * progress;
        let changed = (next - self.focus_progress).abs() > f32::EPSILON;
        self.focus_progress = next;
        if !animating {
            self.focus_progress = transition.to;
            self.focus_transition = None;
        }
        (changed, animating)
    }

    fn handle_motion(
        &mut self,
        world: &World,
        x: f64,
        y: f64,
        resizable: bool,
    ) -> Option<xdg_toplevel::ResizeEdge> {
        let titlebar_offset = f64::from(self.frame_border());
        let titlebar_height = f64::from(self.titlebar_height());
        let titlebar_x = x - titlebar_offset;
        let titlebar_y = y - titlebar_offset;
        let in_titlebar = self.draw_titlebar
            && titlebar_x >= 0.0
            && titlebar_x <= f64::from(self.width)
            && titlebar_y >= 0.0
            && titlebar_y <= titlebar_height;
        let theme = decoration_theme();
        let now = Instant::now();
        let buttons_changed = if in_titlebar {
            self.buttons
                .check_hovered(titlebar_x as f32, titlebar_y as f32, self.capabilities, theme, now)
        } else {
            self.buttons.clear_hovered(theme, now)
        };
        let hovered_button = if in_titlebar {
            self.buttons.hovered_action(self.capabilities)
        } else {
            None
        };
        let hovered_kind = self.buttons.hovered_kind(self.capabilities);
        let active_kind = self.pressed_button.filter(|pressed| hovered_kind == Some(*pressed));
        let active_changed = self.buttons.set_active(active_kind, theme, now);
        let next_resize_edge = if hovered_button.is_none() && resizable {
            resize_edge_for_frame_rect(
                self.frame_width(),
                self.frame_height(),
                self.frame_border(),
                self.resize_border(),
                x,
                y,
                true,
                true,
            )
        } else {
            None
        };
        let resize_changed = self.hovered_resize_edge != next_resize_edge;
        self.hovered_resize_edge = next_resize_edge;

        if buttons_changed || active_changed {
            self.draw_titlebar(world);
        }

        if resize_changed {
            return self.hovered_resize_edge;
        }

        self.hovered_resize_edge
    }

    fn handle_leave(&mut self, world: &World) {
        let theme = decoration_theme();
        let now = Instant::now();
        let hover_changed = self.buttons.clear_hovered(theme, now);
        let active_changed = self.buttons.set_active(None, theme, now);
        self.hovered_resize_edge = None;
        self.pressed_button = None;
        if (hover_changed || active_changed) && self.width > 0 && self.should_draw {
            self.draw_titlebar(world);
        }
    }

    fn handle_press(&mut self, world: &World) -> Option<DecorationAction> {
        let theme = decoration_theme();
        let now = Instant::now();
        let hovered_kind = self.buttons.hovered_kind(self.capabilities);
        self.pressed_button = hovered_kind;
        if self.buttons.set_active(hovered_kind, theme, now)
            && self.width > 0
            && self.should_draw
        {
            self.draw_titlebar(world);
        }

        if let Some(edge) = self.hovered_resize_edge {
            return Some(DecorationAction::Resize(edge));
        }

        if hovered_kind.is_some() {
            None
        } else {
            Some(DecorationAction::Move)
        }
    }

    fn handle_release(&mut self, world: &World) -> Option<DecorationAction> {
        let theme = decoration_theme();
        let now = Instant::now();
        let hovered_kind = self.buttons.hovered_kind(self.capabilities);
        let pressed_kind = self.pressed_button.take();
        let action = if pressed_kind.is_some() && pressed_kind == hovered_kind {
            pressed_kind.map(|kind| match kind {
                DecorationButtonKind::Close => DecorationAction::Close,
                DecorationButtonKind::Maximize => DecorationAction::ToggleMaximized,
                DecorationButtonKind::Minimize => DecorationAction::Minimize,
            })
        } else {
            None
        };

        if self.buttons.set_active(None, theme, now)
            && self.width > 0
            && self.should_draw
        {
            self.draw_titlebar(world);
        }
        action
    }

    fn handle_click(&mut self, world: &World) -> Option<DecorationAction> {
        self.handle_press(world)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecorationButtonKind {
    Minimize,
    Maximize,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecorationAction {
    Move,
    Resize(xdg_toplevel::ResizeEdge),
    Minimize,
    ToggleMaximized,
    Close,
}

#[derive(Debug, Clone, Copy)]
struct DecorationsBox {
    rect: Rect,
    hovered: bool,
    active: bool,
    current_background: Color,
    transition: Option<ButtonTransition>,
    kind: DecorationButtonKind,
}

#[derive(Debug, Clone, Copy)]
struct ButtonTransition {
    from: Color,
    to: Color,
    started_at: Instant,
    spec: TransitionSpec,
}

#[derive(Debug, Clone, Copy)]
struct FocusTransition {
    from: f32,
    to: f32,
    started_at: Instant,
}

#[derive(Debug)]
struct DecorationsButtons {
    minimize: DecorationsBox,
    maximize: DecorationsBox,
    close: DecorationsBox,
}

impl Default for DecorationsButtons {
    fn default() -> Self {
        Self {
            minimize: DecorationsBox::new(DecorationButtonKind::Minimize),
            maximize: DecorationsBox::new(DecorationButtonKind::Maximize),
            close: DecorationsBox::new(DecorationButtonKind::Close),
        }
    }
}

impl DecorationsBox {
    fn new(kind: DecorationButtonKind) -> Self {
        Self {
            rect: Rect::from_xywh(0.0, 0.0, 0.0, 0.0).unwrap(),
            hovered: false,
            active: false,
            current_background: Color::TRANSPARENT,
            transition: None,
            kind,
        }
    }

    fn contains(&self, x: f32, y: f32) -> bool {
        (self.rect.left()..=self.rect.right()).contains(&x)
            && (self.rect.top()..=self.rect.bottom()).contains(&y)
    }

    fn target_background(&self, theme: &DecorationTheme) -> Color {
        theme.button_background_for_state(self.hovered, self.active)
    }

    fn resolved_background(&self, now: Instant, theme: &DecorationTheme) -> Color {
        if let Some(transition) = self.transition {
            let (progress, _) = transition.spec.progress(transition.started_at, now);
            transition.from.lerp_color(transition.to, progress)
        } else if self.current_background == Color::TRANSPARENT {
            self.target_background(theme)
        } else {
            self.current_background
        }
    }

    fn set_hovered(&mut self, hovered: bool, theme: &DecorationTheme, now: Instant) -> bool {
        if self.hovered == hovered {
            return false;
        }

        let from = self.resolved_background(now, theme);
        self.hovered = hovered;
        self.start_transition(
            from,
            self.target_background(theme),
            now,
            theme.button_hover_transition,
        )
    }

    fn set_active(&mut self, active: bool, theme: &DecorationTheme, now: Instant) -> bool {
        if self.active == active {
            return false;
        }

        let from = self.resolved_background(now, theme);
        self.active = active;
        self.start_transition(
            from,
            self.target_background(theme),
            now,
            theme.button_active_transition,
        )
    }

    fn start_transition(
        &mut self,
        from: Color,
        to: Color,
        now: Instant,
        spec: TransitionSpec,
    ) -> bool {
        let changed = self.current_background != to || self.transition.is_some();
        if from == to {
            self.current_background = to;
            self.transition = None;
            return changed;
        }

        self.current_background = from;
        self.transition = Some(ButtonTransition {
            from,
            to,
            started_at: now,
            spec,
        });
        changed
    }

    fn advance_animation(&mut self, now: Instant, theme: &DecorationTheme) -> (bool, bool) {
        let Some(transition) = self.transition else {
            let target = self.target_background(theme);
            if self.current_background != target {
                self.current_background = target;
                return (true, false);
            }
            return (false, false);
        };

        let (progress, animating) = transition.spec.progress(transition.started_at, now);
        let next = transition.from.lerp_color(transition.to, progress);
        let changed = next != self.current_background;
        self.current_background = next;
        if !animating {
            self.current_background = transition.to;
            self.transition = None;
        }
        (changed, animating)
    }

    fn apply_focus_state(&mut self, theme: &DecorationTheme, focused: bool, now: Instant) -> bool {
        if !focused {
            self.hovered = false;
            self.active = false;
        }

        let from = self.resolved_background(now, theme);
        let spec = if self.active {
            theme.button_active_transition
        } else {
            theme.button_hover_transition
        };
        self.start_transition(from, self.target_background(theme), now, spec)
    }

    fn has_active_animation(&self) -> bool {
        self.transition.is_some()
    }
}

impl DecorationsButtons {
    fn buttons_mut(&mut self) -> [&mut DecorationsBox; 3] {
        [&mut self.minimize, &mut self.maximize, &mut self.close]
    }

    fn total_width_pixels(&self, theme: &DecorationTheme, capabilities: ToplevelCapabilities) -> u32 {
        theme
            .button_total_width(self.visible_button_count(capabilities))
            .ceil() as u32
    }

    fn visible_button_count(&self, capabilities: ToplevelCapabilities) -> u32 {
        let mut count = 1;
        if capabilities.contains(ToplevelCapabilities::MAXIMIZE) {
            count += 1;
        }
        if capabilities.contains(ToplevelCapabilities::MINIMIZE) {
            count += 1;
        }
        count
    }

    fn for_each_visible_button(
        &self,
        capabilities: ToplevelCapabilities,
        mut visitor: impl FnMut(&DecorationsBox),
    ) {
        if capabilities.contains(ToplevelCapabilities::MINIMIZE) {
            visitor(&self.minimize);
        }
        if capabilities.contains(ToplevelCapabilities::MAXIMIZE) {
            visitor(&self.maximize);
        }
        visitor(&self.close);
    }

    fn layout(
        &mut self,
        width: f32,
        bar_height: f32,
        theme: &DecorationTheme,
        capabilities: ToplevelCapabilities,
    ) {
        let button_size = theme.button_size();
        let start =
            (width - theme.button_total_width(self.visible_button_count(capabilities))).max(0.0)
                + theme.controls_leading_margin;
        let mut x = start;
        let y = ((bar_height - button_size) / 2.0).max(0.0);

        self.minimize.rect = if capabilities.contains(ToplevelCapabilities::MINIMIZE) {
            let rect = Rect::from_xywh(x, y, button_size, button_size).unwrap();
            x += button_size + theme.controls_spacing;
            rect
        } else {
            Rect::from_xywh(0.0, 0.0, 0.0, 0.0).unwrap()
        };

        self.maximize.rect = if capabilities.contains(ToplevelCapabilities::MAXIMIZE) {
            let rect = Rect::from_xywh(x, y, button_size, button_size).unwrap();
            x += button_size + theme.controls_spacing;
            rect
        } else {
            Rect::from_xywh(0.0, 0.0, 0.0, 0.0).unwrap()
        };

        self.close.rect = Rect::from_xywh(x, y, button_size, button_size).unwrap();
    }

    fn check_hovered(
        &mut self,
        x: f32,
        y: f32,
        capabilities: ToplevelCapabilities,
        theme: &DecorationTheme,
        now: Instant,
    ) -> bool {
        let hovered = if self.close.contains(x, y) {
            Some(DecorationButtonKind::Close)
        } else if capabilities.contains(ToplevelCapabilities::MAXIMIZE)
            && self.maximize.contains(x, y)
        {
            Some(DecorationButtonKind::Maximize)
        } else if capabilities.contains(ToplevelCapabilities::MINIMIZE)
            && self.minimize.contains(x, y)
        {
            Some(DecorationButtonKind::Minimize)
        } else {
            None
        };

        let mut changed = false;
        for button in self.buttons_mut() {
            changed |= button.set_hovered(hovered == Some(button.kind), theme, now);
        }
        changed
    }

    fn clear_hovered(&mut self, theme: &DecorationTheme, now: Instant) -> bool {
        let mut changed = false;
        for button in self.buttons_mut() {
            changed |= button.set_hovered(false, theme, now);
        }
        changed
    }

    fn set_active(
        &mut self,
        active: Option<DecorationButtonKind>,
        theme: &DecorationTheme,
        now: Instant,
    ) -> bool {
        let mut changed = false;
        for button in self.buttons_mut() {
            changed |= button.set_active(active == Some(button.kind), theme, now);
        }
        changed
    }

    fn advance_animations(&mut self, theme: &DecorationTheme, now: Instant) -> (bool, bool) {
        let mut changed = false;
        let mut animating = false;
        for button in self.buttons_mut() {
            let (button_changed, button_animating) = button.advance_animation(now, theme);
            changed |= button_changed;
            animating |= button_animating;
        }
        (changed, animating)
    }

    fn apply_focus_state(&mut self, theme: &DecorationTheme, focused: bool, now: Instant) -> bool {
        let mut changed = false;
        for button in self.buttons_mut() {
            changed |= button.apply_focus_state(theme, focused, now);
        }
        changed
    }

    fn has_active_animation(&self) -> bool {
        self.minimize.has_active_animation()
            || self.maximize.has_active_animation()
            || self.close.has_active_animation()
    }

    fn hovered_action(&self, capabilities: ToplevelCapabilities) -> Option<DecorationAction> {
        if self.close.hovered {
            Some(DecorationAction::Close)
        } else if capabilities.contains(ToplevelCapabilities::MAXIMIZE) && self.maximize.hovered {
            Some(DecorationAction::ToggleMaximized)
        } else if capabilities.contains(ToplevelCapabilities::MINIMIZE) && self.minimize.hovered {
            Some(DecorationAction::Minimize)
        } else {
            None
        }
    }

    fn hovered_kind(&self, capabilities: ToplevelCapabilities) -> Option<DecorationButtonKind> {
        if self.close.hovered {
            Some(DecorationButtonKind::Close)
        } else if capabilities.contains(ToplevelCapabilities::MAXIMIZE) && self.maximize.hovered {
            Some(DecorationButtonKind::Maximize)
        } else if capabilities.contains(ToplevelCapabilities::MINIMIZE) && self.minimize.hovered {
            Some(DecorationButtonKind::Minimize)
        } else {
            None
        }
    }
}

pub(crate) fn draw_pixmap_to_buffer(pixmap: &Pixmap, buffer: &mut [u8]) {
    for (data, pixel) in buffer.chunks_exact_mut(4).zip(pixmap.pixels()) {
        data[0] = pixel.blue();
        data[1] = pixel.green();
        data[2] = pixel.red();
        data[3] = pixel.alpha();
    }
}

fn button_pixmap(
    kind: DecorationButtonKind,
    button_width: u32,
    button_height: u32,
    scale: f32,
    background: Color,
    foreground: Color,
    maximized: bool,
    theme: &DecorationTheme,
) -> Pixmap {
    let mut button = Pixmap::new(button_width.max(1), button_height.max(1)).unwrap();
    if background.alpha() > 0.0 {
        let visual_width = (theme.button_size() * scale)
            .min(button_width as f32)
            .max(1.0);
        let visual_height = (theme.button_size() * scale)
            .min(button_height as f32)
            .max(1.0);
        let radius = visual_width.min(visual_height) / 2.0;
        let cx = button_width as f32 / 2.0;
        let cy = button_height as f32 / 2.0;
        let path = PathBuilder::from_circle(cx, cy, radius).unwrap();
        let mut paint = Paint::default();
        paint.set_color(background);
        button.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
        );
    } else {
        button.fill(Color::TRANSPARENT);
    }

    let width = button.width() as f32;
    let height = button.height() as f32;
    let icon_size = theme
        .button_glyph_size
        * scale
        .min(width)
        .min(height)
        .max(1.0);
    let icon_origin = Point::from_xy(
        ((width - icon_size) / 2.0).max(0.0),
        ((height - icon_size) / 2.0).max(0.0),
    );

    match kind {
        DecorationButtonKind::Close => draw_close_icon(&mut button, icon_origin, icon_size, foreground),
        DecorationButtonKind::Minimize => {
            draw_minimize_icon(&mut button, icon_origin, icon_size, foreground)
        }
        DecorationButtonKind::Maximize if maximized => {
            draw_restore_icon(&mut button, icon_origin, icon_size, foreground)
        }
        DecorationButtonKind::Maximize => {
            draw_maximize_icon(&mut button, icon_origin, icon_size, foreground)
        }
    }

    button
}

fn fill_icon_rect(
    pixmap: &mut Pixmap,
    icon_origin: Point,
    icon_size: f32,
    rect: Rect,
    color: Color,
) {
    let unit = icon_size / 16.0;
    let mut paint = Paint::default();
    paint.set_color(color);
    pixmap.fill_rect(
        rect,
        &paint,
        Transform::from_scale(unit, unit).post_translate(icon_origin.x, icon_origin.y),
        None,
    );
}

fn fill_icon_rect_rotated(
    pixmap: &mut Pixmap,
    icon_origin: Point,
    icon_size: f32,
    rect: Rect,
    angle_deg: f32,
    color: Color,
) {
    let unit = icon_size / 16.0;
    let mut paint = Paint::default();
    paint.set_color(color);
    pixmap.fill_rect(
        rect,
        &paint,
        Transform::from_rotate_at(angle_deg, 8.0, 8.0)
            .post_scale(unit, unit)
            .post_translate(icon_origin.x, icon_origin.y),
        None,
    );
}

fn draw_minimize_icon(
    pixmap: &mut Pixmap,
    icon_origin: Point,
    icon_size: f32,
    color: Color,
) {
    fill_icon_rect(
        pixmap,
        icon_origin,
        icon_size,
        Rect::from_xywh(4.0, 10.0, 8.0, 1.0).unwrap(),
        color,
    );
}

fn draw_maximize_icon(
    pixmap: &mut Pixmap,
    icon_origin: Point,
    icon_size: f32,
    color: Color,
) {
    fill_icon_rect(
        pixmap,
        icon_origin,
        icon_size,
        Rect::from_xywh(4.0, 4.0, 8.0, 1.0).unwrap(),
        color,
    );
    fill_icon_rect(
        pixmap,
        icon_origin,
        icon_size,
        Rect::from_xywh(4.0, 5.0, 1.0, 7.0).unwrap(),
        color,
    );
    fill_icon_rect(
        pixmap,
        icon_origin,
        icon_size,
        Rect::from_xywh(11.0, 5.0, 1.0, 7.0).unwrap(),
        color,
    );
    fill_icon_rect(
        pixmap,
        icon_origin,
        icon_size,
        Rect::from_xywh(5.0, 11.0, 6.0, 1.0).unwrap(),
        color,
    );
}

fn draw_restore_icon(
    pixmap: &mut Pixmap,
    icon_origin: Point,
    icon_size: f32,
    color: Color,
) {
    let mut backdrop_color = color;
    backdrop_color.apply_opacity(0.5);

    fill_icon_rect(
        pixmap,
        icon_origin,
        icon_size,
        Rect::from_xywh(4.0, 6.0, 6.0, 1.0).unwrap(),
        color,
    );
    fill_icon_rect(
        pixmap,
        icon_origin,
        icon_size,
        Rect::from_xywh(4.0, 7.0, 1.0, 5.0).unwrap(),
        color,
    );
    fill_icon_rect(
        pixmap,
        icon_origin,
        icon_size,
        Rect::from_xywh(9.0, 7.0, 1.0, 5.0).unwrap(),
        color,
    );
    fill_icon_rect(
        pixmap,
        icon_origin,
        icon_size,
        Rect::from_xywh(5.0, 11.0, 4.0, 1.0).unwrap(),
        color,
    );
    fill_icon_rect(
        pixmap,
        icon_origin,
        icon_size,
        Rect::from_xywh(6.0, 4.0, 5.0, 1.0).unwrap(),
        backdrop_color,
    );
    fill_icon_rect(
        pixmap,
        icon_origin,
        icon_size,
        Rect::from_xywh(11.0, 5.0, 1.0, 5.0).unwrap(),
        backdrop_color,
    );
}

fn draw_close_icon(
    pixmap: &mut Pixmap,
    icon_origin: Point,
    icon_size: f32,
    color: Color,
) {
    let vertical = Rect::from_xywh(7.375625, 2.843070, 1.248750, 10.313153).unwrap();
    fill_icon_rect_rotated(pixmap, icon_origin, icon_size, vertical, 45.0, color);
    fill_icon_rect_rotated(pixmap, icon_origin, icon_size, vertical, -45.0, color);
}

fn fill_rounded_titlebar(pixmap: &mut Pixmap, rect: Rect, radius: f32, color: Color) {
    let mut paint = Paint::default();
    paint.set_color(color);

    let radius = radius.min(rect.width() / 2.0).min(rect.height()).max(0.0);
    if radius <= 0.0 {
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        return;
    }

    let center_rect = Rect::from_xywh(
        rect.left() + radius,
        rect.top(),
        (rect.width() - radius * 2.0).max(0.0),
        rect.height(),
    )
    .unwrap();
    pixmap.fill_rect(center_rect, &paint, Transform::identity(), None);

    let side_height = (rect.height() - radius).max(0.0);
    if side_height > 0.0 {
        let left_rect = Rect::from_xywh(rect.left(), rect.top() + radius, radius, side_height)
            .unwrap();
        pixmap.fill_rect(left_rect, &paint, Transform::identity(), None);

        let right_rect = Rect::from_xywh(
            rect.right() - radius,
            rect.top() + radius,
            radius,
            side_height,
        )
        .unwrap();
        pixmap.fill_rect(right_rect, &paint, Transform::identity(), None);
    }

    for (cx, cy) in [
        (rect.left() + radius, rect.top() + radius),
        (rect.right() - radius, rect.top() + radius),
    ] {
        let path = PathBuilder::from_circle(cx, cy, radius).unwrap();
        pixmap.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn draw_titlebar_top_highlight(
    pixmap: &mut Pixmap,
    rect: Rect,
    radius: f32,
    color: Color,
    height: f32,
) {
    if color.alpha() <= 0.0 || height <= 0.0 {
        return;
    }

    let highlight_height = height.ceil().max(1.0) as usize;
    let width = pixmap.width() as usize;
    let mask_height = pixmap.height() as usize;
    let mut mask = Pixmap::new(width as u32, mask_height as u32).unwrap();
    fill_rounded_titlebar(&mut mask, rect, radius, Color::from_rgba8(0, 0, 0, 255));

    let color = color.premultiply().to_color_u8();
    for y in 0..highlight_height.min(mask_height) {
        let row = y * width;
        for x in 0..width {
            let coverage = mask.pixels()[row + x].alpha() as u16;
            if coverage > 0 {
                composite_premultiplied_with_coverage(
                    &mut pixmap.pixels_mut()[row + x],
                    color,
                    coverage,
                );
            }
        }
    }
}

fn scaled_rect(x: i32, y: i32, width: i32, height: i32, scale: f32) -> Option<Rect> {
    if width <= 0 || height <= 0 || scale <= 0.0 {
        return None;
    }

    let scale = scale.max(1.0);
    let left = (x as f32 * scale).floor();
    let top = (y as f32 * scale).floor();
    let right = ((x + width) as f32 * scale).ceil();
    let bottom = ((y + height) as f32 * scale).ceil();
    Rect::from_xywh(left, top, (right - left).max(1.0), (bottom - top).max(1.0))
}

fn inset_positive_edges(rect: Rect, amount: f32) -> Option<Rect> {
    if amount <= 0.0 {
        return Some(rect);
    }

    Rect::from_xywh(
        rect.left(),
        rect.top(),
        (rect.width() - amount).max(1.0),
        (rect.height() - amount).max(1.0),
    )
}

#[derive(Debug, Clone, Copy)]
enum RingPlacement {
    Outside,
    Inside,
}

fn draw_window_shadow(
    pixmap: &mut Pixmap,
    rect: Rect,
    radius: f32,
    shadow: WindowShadow,
    scale: f32,
) {
    if shadow.color.alpha() <= 0.0 {
        return;
    }

    let width = pixmap.width() as usize;
    let height = pixmap.height() as usize;
    if width == 0 || height == 0 {
        return;
    }

    let scale = scale.max(1.0);
    let spread = (shadow.spread * scale).ceil().max(0.0) as i32;
    let blur = (shadow.blur * scale).ceil().max(1.0) as i32;
    let offset_x = shadow.offset_x * scale;
    let offset_y = shadow.offset_y * scale;

    let Some(shadow_rect) = offset_rect(expand_rect(rect, spread as f32), offset_x, offset_y) else {
        return;
    };
    let mut mask = Pixmap::new(width as u32, height as u32).unwrap();
    fill_window_shape(
        &mut mask,
        shadow_rect,
        radius + spread as f32,
        Color::from_rgba8(0, 0, 0, 255),
    );

    let mut alpha = mask
        .pixels()
        .iter()
        .map(|pixel| pixel.alpha() as u16)
        .collect::<Vec<_>>();
    let blur_radius = ((blur as f32) / WINDOW_SHADOW_BLUR_PASSES as f32)
        .ceil()
        .max(1.0) as usize;
    let mut scratch = vec![0; alpha.len()];
    for _ in 0..WINDOW_SHADOW_BLUR_PASSES {
        box_blur_alpha_horizontal(&alpha, &mut scratch, width, height, blur_radius);
        box_blur_alpha_vertical(&scratch, &mut alpha, width, height, blur_radius);
    }

    composite_shadow_alpha(pixmap, &alpha, shadow.color);
}

fn draw_window_ring(
    pixmap: &mut Pixmap,
    rect: Rect,
    radius: f32,
    color: Color,
    width: f32,
    placement: RingPlacement,
) {
    if color.alpha() <= 0.0 || width <= 0.0 {
        return;
    }

    match placement {
        RingPlacement::Outside => {
            let Some(outer_rect) = expand_rect(rect, width) else {
                return;
            };
            fill_window_ring(
                pixmap,
                outer_rect,
                radius + width,
                rect,
                radius,
                color,
            );
        }
        RingPlacement::Inside => {
            let Some(inner_rect) = shrink_rect(rect, width) else {
                return;
            };
            fill_window_ring(
                pixmap,
                rect,
                radius,
                inner_rect,
                (radius - width).max(0.0),
                color,
            );
        }
    }
}

fn fill_window_ring(
    pixmap: &mut Pixmap,
    outer_rect: Rect,
    outer_radius: f32,
    inner_rect: Rect,
    inner_radius: f32,
    color: Color,
) {
    let mut builder = PathBuilder::new();
    push_window_outline(&mut builder, outer_rect, outer_radius);
    push_window_outline(&mut builder, inner_rect, inner_radius);
    let Some(path) = builder.finish() else {
        return;
    };

    let mut paint = Paint::default();
    paint.set_color(color);
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::EvenOdd,
        Transform::identity(),
        None,
    );
}

fn fill_window_shape(pixmap: &mut Pixmap, rect: Rect, radius: f32, color: Color) {
    let Some(path) = window_outline_path(rect, radius) else {
        return;
    };

    let mut paint = Paint::default();
    paint.set_color(color);
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

fn clear_window_shape(pixmap: &mut Pixmap, rect: Rect, radius: f32) {
    let Some(path) = window_outline_path(rect, radius) else {
        return;
    };

    let mut paint = Paint::default();
    paint.blend_mode = BlendMode::Clear;
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

fn box_blur_alpha_horizontal(
    src: &[u16],
    dst: &mut [u16],
    width: usize,
    height: usize,
    radius: usize,
) {
    if radius == 0 {
        dst.copy_from_slice(src);
        return;
    }

    let divisor = (radius * 2 + 1) as u32;
    for y in 0..height {
        let row = y * width;
        let mut sum = 0_u32;
        for x in 0..=radius.min(width.saturating_sub(1)) {
            sum += src[row + x] as u32;
        }

        for x in 0..width {
            dst[row + x] = (sum / divisor) as u16;
            if x >= radius {
                sum = sum.saturating_sub(src[row + x - radius] as u32);
            }
            let add_x = x + radius + 1;
            if add_x < width {
                sum += src[row + add_x] as u32;
            }
        }
    }
}

fn box_blur_alpha_vertical(
    src: &[u16],
    dst: &mut [u16],
    width: usize,
    height: usize,
    radius: usize,
) {
    if radius == 0 {
        dst.copy_from_slice(src);
        return;
    }

    let divisor = (radius * 2 + 1) as u32;
    for x in 0..width {
        let mut sum = 0_u32;
        for y in 0..=radius.min(height.saturating_sub(1)) {
            sum += src[y * width + x] as u32;
        }

        for y in 0..height {
            let index = y * width + x;
            dst[index] = (sum / divisor) as u16;
            if y >= radius {
                sum = sum.saturating_sub(src[(y - radius) * width + x] as u32);
            }
            let add_y = y + radius + 1;
            if add_y < height {
                sum += src[add_y * width + x] as u32;
            }
        }
    }
}

fn composite_shadow_alpha(pixmap: &mut Pixmap, alpha: &[u16], color: Color) {
    let color = color.premultiply().to_color_u8();
    if color.alpha() == 0 {
        return;
    }

    for (pixel, mask_alpha) in pixmap.pixels_mut().iter_mut().zip(alpha) {
        if *mask_alpha == 0 {
            continue;
        }

        composite_premultiplied_with_coverage(pixel, color, *mask_alpha);
    }
}

fn composite_premultiplied_with_coverage(
    pixel: &mut PremultipliedColorU8,
    color: PremultipliedColorU8,
    coverage: u16,
) {
    let coverage = u32::from(coverage);
    let src_alpha = (u32::from(color.alpha()) * coverage + 127) / 255;
    if src_alpha == 0 {
        return;
    }

    let src_red = (u32::from(color.red()) * coverage + 127) / 255;
    let src_green = (u32::from(color.green()) * coverage + 127) / 255;
    let src_blue = (u32::from(color.blue()) * coverage + 127) / 255;
    let inv_alpha = 255 - src_alpha;
    let dst_red = src_red + (u32::from(pixel.red()) * inv_alpha + 127) / 255;
    let dst_green = src_green + (u32::from(pixel.green()) * inv_alpha + 127) / 255;
    let dst_blue = src_blue + (u32::from(pixel.blue()) * inv_alpha + 127) / 255;
    let dst_alpha = src_alpha + (u32::from(pixel.alpha()) * inv_alpha + 127) / 255;

    *pixel = PremultipliedColorU8::from_rgba(
        dst_red.min(255) as u8,
        dst_green.min(255) as u8,
        dst_blue.min(255) as u8,
        dst_alpha.min(255) as u8,
    )
    .unwrap_or(PremultipliedColorU8::TRANSPARENT);
}

fn expand_rect(rect: Rect, amount: f32) -> Option<Rect> {
    Rect::from_xywh(
        rect.left() - amount,
        rect.top() - amount,
        rect.width() + amount * 2.0,
        rect.height() + amount * 2.0,
    )
}

fn shrink_rect(rect: Rect, amount: f32) -> Option<Rect> {
    Rect::from_xywh(
        rect.left() + amount,
        rect.top() + amount,
        (rect.width() - amount * 2.0).max(0.0),
        (rect.height() - amount * 2.0).max(0.0),
    )
}

fn offset_rect(rect: Option<Rect>, x: f32, y: f32) -> Option<Rect> {
    let rect = rect?;
    Rect::from_xywh(rect.left() + x, rect.top() + y, rect.width(), rect.height())
}

fn window_outline_path(rect: Rect, radius: f32) -> Option<tiny_skia::Path> {
    let mut path = PathBuilder::new();
    push_window_outline(&mut path, rect, radius);
    path.finish()
}

fn push_window_outline(path: &mut PathBuilder, rect: Rect, radius: f32) {
    let left = rect.left();
    let top = rect.top();
    let right = rect.right();
    let bottom = rect.bottom();
    let radius = radius
        .min(rect.width() / 2.0)
        .min(rect.height() / 2.0)
        .max(0.0);

    if radius <= 0.0 {
        path.move_to(left, top);
        path.line_to(right, top);
        path.line_to(right, bottom);
        path.line_to(left, bottom);
        path.close();
        return;
    }

    const KAPPA: f32 = 0.552_284_8;
    let control = radius * KAPPA;

    path.move_to(left, bottom);
    path.line_to(left, top + radius);
    path.cubic_to(
        left,
        top + radius - control,
        left + radius - control,
        top,
        left + radius,
        top,
    );
    path.line_to(right - radius, top);
    path.cubic_to(
        right - radius + control,
        top,
        right,
        top + radius - control,
        right,
        top + radius,
    );
    path.line_to(right, bottom);
    path.line_to(left, bottom);
    path.close();
}

fn title_pixmap(
    title: &str,
    max_width: u32,
    height: u32,
    scale: f32,
    theme: &DecorationTheme,
    color: Color,
) -> Option<Pixmap> {
    if title.is_empty() {
        return None;
    }

    let padding_left = (theme.title_padding.left * scale).round().max(0.0) as u32;
    let padding_right = (theme.title_padding.right * scale).round().max(0.0) as u32;
    let glyph_max_width = max_width.saturating_sub(padding_left + padding_right);
    if glyph_max_width == 0 {
        return None;
    }

    let (glyphs, font) = layout_title_glyphs(title, glyph_max_width, height, scale);

    let text_width = glyphs
        .last()
        .map(|glyph| glyph.position.x + font.h_advance(glyph.id))?
        .ceil() as u32;
    let width = (text_width + padding_left + padding_right).min(max_width).max(1);

    let mut pixmap = Pixmap::new(width, height).unwrap();
    let data = pixmap.pixels_mut();

    for glyph in glyphs {
        if let Some(outline) = font.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            let mut draw_outline = |x: u32, y: u32, coverage: f32, x_offset: i32| {
                let px = bounds.min.x as i32 + x as i32 + x_offset + padding_left as i32;
                let py = bounds.min.y as i32 + y as i32;
                if px < 0 || py < 0 || px as u32 >= width || py as u32 >= height {
                    return;
                }

                let pixel_idx = (px as u32 + py as u32 * width) as usize;
                let next = color.with_coverage(coverage);
                let alpha = data[pixel_idx].alpha().saturating_add(next.alpha());
                let color = color.to_color_u8();
                data[pixel_idx] = ColorU8::from_rgba(
                    color.red(),
                    color.green(),
                    color.blue(),
                    alpha,
                )
                .premultiply();
            };

            outline.draw(|x, y, coverage| {
                draw_outline(x, y, coverage, 0);
            });
        }
    }

    Some(pixmap)
}

static FONT_DATA: LazyLock<Cow<'static, [u8]>> = LazyLock::new(|| {
    #[cfg(feature = "fontconfig")]
    {
        let fc = fontconfig::Fontconfig::new().expect("Failed to initialize fontconfig.");
        let font = fc
            .find("Open Sans", Some("Bold"))
            .or_else(|| {
                warn!(
                    "Failed to locate Open Sans Bold via fontconfig, falling back to the default Open Sans match"
                );
                fc.find("Open Sans", None)
            })
            .expect("Failed to load Open Sans from fontconfig.");
        let data = std::fs::read(&font.path).expect("Failed to read font from disk.");
        Cow::Owned(data)
    }
    #[cfg(not(feature = "fontconfig"))]
    {
        Cow::Borrowed(include_bytes!("../../OpenSans-Bold.ttf"))
    }
});

static FONT: LazyLock<FontRef<'_>> =
    LazyLock::new(|| FontRef::try_from_slice(FONT_DATA.as_ref()).unwrap());

fn layout_title_glyphs(
    text: &str,
    max_width: u32,
    height: u32,
    scale: f32,
) -> (Vec<Glyph>, PxScaleFont<&'static FontRef<'static>>) {
    const TEXT_SIZE: f32 = 10.0;

    let mut glyphs = Vec::<Glyph>::new();

    let px_scale = FONT.pt_to_px_scale(TEXT_SIZE * scale).unwrap();
    let font = FONT.as_scaled(px_scale);
    for character in text.chars() {
        let mut glyph = font.scaled_glyph(character);
        glyph.position.y = (height as f32 / 2.0) - font.descent();
        if let Some(previous) = glyphs.last() {
            glyph.position.x = previous.position.x
                + font.h_advance(previous.id)
                + font.kern(previous.id, glyph.id);
        } else {
            glyph.position.x = 0.0;
        }
        if (glyph.position.x + font.h_advance(glyph.id)).ceil() as u32 > max_width {
            break;
        }

        glyphs.push(glyph);
    }

    (glyphs, font)
}

fn get_decoration(
    world: &World,
    parent: Entity,
) -> Option<hecs::RefMut<'_, Box<DecorationsDataSatellite>>> {
    let role = world.get::<&mut SurfaceRole>(parent).ok()?;
    Some(hecs::RefMut::map(role, |role| {
        let SurfaceRole::Toplevel(Some(toplevel)) = &mut *role else {
            unreachable!()
        };

        toplevel.decoration.satellite.as_mut().unwrap()
    }))
}

pub(crate) fn toplevel_has_resize_border(
    window: &WindowData,
    fullscreen: bool,
    maximized: bool,
    tiled: bool,
) -> bool {
    if fullscreen || maximized || tiled {
        return false;
    }

    if let Some(decorations) = window.attrs.decorations {
        if !decorations.intersects(
            crate::xstate::Decorations::Resizeh | crate::xstate::Decorations::All,
        ) {
            return false;
        }
    }

    !matches!(
        window.attrs.size_hints,
        Some(crate::xstate::WmNormalHints {
            min_size: Some(min_size),
            max_size: Some(max_size),
        }) if min_size == max_size
    )
}

pub(crate) fn toplevel_can_resize(
    window: &WindowData,
    fullscreen: bool,
    maximized: bool,
    tiled: bool,
) -> bool {
    toplevel_has_resize_border(window, fullscreen, maximized, tiled)
        && !matches!(
            window.configure_interaction,
            Some(crate::server::ConfigureInteraction::Move { .. })
        )
}

fn parent_resize_state(world: &World, parent: Entity) -> (bool, bool) {
    let Ok(window) = world.get::<&WindowData>(parent) else {
        return (false, false);
    };
    let Ok(role) = world.get::<&SurfaceRole>(parent) else {
        return (false, false);
    };
    let SurfaceRole::Toplevel(Some(toplevel)) = &*role else {
        return (false, false);
    };

    let has_resize_border = toplevel_has_resize_border(
        &window,
        toplevel.fullscreen,
        toplevel.maximized,
        toplevel.tiled,
    );
    let can_resize = has_resize_border
        && !matches!(
            window.configure_interaction,
            Some(crate::server::ConfigureInteraction::Move { .. })
        );

    (has_resize_border, can_resize)
}

pub fn handle_pointer_motion(
    state: &InnerServerState<impl X11Selection>,
    parent: Entity,
    part: DecorationPart,
    surface_x: f64,
    surface_y: f64,
) -> Option<xdg_toplevel::ResizeEdge> {
    let (has_resize_border, can_resize) = parent_resize_state(&state.world, parent);

    if let Some(mut decoration) = get_decoration(&state.world, parent) {
        decoration.set_resizable(has_resize_border);
        decoration.set_resize_input_enabled(can_resize);
        let (frame_x, frame_y) =
            decoration.surface_point_to_frame_point(part, surface_x, surface_y);
        return decoration.handle_motion(&state.world, frame_x, frame_y, can_resize);
    }

    None
}

pub fn handle_pointer_leave(world: &World, parent: Entity) {
    if let Some(mut decoration) = get_decoration(world, parent) {
        decoration.handle_leave(world);
    }
}

pub fn handle_pointer_click(
    state: &mut ServerState<impl XConnection>,
    parent: Entity,
    seat: &WlSeat,
    serial: u32,
) {
    let Ok(mut role) = state.world.get::<&mut SurfaceRole>(parent) else {
        return;
    };
    let SurfaceRole::Toplevel(Some(toplevel)) = &mut *role else {
        unreachable!();
    };

    let action = toplevel
        .decoration
        .satellite
        .as_mut()
        .unwrap()
        .handle_click(&state.world);
    let window = *state.world.get::<&x::Window>(parent).unwrap();
    let toplevel = toplevel.toplevel.clone();
    drop(role);

    match action {
        Some(DecorationAction::Resize(edge)) => state.resize_window_by_edge(window, edge),
        Some(DecorationAction::Move) => {
            state.begin_wayland_move_interaction(window, serial);
            toplevel._move(seat, serial)
        }
        Some(DecorationAction::Close)
        | Some(DecorationAction::ToggleMaximized)
        | Some(DecorationAction::Minimize)
        | None => {}
    }
}

pub fn handle_pointer_release(state: &mut ServerState<impl XConnection>, parent: Entity) {
    let Ok(mut role) = state.world.get::<&mut SurfaceRole>(parent) else {
        return;
    };
    let SurfaceRole::Toplevel(Some(toplevel)) = &mut *role else {
        return;
    };

    let action = toplevel
        .decoration
        .satellite
        .as_mut()
        .unwrap()
        .handle_release(&state.world);
    let window = *state.world.get::<&x::Window>(parent).unwrap();
    drop(role);

    match action {
        Some(DecorationAction::Close) => state.close_x_window(window),
        Some(DecorationAction::ToggleMaximized) => {
            state.set_maximized(window, crate::xstate::SetState::Toggle)
        }
        Some(DecorationAction::Minimize) => {
            state.set_minimized(window);
            state.connection.set_minimized(window, true);
        }
        Some(DecorationAction::Move) | Some(DecorationAction::Resize(_)) | None => {}
    }
}
