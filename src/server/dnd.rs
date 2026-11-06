use super::InnerServerState;
use super::WindowData;
use super::clientside::WlDndEvent;
use super::event::SurfaceScaleFactor;
use crate::X11Selection;
use hecs::Entity;
use log::warn;
use rustix::event::{PollFd, PollFlags, poll};
use smithay_client_toolkit::data_device_manager::ReadPipe;
use smithay_client_toolkit::data_device_manager::data_offer::DragOffer;
use std::io::Read;
use wayland_client::Proxy;
use xcb::x;

/// A Wayland drag over one of our X windows, in X root coordinates.
pub enum DndEvent {
    Enter {
        offer: DragOffer,
        window: x::Window,
        x: i16,
        y: i16,
    },
    Motion {
        x: i16,
        y: i16,
    },
    Leave,
    Drop(DragOffer),
}

/// Read a Wayland transfer to the end, giving up if the source goes quiet: a stuck source
/// must not freeze every X client.
pub(crate) fn read_pipe(mut pipe: ReadPipe) -> Vec<u8> {
    let mut data = Vec::new();
    let mut buf = [0; 65536];
    loop {
        let mut fds = [PollFd::new(&pipe, PollFlags::IN)];
        match poll(&mut fds, Some(&crate::timespec_from_millis(2000))) {
            Ok(0) => {
                warn!("Wayland data source timed out after {} bytes", data.len());
                break;
            }
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                warn!("Failed polling Wayland data source: {e}");
                break;
            }
        }
        match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => data.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                warn!("Failed reading Wayland data source: {e}");
                break;
            }
        }
    }
    data
}

impl<S: X11Selection> InnerServerState<S> {
    fn dnd_root_pos(&self, entity: Entity, x: f64, y: f64) -> Option<(x::Window, i16, i16)> {
        let mut query = self
            .world
            .query_one::<(&x::Window, &WindowData, &SurfaceScaleFactor)>(entity)
            .ok()?;
        let (window, data, scale) = query.get()?;
        let dims = &data.attrs.dims;
        Some((
            *window,
            dims.x.saturating_add((x * scale.0) as i16),
            dims.y.saturating_add((y * scale.0) as i16),
        ))
    }

    pub(crate) fn take_dnd_events(&mut self) -> Vec<DndEvent> {
        let mut out = Vec::new();
        for event in std::mem::take(&mut self.world.dnd) {
            match event {
                WlDndEvent::Enter(offer) => {
                    // Decoration surfaces carry no Entity: they are no drop target.
                    self.dnd_surface = offer.surface.data::<Entity>().copied();
                    match self
                        .dnd_surface
                        .and_then(|e| self.dnd_root_pos(e, offer.x, offer.y))
                    {
                        Some((window, x, y)) => out.push(DndEvent::Enter {
                            offer,
                            window,
                            x,
                            y,
                        }),
                        None => self.dnd_surface = None,
                    }
                }
                WlDndEvent::Motion(x, y) => {
                    if let Some((_, x, y)) =
                        self.dnd_surface.and_then(|e| self.dnd_root_pos(e, x, y))
                    {
                        out.push(DndEvent::Motion { x, y });
                    }
                }
                WlDndEvent::Leave => {
                    if self.dnd_surface.take().is_some() {
                        out.push(DndEvent::Leave);
                    }
                }
                WlDndEvent::Drop(offer) => {
                    if self.dnd_surface.is_some() {
                        out.push(DndEvent::Drop(offer));
                    }
                }
            }
        }
        out
    }

    pub(crate) fn receive_dnd(&self, offer: &DragOffer, mime: String) -> Vec<u8> {
        match offer.receive(mime) {
            Ok(pipe) => {
                self.flush_wayland();
                read_pipe(pipe)
            }
            Err(e) => {
                warn!("Could not receive dropped data: {e}");
                Vec::new()
            }
        }
    }

    pub(crate) fn flush_wayland(&self) {
        if let Err(e) = self.queue.flush() {
            warn!("Failed to flush Wayland requests: {e}");
        }
    }
}
