use crate::server::{InnerServerState, ServerState, SurfaceRole};
use crate::{X11Selection, XConnection};

use ab_glyph::{Font, FontRef, Glyph, PxScaleFont, ScaleFont};
use hecs::{CommandBuffer, Entity, World};
use log::{error, warn};
use smithay_client_toolkit::registry::SimpleGlobal;
use smithay_client_toolkit::shm::slot::SlotPool;
use std::sync::LazyLock;
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Stroke, Transform};
use tiny_skia::{ColorU8, Rect};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::Proxy;
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
    x_data: DecorationsBox,
    title: Option<String>,
    title_rect: Rect,
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
        subsurface.set_desync();
        subsurface.set_position(0, -Self::TITLEBAR_HEIGHT);
        let viewport = state.viewporter.get_viewport(&surface, &state.qh, ());

        Some((
            Self {
                surface,
                subsurface,
                pool: pool_entity,
                viewport,
                x_data: DecorationsBox::default(),
                pixmap: Pixmap::new(1, 1).unwrap(),
                scale: 1.0,
                title: title.map(str::to_string),
                title_rect: Rect::from_ltrb(0.0, 0.0, 0.0, 0.0).unwrap(),
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

        self.scale = parent_scale_factor;
        let mut drawn_width = (width as f32 * self.scale).ceil() as i32;
        let drawn_height = (Self::TITLEBAR_HEIGHT as f32 * self.scale).ceil() as i32;

        let x = x_pixmap(drawn_height as u32, self.scale, self.x_data.hovered);
        if x.width() > drawn_width as u32 {
            drawn_width = x.width() as i32;
        }

        let title = self.title.as_ref().and_then(|t| {
            let width = (drawn_width as u32).saturating_sub(x.width());
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

        bar.draw_pixmap(
            (bar.width() - x.width()) as i32,
            0,
            x.as_ref(),
            &Default::default(),
            Transform::identity(),
            None,
        );
        self.x_data = DecorationsBox {
            rect: Rect::from_ltrb(
                width as f32 - Self::TITLEBAR_HEIGHT as f32,
                0.0,
                width as f32,
                Self::TITLEBAR_HEIGHT as f32,
            )
            .unwrap(),
            hovered: false,
        };

        self.pixmap = bar;
        self.viewport.set_destination(width, Self::TITLEBAR_HEIGHT);
        self.update_buffer(world);
    }

    fn redraw_x_pixmap(&mut self, world: &World) {
        let x = x_pixmap(self.pixmap.height(), self.scale, self.x_data.hovered);

        self.pixmap.draw_pixmap(
            (self.pixmap.width() - x.width()) as i32,
            0,
            x.as_ref(),
            &Default::default(),
            Transform::identity(),
            None,
        );

        self.surface.damage_buffer(
            (self.pixmap.width() - x.width()) as i32,
            0,
            x.width() as i32,
            x.height() as i32,
        );
        self.update_buffer(world);
    }

    pub fn set_title(&mut self, world: &World, title: &str) {
        self.title = Some(title.to_string());

        // Don't draw title if there's not enough space
        let title_pixmap = title_pixmap(
            title,
            self.pixmap.width() - self.x_data.rect.width() as u32,
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

    fn handle_motion(&mut self, world: &World, x: f64, y: f64) {
        if self.x_data.check_hovered(x as f32, y as f32) {
            self.redraw_x_pixmap(world);
        }
    }

    fn handle_leave(&mut self, world: &World) {
        if self.x_data.hovered {
            self.x_data.hovered = false;
            self.redraw_x_pixmap(world);
        }
    }

    /// Returns true if the toplevel should be closed
    fn handle_click(&self, toplevel: &XdgToplevel, seat: &WlSeat, serial: u32) -> bool {
        if self.x_data.hovered {
            true
        } else {
            toplevel._move(seat, serial);
            false
        }
    }
}

#[derive(Debug)]
struct DecorationsBox {
    rect: Rect,
    hovered: bool,
}

impl Default for DecorationsBox {
    fn default() -> Self {
        Self {
            rect: Rect::from_xywh(0.0, 0.0, 0.0, 0.0).unwrap(),
            hovered: false,
        }
    }
}

impl DecorationsBox {
    /// Returns true if hover state changed.
    fn check_hovered(&mut self, x: f32, y: f32) -> bool {
        let old_hovered = self.hovered;
        self.hovered = (self.rect.left()..=self.rect.right()).contains(&x)
            && (self.rect.top()..=self.rect.bottom()).contains(&y);

        old_hovered != self.hovered
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

fn x_pixmap(bar_height: u32, scale: f32, hovered: bool) -> Pixmap {
    let mut x = Pixmap::new(bar_height, bar_height).unwrap();
    if hovered {
        x.fill(Color::from_rgba(1.0, 0.0, 0.0, 0.8).unwrap());
    } else {
        x.fill(Color::WHITE);
    }
    let size = x.width() as f32;
    let margin = 8.4 * scale;

    let mut line = PathBuilder::new();
    line.move_to(margin, margin);
    line.line_to(size - margin, size - margin);
    line.move_to(size - margin, margin);
    line.line_to(margin, size - margin);
    let line = line.finish().unwrap();
    x.stroke_path(
        &line,
        &Default::default(),
        &Stroke {
            width: scale + 0.5,
            ..Default::default()
        },
        Default::default(),
        None,
    );

    x
}

fn title_pixmap(title: &str, max_width: u32, height: u32, scale: f32) -> Option<Pixmap> {
    if title.is_empty() {
        return None;
    }

    let (glyphs, font) = layout_title_glyphs(title, max_width, height, scale);

    let width = glyphs
        .last()
        .map(|g| g.position.x + font.h_advance(g.id))
        .unwrap()
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

fn layout_title_glyphs(
    text: &str,
    max_width: u32,
    height: u32,
    scale: f32,
) -> (Vec<Glyph>, PxScaleFont<&FontRef<'_>>) {
    const TEXT_SIZE: f32 = 10.0;
    const TEXT_MARGIN: f32 = 11.0;
    static FONT: LazyLock<FontRef<'_>> = LazyLock::new(|| {
        FontRef::try_from_slice(include_bytes!("../../OpenSans-Regular.ttf")).unwrap()
    });

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

    if toplevel
        .decoration
        .satellite
        .as_mut()
        .unwrap()
        .handle_click(&toplevel.toplevel, seat, serial)
    {
        let window = *state.world.get::<&x::Window>(parent).unwrap();
        drop(role);
        state.close_x_window(window);
    }
}
