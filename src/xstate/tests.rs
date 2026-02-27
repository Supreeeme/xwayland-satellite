#![allow(clippy::needless_update)]
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

mod window_role_heuristics {
    use crate::xstate::{WindowRole, WindowRoleHeuristics, WindowTypes, motif};

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
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.dialog],
            has_transient_for: true,
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0, 0, 0, 0].as_slice())),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/112
    #[test]
    fn reaper_main_app() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            motif_wm_hints: Some(motif::Hints::from([0x2_u32, 0, 0x11, 0, 0].as_slice())),
            window_types: vec![win_types.normal],
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }
    // Originally, `skip_taskbar` was used to check the pop-up status of the dialog, but since then,
    // override_redirect with no NET window type (fallback to NORMAL) is the used heuristic
    #[test]
    fn reaper_dialog() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            override_redirect: true,
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
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.popup_menu],
            override_redirect: true,
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
        let win = WindowRoleHeuristics {
            override_redirect: true,
            has_transient_for: true,
            motif_wm_hints: Some(motif::Hints::from([0x2_u32, 1, 0, 0, 0].as_slice())),
            window_types: vec![win_types.utility, win_types.normal],
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/pull/323
    // Same as above, UTILITY + override_redirect
    #[test]
    fn godot_popup() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            has_transient_for: true,
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
        let win = WindowRoleHeuristics {
            has_transient_for: true,
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
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.utility],
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }
    #[test]
    fn ardour_midi_setup_dialog() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            has_transient_for: true,
            window_types: vec![win_types.utility],
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

    // https://github.com/Supreeeme/xwayland-satellite/pull/293
    // Yabridge has dropdowns which expect to be pop-ups but does not express that clearly through
    // its X properties (which are not even consistent across WINE versions).
    #[test]
    #[ignore = "intentional regression: WM_HINTS heuristic removed to fix #365, #392"]
    fn yabridge_vst_menu() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.normal],
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0x24, 0, 0, 0].as_slice())),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }
    // Several Steam games running through WINE set WM_HINTS and _MOTIF_WM_HINTS identically to
    // Yabridge (the properties checked for by a previous fix) but which expect to be top-level.
    #[test]
    fn steam_pixel_composer() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.normal],
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0x24, 0, 0, 0].as_slice())),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }
    #[test]
    fn battlenet_login() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.normal],
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0x2c, 0, 0, 0].as_slice())),
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
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.normal],
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0x3e, 0, 0, 0].as_slice())),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }
}
