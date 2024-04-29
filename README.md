# xwayland-satellite
xwayland-satellite grants rootless Xwayland integration to any Wayland compositor implementing xdg_wm_base.
This is particularly useful for compositors that (understandably) do not want to go through implementing support for rootless Xwayland themselves.

## Usage
Run `xwayland-satellite`. You can specify an X display to use (i.e. `:12`). Be sure to set the same `DISPLAY` environment variable for any X11 clients.
Because xwayland-satellite is a Wayland client (in addition to being a Wayland compositor), it will need to launch after your compositor launches, but obviously before any X11 applications.

## Building
```
cargo build
cargo run
```
