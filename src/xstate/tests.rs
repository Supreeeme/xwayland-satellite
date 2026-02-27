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
            wm_class: Some("ghidra-Ghidra".into()),
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
            wm_class: Some("REAPER".into()),
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
            wm_class: Some("REAPER".into()),
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
            wm_class: Some("Menu".into()),
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
            wm_class: Some("wechat".into()),
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
            wm_class: Some("Godot".into()),
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
            wm_class: Some("Material Maker".into()),
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
            wm_class: Some("Ardour-8.12.0".into()),
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
            wm_class: Some("Ardour-8.12.0".into()),
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
            wm_class: Some("fcitx".into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/pull/293
    // Yabridge has dropdowns which expect to be pop-ups but does not express that clearly through
    // its X properties (which are not even consistent across WINE versions).
    #[test]
    fn yabridge_vst_menu() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.normal],
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0x24, 0, 0, 0].as_slice())),
            wm_class: Some("yabridge-host.exe.so".into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }
    // Several Steam games running through WINE set WM_HINTS and _MOTIF_WM_HINTS identically to
    // Yabridge (the properties checked for by a previous fix) but which expect to be top-level.
    // See also https://github.com/Supreeeme/xwayland-satellite/issues/365 for more examples.
    #[test]
    fn steam_pixel_composer() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.normal],
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0x24, 0, 0, 0].as_slice())),
            wm_class: Some("steam_app_2299510".into()),
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
            wm_class: Some("battle.net.exe".into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }
    // https://github.com/Supreeeme/xwayland-satellite/issues/365
    #[test]
    fn wallpaper_engine() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            window_types: vec![win_types.normal],
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0x3e, 0, 0, 0].as_slice())),
            wm_class: Some("steam_app_431960".into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/280
    // _NET_WM_WINDOW_TYPE_DIALOG is a pop-up if it has a TRANSIENT_FOR window and no Motif decor
    #[test]
    fn clip_studio_paint_menu() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            motif_wm_hints: Some(motif::Hints::from([0x3_u32, 0x24, 0, 0, 0].as_slice())),
            window_types: vec![win_types.dialog],
            has_transient_for: true,
            wm_class: Some("clipstudiopaint.exe".into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/issues/307
    // Same logic as above
    #[test]
    fn davinci_resolve_timeline_menu() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            has_transient_for: true,
            motif_wm_hints: Some(motif::Hints::from([0x2_u32, 0x1, 0, 0, 0].as_slice())),
            window_types: vec![win_types.dialog, win_types.normal],
            wm_class: Some("resolve".into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Popup);
    }

    // https://github.com/Supreeeme/xwayland-satellite/pull/390
    // With Motif decorations, even a dialog with TRANSIENT_FOR is better treated as a top-level
    #[test]
    fn krita_color_picker() {
        let win_types = WindowTypes::new();
        let win = WindowRoleHeuristics {
            has_transient_for: true,
            motif_wm_hints: Some(motif::Hints::from([0x3, 0x26, 0x1e, 0x0, 0x0].as_slice())),
            window_types: vec![win_types.dialog, win_types.normal],
            wm_class: Some("krita".into()),
            ..Default::default()
        };
        assert_eq!(win.guess_window_role(&win_types), WindowRole::Toplevel);
    }
}
