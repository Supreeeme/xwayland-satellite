# xwayland-satellite
xwayland-satellite grants rootless Xwayland integration to any Wayland compositor implementing xdg_wm_base and viewporter.
This is particularly useful for compositors that (understandably) do not want to go through implementing support for rootless Xwayland themselves.

Found a bug? [Open a bug report.](https://github.com/Supreeeme/xwayland-satellite/issues/new?template=bug_report.yaml)

Need help troubleshooting, or have some other general question? [Ask on GitHub Discussions.](https://github.com/Supreeeme/xwayland-satellite/discussions)

## Dependencies
- Xwayland >=23.1
- xcb
- xcb-util-cursor
- clang (building only)

## Usage
Run `xwayland-satellite`. You can specify an X display to use (i.e. `:12`). Be sure to set the same `DISPLAY` environment variable for any X11 clients.
Because xwayland-satellite is a Wayland client (in addition to being a Wayland compositor), it will need to launch after your compositor launches, but obviously before any X11 applications.

## Java applications
Some (most?) Java applications may present themselves as a blank screen by default with satellite. To fix this, simply set the environment variable
`_JAVA_AWT_WM_NONREPARENTING=1` before launching it to fix this. Unfortunately there is not a way for satellite to automatically do this.

## Building
```
# dev build
cargo build
# release build
cargo build --release

# run - will also build if not already built
cargo run # --release
```

## Systemd support
xwayland-satellite can be built with systemd support - simply add `-F systemd` to your build command - i.e. `cargo build --release -F systemd`.

With systemd support, satellite will send a state change notification when Xwayland has been initialized, allowing for having services dependent on satellite's startup.

An example service file is located in `resources/xwayland-satellite.service` - be sure to replace the `ExecStart` line with the proper location before using it.
It can be placed in a systemd user unit directory (i.e. `$XDG_CONFIG_HOME/systemd/user` or `/etc/systemd/user`),
and be launched and enabled with `systemctl --user enable --now xwayland-satellite`.
It will be started when the `graphical-session.target` is reached,
which is likely after your compositor is started if it supports systemd.

## Scaling/HiDPI
For most GTK and Qt apps, xwayland-satellite should automatically scale them properly. Note that for mixed DPI monitor setups, satellite will choose
the smallest monitor's DPI, meaning apps may have small text on other monitors.

Other miscellaneous apps (such as Wine apps) may have small text on HiDPI displays. It is application dependent on getting apps to scale properly with satellite,
so you will have to figure out what app specific config needs to be set. See [the Arch Wiki on HiDPI](https://wiki.archlinux.org/title/HiDPI) for a good place start.

Satellite acts as an Xsettings manager for setting scaling related settings, but will get out of the way of other Xsettings managers.
To manually set these settings, try [xsettingsd](https://codeberg.org/derat/xsettingsd) or another Xsettings manager.

## Wayland protocols used
The host compositor **must** implement the following protocols/interfaces for satellite to function:
- Core interfaces (wl_output, wl_surface, wl_compositor, etc)
- xdg_shell (xdg_wm_base, xdg_surface, xdg_popup, xdg_toplevel)
- wp_viewporter - used for scaling

Additionally, satellite can *optionally* take advantage of the following protocols:
- Linux dmabuf
- XDG activation
- XDG foreign
- Pointer constraints
- Tablet input
- Fractional scale

## Compositor integration
Satellite supports passing through the `-listenfd` Xwayland argument. What this means is you can integrate satellite
(and by extension Xwayland) into your compositor, and do things like on demand activation. Note that you *must* pass
a display number to satellite as the first argument, and then the `-listenfd` argument.

You can view [Niri's implementation of this integration](https://github.com/YaLTeR/niri/pull/1728/files) for understanding
how it should work.


## Extension support
Satellite supports adding extensions to the X server through the `+extension` argument. This is particularly useful for OpenGl, which can be enabled with `+extension GLX`. Similar to `-listenfd`, you *must* pass a display number to satellite as the first argument.
