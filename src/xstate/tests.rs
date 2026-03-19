use super::*;

use xcb::{XidNew, x::Atom};

impl WindowTypes {
    fn new() -> Self {
        Self {
            ty: Atom::new(0x100),
            normal: Atom::new(0x101),
            dialog: Atom::new(0x102),
            drag_n_drop: Atom::new(0x103),
            splash: Atom::new(0x104),
            menu: Atom::new(0x105),
            popup_menu: Atom::new(0x106),
            dropdown_menu: Atom::new(0x107),
            utility: Atom::new(0x109),
            tooltip: Atom::new(0x109),
            combo: Atom::new(0x10a),
        }
    }
}

// The Tronche version of the ICCCM inexplicably leave out all of the `min` fields.
// See the original ICCCM: https://www.x.org/docs/ICCCM/icccm.pdf page 22 (PDF page 27).
struct WmNormalHints {
    fields: [u32; 18],
}
impl From<WmNormalHints> for super::WmNormalHints {
    fn from(value: WmNormalHints) -> Self {
        value.fields.as_slice().into()
    }
}
impl WmNormalHints {
    fn new() -> Self {
        Self { fields: [0; 18] }
    }
    /// If called after `program_pos`, both positions will be set to the value passed to `user_pos`.
    fn user_pos(mut self, x: u32, y: u32) -> Self {
        self.fields[0] |= 0x001;
        self.fields[1] = x;
        self.fields[2] = y;
        self
    }
    /// If called after `program_size`, both sizes will be set to the value passed to `user_size`.
    fn user_size(mut self, w: u32, h: u32) -> Self {
        self.fields[0] |= 0x002;
        self.fields[3] = w;
        self.fields[4] = h;
        self
    }
    /// If called after `user_pos`, both positions will be set to the value passed to `program_pos`.
    fn program_pos(mut self, x: u32, y: u32) -> Self {
        self.fields[0] |= 0x004;
        self.fields[1] = x;
        self.fields[2] = y;
        self
    }
    /// If called after `user_size`, both sizes will be set to the value passed to `program_size`.
    fn program_size(mut self, w: u32, h: u32) -> Self {
        self.fields[0] |= 0x008;
        self.fields[3] = w;
        self.fields[4] = h;
        self
    }
    fn min_size(mut self, w: u32, h: u32) -> Self {
        self.fields[0] |= 0x010;
        self.fields[5] = w;
        self.fields[6] = h;
        self
    }
    fn max_size(mut self, w: u32, h: u32) -> Self {
        self.fields[0] |= 0x020;
        self.fields[7] = w;
        self.fields[8] = h;
        self
    }
    fn resize_incr(mut self, w: u32, h: u32) -> Self {
        self.fields[0] |= 0x040;
        self.fields[9] = w;
        self.fields[10] = h;
        self
    }
    /// Each tuple's first number is its numerator and second is its denominator.
    fn _aspect_ratios(mut self, min: (u32, u32), max: (u32, u32)) -> Self {
        self.fields[0] |= 0x080;
        self.fields[11] = min.0;
        self.fields[12] = min.1;
        self.fields[13] = max.0;
        self.fields[14] = max.1;
        self
    }
    fn base_size(mut self, w: u32, h: u32) -> Self {
        self.fields[0] |= 0x100;
        self.fields[15] = w;
        self.fields[16] = h;
        self
    }
    fn win_gravity(mut self, gravity: xcb::x::Gravity) -> Self {
        self.fields[0] |= 0x200;
        self.fields[17] = gravity as _;
        self
    }
}

#[allow(dead_code)]
#[repr(u32)]
enum StateHint {
    DoesNotCare = 0,
    Normal = 1,
    Zoomed = 2,
    Iconic = 3,
    Inactive = 4,
}

// Similarly to with WM_NORMAL_HINTS, the Tronche rendition leaves out the `window_group` field.
// See https://www.x.org/docs/ICCCM/icccm.pdf page 23 (PDF page 28).
struct WmHints {
    fields: [u32; 9],
}
impl From<WmHints> for super::WmHints {
    fn from(value: WmHints) -> Self {
        value.fields.as_slice().into()
    }
}
impl WmHints {
    fn new() -> Self {
        Self { fields: [0; 9] }
    }
    fn input_model(mut self, input_hint: bool) -> Self {
        self.fields[0] |= 0x001;
        self.fields[1] = input_hint as _;
        self
    }
    fn initial_state(mut self, state_hint: StateHint) -> Self {
        self.fields[0] |= 0x002;
        self.fields[2] = state_hint as _;
        self
    }
    fn icon_pixmap(mut self, pixmap: u32) -> Self {
        self.fields[0] |= 0x004;
        self.fields[3] = pixmap;
        self
    }
    fn icon_window(mut self, window: u32) -> Self {
        self.fields[0] |= 0x008;
        self.fields[4] = window;
        self
    }
    fn _icon_position(mut self, x: u32, y: u32) -> Self {
        self.fields[0] |= 0x010;
        self.fields[5] = x;
        self.fields[6] = y;
        self
    }
    fn icon_mask(mut self, mask: u32) -> Self {
        self.fields[0] |= 0x020;
        self.fields[7] = mask;
        self
    }
    fn group_leader(mut self, group: u32) -> Self {
        self.fields[0] |= 0x040;
        self.fields[8] = group;
        self
    }
    fn _urgent(mut self) -> Self {
        self.fields[0] |= 0x100;
        self
    }
}

mod wrh {
    use super::{StateHint, WmHints, WmNormalHints};
    use crate::xstate::{WindowRole, WindowRoleHeuristics, WindowTypes, motif};
    use xcb::x::Gravity;

    #[test]
    fn default() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics::default();
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/110
    // Popup because _MOTIF_WM_HINTS indicates user cannot interact which the window
    #[test]
    fn ghidra_popup() {
        let win_types = WindowTypes::new();
        let wm_hints = WmHints::new().input_model(false).group_leader(0x3e00075);
        let wm_normal_hints = WmNormalHints::new()
            .program_pos(221, 589)
            .program_size(133, 28)
            .win_gravity(Gravity::NorthWest);
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.dialog],
            skip_taskbar: Some(true),
            wm_hints: Some(wm_hints.into()),
            has_transient_for: true,
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0, 0, 0, 0].as_slice())),
            wm_normal_hints: Some(wm_normal_hints.into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/112
    #[test]
    fn reaper_main_app() {
        let win_types = WindowTypes::new();
        let wm_hints = WmHints::new()
            .input_model(true)
            .initial_state(StateHint::Normal)
            .icon_pixmap(0x4000128)
            .icon_mask(0x400012e)
            .group_leader(0x4000001);
        let wm_normal_hints = WmNormalHints::new().program_size(936, 1048);
        let win = WindowRoleHeuristics {
            wm_hints: Some(wm_hints.into()),
            motif_wm_hints: Some(motif::Hints::from([0x2_u32, 0, 0x11, 0, 0].as_slice())),
            window_types: vec![win_types.normal],
            wm_normal_hints: Some(wm_normal_hints.into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }
    // Originally, `skip_taskbar` was used to check the pop-up status of the dialog, but since then,
    // override_redirect with no NET window type (fallback to NORMAL) is the used heuristic
    #[test]
    fn reaper_dialog() {
        let win_types = WindowTypes::new();
        let wm_hints = WmHints::new()
            .input_model(true)
            .initial_state(StateHint::Normal)
            .icon_window(0x4000001);
        let wm_normal_hints = WmNormalHints::new()
            .program_pos(0, 0)
            .min_size(382, 160)
            .max_size(382, 160);
        let win = WindowRoleHeuristics {
            override_redirect: true,
            wm_hints: Some(wm_hints.into()),
            skip_taskbar: Some(true),
            wm_normal_hints: Some(wm_normal_hints.into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/161
    // Popup for specifying the _NET_WM_WINDOW_TYPE_MENU window type
    #[test]
    fn chromium_tooltip() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.menu],
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/166
    // Popup for specifying the _NET_WM_WINDOW_TYPE_DND window type
    #[test]
    fn discord_dnd() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.drag_n_drop],
            override_redirect: true,
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/253
    // Popup for specifying the _NET_WM_WINDOW_TYPE_POPUP_MENU window type
    #[test]
    fn git_gui_popup() {
        let win_types = WindowTypes::new();
        let wm_normal_hints = WmNormalHints::new()
            .user_pos(0, 0)
            .min_size(1, 1)
            .resize_incr(1, 1);
        let wm_hints = WmHints::new()
            .input_model(true)
            .initial_state(StateHint::Normal);
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.popup_menu],
            override_redirect: true,
            wm_normal_hints: Some(wm_normal_hints.into()),
            wm_hints: Some(wm_hints.into()),
            skip_taskbar: Some(false),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // Popup for specifying the _NET_WM_WINDOW_TYPE_DROPDOWN_MENU window type
    #[test]
    fn dropdown_menu() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.dropdown_menu],
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/277
    // _NET_WM_WINDOW_TYPE_UTILITY is more complex. `override_redirect` the effective heuristic
    #[test]
    fn wechat_popup() {
        let win_types = WindowTypes::new();
        let wm_hints = WmHints::new().input_model(false).group_leader(0xa00008);
        let wm_normal_hints = WmNormalHints::new()
            .user_pos(2178, 102)
            .user_size(560, 112)
            .min_size(560, 112)
            .max_size(560, 112)
            .resize_incr(2, 2)
            .base_size(0, 0)
            .win_gravity(Gravity::Static);
        let win = WindowRoleHeuristics {
            override_redirect: true,
            has_transient_for: true,
            motif_wm_hints: Some(motif::Hints::from([0x2_u32, 1, 0, 0, 0].as_slice())),
            window_types: vec![win_types.utility, win_types.normal],
            wm_hints: Some(wm_hints.into()),
            wm_normal_hints: Some(wm_normal_hints.into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/pull/323
    // Same as above, UTILITY + override_redirect
    #[test]
    fn godot_popup() {
        let win_types = WindowTypes::new();
        let wm_normal_hints = WmNormalHints::new()
            .program_pos(2542, 338)
            .program_size(245, 329)
            .min_size(245, 329)
            .max_size(245, 329);
        let win = WindowRoleHeuristics {
            has_transient_for: true,
            wm_normal_hints: Some(wm_normal_hints.into()),
            override_redirect: true,
            window_types: vec![win_types.utility],
            motif_wm_hints: Some(motif::Hints::from([0x2_u32, 0, 0, 0, 0].as_slice())),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }
    // A UTILITY type can also be a pop-up if the `_MOTIF_WM_HINTS` have no decorations.
    // The above test also meets these conditions, but override_redirect takes precedence.
    #[test]
    fn material_maker_popup() {
        let win_types = WindowTypes::new();
        let wm_normal_hints = WmNormalHints::new()
            .program_pos(2453, 413)
            .program_size(340, 400)
            .min_size(340, 400)
            .max_size(340, 400);
        let win = WindowRoleHeuristics {
            has_transient_for: true,
            wm_normal_hints: Some(wm_normal_hints.into()),
            window_types: vec![win_types.utility],
            motif_wm_hints: Some(motif::Hints::from([0x2_u32, 0, 0, 0, 0].as_slice())),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/294
    // UTILITY types with no MOTIF decorations or override_redirect should be top-levels.
    #[test]
    fn ardour_vst3_plugin() {
        let win_types = WindowTypes::new();
        let wm_hints = WmHints::new()
            .input_model(true)
            .initial_state(StateHint::Normal)
            .icon_pixmap(0x16002e2)
            .icon_mask(0x16002e3)
            .group_leader(0x1600001);
        let wm_normal_hints = WmNormalHints::new()
            .program_pos(0, 0)
            .min_size(1190, 769)
            .max_size(1190, 769)
            .win_gravity(Gravity::NorthWest);
        let win = WindowRoleHeuristics {
            wm_hints: Some(wm_hints.into()),
            window_types: vec![win_types.utility],
            wm_normal_hints: Some(wm_normal_hints.into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }
    #[test]
    fn ardour_midi_setup_dialog() {
        let win_types = WindowTypes::new();
        let wm_hints = WmHints::new()
            .input_model(true)
            .initial_state(StateHint::Normal)
            .icon_pixmap(0x14002ce)
            .icon_mask(0x14002cf)
            .group_leader(0x1400001);
        let wm_normal_hints = WmNormalHints::new()
            .program_pos(0, 0)
            .min_size(633, 397)
            .win_gravity(Gravity::NorthWest);
        let win = WindowRoleHeuristics {
            wm_hints: Some(wm_hints.into()),
            has_transient_for: true,
            window_types: vec![win_types.utility],
            wm_normal_hints: Some(wm_normal_hints.into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/383
    // UTILITY types which can be resized (min_size != max_size) should be top-level
    #[test]
    fn davinci_resolve_transcription() {
        let win_types = WindowTypes::new();
        let wm_hints = WmHints::new().input_model(true).group_leader(0x600008);
        let wm_normal_hints = WmNormalHints::new()
            .user_pos(4225, 154)
            .user_size(660, 657)
            .min_size(660, 590)
            .win_gravity(Gravity::Static);
        let win = WindowRoleHeuristics {
            has_transient_for: true,
            motif_wm_hints: Some(motif::Hints::from([0x2_u32, 0x1, 0, 0, 0].as_slice())),
            window_types: vec![win_types.utility, win_types.normal],
            wm_hints: Some(wm_hints.into()),
            wm_normal_hints: Some(wm_normal_hints.into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/356
    // Popup for specifying the _NET_WM_WINDOW_TYPE_COMBO window type
    #[test]
    fn fcitx5_popup() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.combo],
            override_redirect: true,
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/pull/328
    // Utilizing the WM_HINTS input focus set to false, along with no decoration in MOTIF hints to
    // avoid false positives (because of Pixel Composer, skip_taskbar is not one of those hints)
    #[test]
    fn yabridge_vst_menu() {
        let win_types = WindowTypes::new();
        let wm_hints = WmHints::new()
            .input_model(false)
            .initial_state(StateHint::Normal)
            .icon_pixmap(0xc00164)
            .icon_mask(0xc00166)
            .group_leader(0xe00006);
        let wm_normal_hints = WmNormalHints::new()
            .program_pos(1920, 0)
            .min_size(307, 700)
            .max_size(307, 700)
            .win_gravity(Gravity::Static);
        let win = WindowRoleHeuristics {
            skip_taskbar: Some(true),
            wm_hints: Some(wm_hints.into()),
            window_types: vec![win_types.normal],
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0x24, 0, 0, 0].as_slice())),
            wm_normal_hints: Some(wm_normal_hints.into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }
    #[test]
    fn steam_pixel_composer() {
        let win_types = WindowTypes::new();
        let wm_hints = WmHints::new()
            .input_model(false)
            .initial_state(StateHint::Normal)
            .icon_pixmap(0x4600036)
            .icon_mask(0x4600038)
            .group_leader(0x4800001);
        let wm_normal_hints = WmNormalHints::new()
            .program_pos(2387, 267)
            .min_size(1600, 800)
            .max_size(1600, 800)
            .win_gravity(Gravity::Static);
        let win = WindowRoleHeuristics {
            wm_hints: Some(wm_hints.into()),
            window_types: vec![win_types.normal],
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0x24, 0, 0, 0].as_slice())),
            wm_normal_hints: Some(wm_normal_hints.into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }
    #[test]
    fn battlenet_login() {
        let win_types = WindowTypes::new();
        let wm_hints = WmHints::new()
            .input_model(false)
            .initial_state(StateHint::Normal)
            .icon_pixmap(0x30005d9)
            .icon_mask(0x30005db)
            .group_leader(0x320000a);
        let wm_normal_hints = WmNormalHints::new()
            .program_pos(2173, -19 as _)
            .min_size(362, 645)
            .max_size(362, 645)
            .win_gravity(Gravity::Static);
        let win = WindowRoleHeuristics {
            wm_hints: Some(wm_hints.into()),
            window_types: vec![win_types.normal],
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0x2c, 0, 0, 0].as_slice())),
            wm_normal_hints: Some(wm_normal_hints.into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/365
    // Similar to Battle.net above, certain MOTIF hints settings overrule the WM_HINTS pop-up
    // heuristic, so the window is top-level.
    #[test]
    fn wallpaper_engine() {
        let win_types = WindowTypes::new();
        let wm_hints = WmHints::new()
            .input_model(false)
            .initial_state(StateHint::Normal)
            .icon_pixmap(0x4a00031)
            .icon_mask(0x4a00033)
            .group_leader(0x4c00004);
        let wm_normal_hints = WmNormalHints::new()
            .program_pos(1920, 0)
            .win_gravity(Gravity::Static);
        let win = WindowRoleHeuristics {
            wm_hints: Some(wm_hints.into()),
            window_types: vec![win_types.normal],
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0x3e, 0, 0, 0].as_slice())),
            wm_normal_hints: Some(wm_normal_hints.into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/280
    // _NET_WM_WINDOW_TYPE_DIALOG is a pop-up if it has a window it is transient for
    #[test]
    fn clip_studio_paint_menu() {
        let win_types = WindowTypes::new();
        let wm_hints = WmHints::new()
            .input_model(false)
            .initial_state(StateHint::Normal)
            .icon_pixmap(0xe06180)
            .icon_mask(0xe06182)
            .group_leader(0x100000c);
        let wm_normal_hints = WmNormalHints::new()
            .program_pos(0, 1080)
            .min_size(363, 801)
            .max_size(363, 801)
            .win_gravity(Gravity::Static);
        let win = WindowRoleHeuristics {
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0x24, 0, 0, 0].as_slice())),
            wm_hints: Some(wm_hints.into()),
            window_types: vec![win_types.dialog],
            has_transient_for: true,
            wm_normal_hints: Some(wm_normal_hints.into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/307
    // Same logic as above
    #[test]
    fn davinci_resolve_timeline_menu() {
        let win_types = WindowTypes::new();
        let wm_hints = WmHints::new().input_model(true).group_leader(0xa00008);
        let wm_normal_hints = WmNormalHints::new()
            .user_pos(686, 642)
            .user_size(992, 740)
            .min_size(992, 740)
            .max_size(992, 740)
            .resize_incr(2, 2)
            .base_size(-2 as _, -2 as _)
            .win_gravity(Gravity::Static);
        let win = WindowRoleHeuristics {
            has_transient_for: true,
            motif_wm_hints: Some(motif::Hints::from([0x2_u32, 1, 0, 0, 0].as_slice())),
            window_types: vec![win_types.dialog, win_types.normal],
            wm_hints: Some(wm_hints.into()),
            wm_normal_hints: Some(wm_normal_hints.into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    #[test]
    fn splash_screen() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.splash],
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Splash);
    }
}
