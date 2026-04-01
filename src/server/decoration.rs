use crate::server::{InnerServerState, ServerState, SurfaceRole};
use crate::{ToplevelCapabilities, X11Selection, XConnection};

use ab_glyph::{Font, FontRef, Glyph, PxScaleFont, ScaleFont};
use hecs::{CommandBuffer, Entity, World};
use log::{error, warn};
use smithay_client_toolkit::registry::SimpleGlobal;
use smithay_client_toolkit::shm::slot::SlotPool;
use std::borrow::Cow;
use std::sync::LazyLock;
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Stroke, Transform};
use tiny_skia::{ColorU8, Rect};
use wayland_client::Proxy;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1;
use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use xcb::x;

#[derive(Debug)]
pub struct DecorationsData {
    pub wl: Option<ZxdgToplevelDecorationV1>,
    // Boxed to avoid making ToplevelData so much bigger than PopupData
    pub satellite: Option<Box<DecorationsDataSatellite>>,
}

pub struct DecorationMarker {
    pub parent: Entity,
}

#[derive(Debug)]
pub struct DecorationsDataSatellite {
    surface: WlSurface,
    subsurface: WlSubsurface,
    pool: Entity,
    viewport: WpViewport,
    scale: f32,
    pixmap: Pixmap,
    buttons: DecorationsButtons,
    title: Option<String>,
    title_rect: Rect,
    width: i32,
    maximized: bool,
    capabilities: ToplevelCapabilities,
    should_draw: bool,
    remove_buffer: bool,
}

impl Drop for DecorationsDataSatellite {
    fn drop(&mut self) {
        self.subsurface.destroy();
        self.surface.destroy();
        self.viewport.destroy();
    }
}

impl DecorationsDataSatellite {
    pub const TITLEBAR_HEIGHT: i32 = 25;

    pub fn try_new(
        state: &InnerServerState<impl X11Selection>,
        parent: &WlSurface,
        title: Option<&str>,
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

        let surface = state.compositor.create_surface(
            &state.qh,
            DecorationMarker {
                parent: parent.data().copied().unwrap(),
            },
        );
        let subsurface = {
            state
                .subcompositor
                .get_subsurface(&surface, parent, &state.qh, ())
        };
        subsurface.set_position(0, -Self::TITLEBAR_HEIGHT);
        let viewport = state.viewporter.get_viewport(&surface, &state.qh, ());

        Some((
            Self {
                surface,
                subsurface,
                pool: pool_entity,
                viewport,
                buttons: DecorationsButtons::default(),
                pixmap: Pixmap::new(1, 1).unwrap(),
                scale: 1.0,
                title: title.map(str::to_string),
                title_rect: Rect::from_ltrb(0.0, 0.0, 0.0, 0.0).unwrap(),
                width: 0,
                maximized: false,
                capabilities: ToplevelCapabilities::all(),
                should_draw: true,
                remove_buffer: false,
            }
            .into(),
            new_pool.map(|p| {
                let mut buf = CommandBuffer::new();
                buf.insert_one(pool_entity, p);
                buf
            }),
        ))
    }

    fn pool<'a>(&self, world: &'a World) -> hecs::RefMut<'a, SlotPool> {
        world.get::<&mut SlotPool>(self.pool).unwrap()
    }

    fn update_buffer(&mut self, world: &World) {
        let mut pool = self.pool(world);
        let (buffer, data) = match pool.create_buffer(
            self.pixmap.width() as i32,
            self.pixmap.height() as i32,
            self.pixmap.width() as i32 * 4,
            wl_shm::Format::Xrgb8888,
        ) {
            Ok(b) => b,
            Err(err) => {
                error!("Failed to create buffer for decorations: {err:?}");
                return;
            }
        };

        draw_pixmap_to_buffer(&self.pixmap, data);
        buffer.attach_to(&self.surface).unwrap();
        self.surface.commit();
    }

    #[must_use]
    pub fn will_draw_decorations(&self, width: i32) -> bool {
        width > 0 && self.should_draw
    }

    pub fn draw_decorations(&mut self, world: &World, width: i32, parent_scale_factor: f32) {
        if !self.will_draw_decorations(width) {
            if self.remove_buffer {
                self.surface.attach(None, 0, 0);
                self.surface.commit();
                self.remove_buffer = false;
            }
            return;
        }

        self.width = width;
        self.scale = parent_scale_factor;
        let mut drawn_width = (width as f32 * self.scale).ceil() as i32;
        let drawn_height = (Self::TITLEBAR_HEIGHT as f32 * self.scale).ceil() as i32;
        let buttons_width = self
            .buttons
            .total_width_pixels(drawn_height as u32, self.capabilities) as i32;

        if buttons_width > drawn_width {
            drawn_width = buttons_width;
        }

        let title = self.title.as_ref().and_then(|t| {
            let width = (drawn_width as u32).saturating_sub(buttons_width as u32);
            if width > 0 {
                title_pixmap(t, width, drawn_height as u32, self.scale)
            } else {
                None
            }
        });

        // Draw the bar and its components
        let mut bar = Pixmap::new(drawn_width as u32, drawn_height as u32).unwrap();
        bar.fill(Color::WHITE);

        if let Some(title) = title {
            bar.draw_pixmap(
                0,
                0,
                title.as_ref(),
                &Default::default(),
                Transform::identity(),
                None,
            );
            self.title_rect =
                Rect::from_xywh(0.0, 0.0, title.width() as f32, title.height() as f32).unwrap();
        }

        self.buttons
            .layout(width as f32, Self::TITLEBAR_HEIGHT as f32, self.capabilities);

        for button in self.buttons.visible_buttons(self.capabilities) {
            let pixmap = button_pixmap(
                button.kind,
                drawn_height as u32,
                self.scale,
                button.hovered,
                self.maximized,
            );
            bar.draw_pixmap(
                (button.rect.left() * self.scale).round() as i32,
                0,
                pixmap.as_ref(),
                &Default::default(),
                Transform::identity(),
                None,
            );
        }

        self.pixmap = bar;
        self.viewport.set_destination(width, Self::TITLEBAR_HEIGHT);
        self.update_buffer(world);
    }

    pub fn set_title(&mut self, world: &World, title: &str) {
        self.title = Some(title.to_string());
        if !self.should_draw {
            return;
        }

        // Don't draw title if there's not enough space
        let title_pixmap = title_pixmap(
            title,
            self.pixmap
                .width()
                .saturating_sub(
                    self.buttons
                        .total_width_pixels(self.pixmap.height(), self.capabilities),
                ),
            self.pixmap.height(),
            self.scale,
        );

        let new_title_rect = title_pixmap
            .as_ref()
            .map(|p| Rect::from_xywh(0.0, 0.0, p.width() as f32, p.height() as f32).unwrap())
            .unwrap_or_else(|| Rect::from_ltrb(0.0, 0.0, 0.0, 0.0).unwrap());

        let last_title_rect = std::mem::replace(&mut self.title_rect, new_title_rect);

        // Clear last title with white
        let mut paint = Paint::default();
        paint.set_color(Color::WHITE);
        self.pixmap
            .fill_rect(last_title_rect, &paint, Transform::identity(), None);

        if let Some(p) = title_pixmap.as_ref() {
            self.pixmap.draw_pixmap(
                0,
                0,
                p.as_ref(),
                &Default::default(),
                Transform::identity(),
                None,
            );
        }

        let damaged_width = last_title_rect
            .width()
            .max(title_pixmap.map(|p| p.width() as f32).unwrap_or(0.0));

        self.surface
            .damage_buffer(0, 0, damaged_width as i32, last_title_rect.height() as i32);
        self.update_buffer(world);
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
            self.draw_decorations(world, self.width, self.scale);
        }
    }

    pub fn set_capabilities(&mut self, world: &World, capabilities: ToplevelCapabilities) {
        if self.capabilities == capabilities {
            return;
        }

        self.capabilities = capabilities;
        if self.width > 0 && self.should_draw {
            self.draw_decorations(world, self.width, self.scale);
        }
    }

    fn handle_motion(&mut self, world: &World, x: f64, y: f64) {
        if self
            .buttons
            .check_hovered(x as f32, y as f32, self.capabilities)
        {
            self.draw_decorations(world, self.width, self.scale);
        }
    }

    fn handle_leave(&mut self, world: &World) {
        if self.buttons.clear_hovered() {
            self.draw_decorations(world, self.width, self.scale);
        }
    }

    fn handle_click(
        &self,
        toplevel: &XdgToplevel,
        seat: &WlSeat,
        serial: u32,
    ) -> DecorationAction {
        match self
            .buttons
            .hovered_action(self.maximized, self.capabilities)
        {
            Some(DecorationAction::Close) => DecorationAction::Close,
            Some(DecorationAction::ToggleMaximized) => DecorationAction::ToggleMaximized,
            Some(DecorationAction::Minimize) => DecorationAction::Minimize,
            _ => {
                toplevel._move(seat, serial);
                DecorationAction::Move
            }
        }
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
    Minimize,
    ToggleMaximized,
    Close,
}

#[derive(Debug, Clone, Copy)]
struct DecorationsBox {
    rect: Rect,
    hovered: bool,
    kind: DecorationButtonKind,
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
            kind,
        }
    }

    fn contains(&self, x: f32, y: f32) -> bool {
        (self.rect.left()..=self.rect.right()).contains(&x)
            && (self.rect.top()..=self.rect.bottom()).contains(&y)
    }
}

impl DecorationsButtons {
    fn total_width_pixels(&self, button_height: u32, capabilities: ToplevelCapabilities) -> u32 {
        button_height.saturating_mul(self.visible_button_count(capabilities))
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

    fn visible_buttons(&self, capabilities: ToplevelCapabilities) -> Vec<&DecorationsBox> {
        let mut buttons = Vec::with_capacity(self.visible_button_count(capabilities) as usize);
        if capabilities.contains(ToplevelCapabilities::MINIMIZE) {
            buttons.push(&self.minimize);
        }
        if capabilities.contains(ToplevelCapabilities::MAXIMIZE) {
            buttons.push(&self.maximize);
        }
        buttons.push(&self.close);
        buttons
    }

    fn layout(&mut self, width: f32, button_width: f32, capabilities: ToplevelCapabilities) {
        let count = self.visible_button_count(capabilities) as f32;
        let start = (width - button_width * count).max(0.0);
        let mut index = 0.0;

        self.minimize.rect = if capabilities.contains(ToplevelCapabilities::MINIMIZE) {
            let rect =
                Rect::from_xywh(start + button_width * index, 0.0, button_width, button_width)
                    .unwrap();
            index += 1.0;
            rect
        } else {
            Rect::from_xywh(0.0, 0.0, 0.0, 0.0).unwrap()
        };

        self.maximize.rect = if capabilities.contains(ToplevelCapabilities::MAXIMIZE) {
            let rect =
                Rect::from_xywh(start + button_width * index, 0.0, button_width, button_width)
                    .unwrap();
            index += 1.0;
            rect
        } else {
            Rect::from_xywh(0.0, 0.0, 0.0, 0.0).unwrap()
        };

        self.close.rect =
            Rect::from_xywh(start + button_width * index, 0.0, button_width, button_width)
                .unwrap();
    }

    fn check_hovered(&mut self, x: f32, y: f32, capabilities: ToplevelCapabilities) -> bool {
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
        for button in [&mut self.minimize, &mut self.maximize, &mut self.close] {
            let next = hovered == Some(button.kind);
            if button.hovered != next {
                button.hovered = next;
                changed = true;
            }
        }
        changed
    }

    fn clear_hovered(&mut self) -> bool {
        let mut changed = false;
        for button in [&mut self.minimize, &mut self.maximize, &mut self.close] {
            if button.hovered {
                button.hovered = false;
                changed = true;
            }
        }
        changed
    }

    fn hovered_action(
        &self,
        _maximized: bool,
        capabilities: ToplevelCapabilities,
    ) -> Option<DecorationAction> {
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
}

fn draw_pixmap_to_buffer(pixmap: &Pixmap, buffer: &mut [u8]) {
    // TODO: support big endian?
    for (data, pixel) in buffer.chunks_exact_mut(4).zip(pixmap.pixels()) {
        data[0] = pixel.blue();
        data[1] = pixel.green();
        data[2] = pixel.red();
        data[3] = pixel.alpha();
    }
}

fn button_pixmap(
    kind: DecorationButtonKind,
    bar_height: u32,
    scale: f32,
    hovered: bool,
    maximized: bool,
) -> Pixmap {
    let mut button = Pixmap::new(bar_height, bar_height).unwrap();
    if hovered {
        let bg = match kind {
            DecorationButtonKind::Close => Color::from_rgba(1.0, 0.0, 0.0, 0.8).unwrap(),
            _ => Color::from_rgba(0.85, 0.85, 0.85, 1.0).unwrap(),
        };
        button.fill(bg);
    } else {
        button.fill(Color::WHITE);
    }
    let size = button.width() as f32;
    let margin = 8.4 * scale;
    let mut paint = Paint::default();
    paint.set_color(Color::BLACK);

    match kind {
        DecorationButtonKind::Close => {
            let mut line = PathBuilder::new();
            line.move_to(margin, margin);
            line.line_to(size - margin, size - margin);
            line.move_to(size - margin, margin);
            line.line_to(margin, size - margin);
            let line = line.finish().unwrap();
            button.stroke_path(
                &line,
                &paint,
                &Stroke {
                    width: scale + 0.5,
                    ..Default::default()
                },
                Default::default(),
                None,
            );
        }
        DecorationButtonKind::Minimize => {
            let y = size - margin * 1.25;
            let mut line = PathBuilder::new();
            line.move_to(margin, y);
            line.line_to(size - margin, y);
            let line = line.finish().unwrap();
            button.stroke_path(
                &line,
                &paint,
                &Stroke {
                    width: scale + 0.75,
                    ..Default::default()
                },
                Default::default(),
                None,
            );
        }
        DecorationButtonKind::Maximize => {
            let inset = margin + scale;
            let mut path = PathBuilder::new();
            if maximized {
                path.move_to(inset + 3.0 * scale, inset);
                path.line_to(size - inset, inset);
                path.line_to(size - inset, size - inset - 3.0 * scale);
                path.line_to(inset + 3.0 * scale, size - inset - 3.0 * scale);
                path.close();
                path.move_to(inset, inset + 3.0 * scale);
                path.line_to(size - inset - 3.0 * scale, inset + 3.0 * scale);
                path.line_to(size - inset - 3.0 * scale, size - inset);
                path.line_to(inset, size - inset);
                path.close();
            } else {
                path.move_to(inset, inset);
                path.line_to(size - inset, inset);
                path.line_to(size - inset, size - inset);
                path.line_to(inset, size - inset);
                path.close();
            }
            let path = path.finish().unwrap();
            button.stroke_path(
                &path,
                &paint,
                &Stroke {
                    width: scale + 0.5,
                    ..Default::default()
                },
                Default::default(),
                None,
            );
        }
    }

    button
}

fn title_pixmap(title: &str, max_width: u32, height: u32, scale: f32) -> Option<Pixmap> {
    if title.is_empty() {
        return None;
    }

    let (glyphs, font) = layout_title_glyphs(title, max_width, height, scale);

    let width = glyphs
        .last()
        .map(|g| g.position.x + font.h_advance(g.id))?
        .ceil() as u32;

    let mut pixmap = Pixmap::new(width, height).unwrap();
    let data = pixmap.pixels_mut();

    for glyph in glyphs {
        if let Some(og) = font.outline_glyph(glyph) {
            let bounds = og.px_bounds();
            og.draw(|x, y, coverage| {
                let pixel_idx =
                    ((bounds.min.x as u32 + x) + (bounds.min.y as u32 + y) * width) as usize;

                data[pixel_idx] =
                    ColorU8::from_rgba(0, 0, 0, (coverage * 255.0) as u8).premultiply();
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
            .find("opensans", None)
            .expect("Failed to load Open Sans Regular.");
        let data = std::fs::read(font.path).expect("Failed to read font from disk.");
        Cow::Owned(data)
    }
    #[cfg(not(feature = "fontconfig"))]
    {
        Cow::Borrowed(include_bytes!("../../OpenSans-Regular.ttf"))
    }
});

static FONT: LazyLock<FontRef<'_>> =
    LazyLock::new(|| FontRef::try_from_slice(FONT_DATA.as_ref()).unwrap());

fn layout_title_glyphs(
    text: &str,
    max_width: u32,
    height: u32,
    scale: f32,
) -> (Vec<Glyph>, PxScaleFont<&FontRef<'_>>) {
    const TEXT_SIZE: f32 = 10.0;
    const TEXT_MARGIN: f32 = 11.0;

    let mut ret = Vec::<Glyph>::new();

    let px_scale = FONT.pt_to_px_scale(TEXT_SIZE * scale).unwrap();
    let font = FONT.as_scaled(px_scale);
    for c in text.chars() {
        let mut glyph = font.scaled_glyph(c);
        // This centers the glyphs vertically
        glyph.position.y = (height as f32 / 2.0) - font.descent();
        if let Some(previous) = ret.last() {
            glyph.position.x = previous.position.x
                + font.h_advance(previous.id)
                + font.kern(glyph.id, previous.id);
        } else {
            glyph.position.x = TEXT_MARGIN * scale;
        }
        if (glyph.position.x + font.h_advance(glyph.id)).ceil() as u32 > max_width {
            break;
        }

        ret.push(glyph);
    }

    (ret, font)
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

pub fn handle_pointer_leave(state: &InnerServerState<impl X11Selection>, parent: Entity) {
    if let Some(mut decoration) = get_decoration(&state.world, parent) {
        decoration.handle_leave(&state.world);
    }
}

pub fn handle_pointer_motion(
    state: &InnerServerState<impl X11Selection>,
    parent: Entity,
    surface_x: f64,
    surface_y: f64,
) {
    if let Some(mut decoration) = get_decoration(&state.world, parent) {
        decoration.handle_motion(&state.world, surface_x, surface_y);
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
        unreachable!()
    };

    let action = toplevel
        .decoration
        .satellite
        .as_mut()
        .unwrap()
        .handle_click(&toplevel.toplevel, seat, serial);
    let window = *state.world.get::<&x::Window>(parent).unwrap();
    drop(role);

    match action {
        DecorationAction::Close => state.close_x_window(window),
        DecorationAction::ToggleMaximized => {
            state.set_maximized(window, crate::xstate::SetState::Toggle)
        }
        DecorationAction::Minimize => {
            state.set_minimized(window);
            state.connection.set_minimized(window, true);
        }
        DecorationAction::Move => {}
    }
}
