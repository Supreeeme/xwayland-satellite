//! Recognizing unnamed X cursors by their image.
//!
//! libXcursor names every cursor it loads by name, but toolkits that read the
//! theme themselves and create the cursor from the image do not: Chromium (and
//! with it Electron apps such as Discord) and GLFW (Minecraft and other LWJGL
//! games) both do this. Their cursors are still the theme's images, pixel for
//! pixel, so hashing the images of the cursors we can forward as shapes lets
//! such an unnamed cursor be matched back to its name.
//!
//! Themes are looked up the way libXcursor does (`XCURSOR_PATH`, `Inherits`),
//! for every theme a client is likely to use: `XCURSOR_THEME`, the
//! `Xcursor.theme` resource, GTK's `gtk-cursor-theme-name` (which Chromium
//! follows) and `default`. Every image of every size and animation frame is
//! indexed, so the size the client chose does not matter.

use log::{debug, warn};
use std::collections::hash_map::{DefaultHasher, Entry};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

const XCURSOR_MAGIC: &[u8; 4] = b"Xcur";
const XCURSOR_IMAGE_TYPE: u32 = 0xfffd_0002;
/// Guard against runaway `Inherits` chains.
const MAX_INHERIT_DEPTH: usize = 8;
/// Largest image indexed; cursor themes top out well below this.
const MAX_IMAGE_SIDE: u32 = 512;

/// Hash of a cursor image as XFixes reports it: its size and its ARGB pixels.
pub(super) fn image_hash(width: u32, height: u32, pixels: &[u32]) -> u64 {
    let mut hasher = DefaultHasher::new();
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    pixels.hash(&mut hasher);
    hasher.finish()
}

/// Cursor names by image hash.
#[derive(Default)]
pub(super) struct CursorImageIndex {
    /// `None` marks an image shared by names of different shapes, which cannot
    /// be told apart.
    names: HashMap<u64, Option<&'static str>>,
}

impl CursorImageIndex {
    /// Index the images of `names` in each of `themes`.
    ///
    /// `same_cursor(a, b)` says whether two names stand for the same cursor,
    /// so an image that several aliases share is not reported as ambiguous.
    pub fn build(
        themes: &[String],
        names: impl Iterator<Item = &'static str> + Clone,
        search_path: &[PathBuf],
        same_cursor: impl Fn(&str, &str) -> bool,
    ) -> Self {
        let mut index = Self::default();
        // Aliases are usually symlinks to one file: parse each file once, but
        // record the image hashes under every name that leads to it.
        let mut hashes_by_file: HashMap<PathBuf, Vec<u64>> = HashMap::new();
        let mut indexed = HashSet::new();
        for theme in themes {
            for name in names.clone() {
                let Some(path) = find_cursor_file(search_path, theme, name) else {
                    continue;
                };
                let Ok(canonical) = path.canonicalize() else {
                    continue;
                };
                if !indexed.insert((canonical.clone(), name)) {
                    continue;
                }
                let hashes =
                    hashes_by_file
                        .entry(canonical)
                        .or_insert_with_key(|file| match std::fs::read(file) {
                            Ok(data) => parse_xcursor_images(&data)
                                .iter()
                                .map(|(width, height, pixels)| image_hash(*width, *height, pixels))
                                .collect(),
                            Err(err) => {
                                warn!("could not read cursor {}: {err}", file.display());
                                Vec::new()
                            }
                        });
                for hash in hashes.iter() {
                    index.insert(*hash, name, &same_cursor);
                }
            }
        }
        debug!(
            "indexed {} cursor images from themes {themes:?}",
            index.names.len()
        );
        index
    }

    fn insert(&mut self, hash: u64, name: &'static str, same_cursor: impl Fn(&str, &str) -> bool) {
        match self.names.entry(hash) {
            Entry::Vacant(entry) => {
                entry.insert(Some(name));
            }
            Entry::Occupied(mut entry) => {
                if let Some(existing) = *entry.get()
                    && !same_cursor(existing, name)
                {
                    entry.insert(None);
                }
            }
        }
    }

    /// The name of the cursor with this image, if it is known and unambiguous.
    pub fn lookup(&self, hash: u64) -> Option<&'static str> {
        self.names.get(&hash).copied().flatten()
    }
}

/// Themes a client may take its cursors from, most likely first.
pub(super) fn candidate_themes(resources: &str) -> Vec<String> {
    let mut themes = Vec::new();
    let mut push = |theme: Option<String>| {
        if let Some(theme) = theme.map(|t| t.trim().to_owned())
            && !theme.is_empty()
            && !themes.contains(&theme)
        {
            themes.push(theme);
        }
    };
    push(std::env::var("XCURSOR_THEME").ok());
    push(xresource(resources, "Xcursor.theme"));
    for gtk in ["gtk-3.0", "gtk-4.0"] {
        push(
            config_home()
                .map(|dir| dir.join(gtk).join("settings.ini"))
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|ini| ini_value(&ini, "gtk-cursor-theme-name")),
        );
    }
    push(Some("default".to_owned()));
    themes
}

/// The directories libXcursor searches for themes.
pub(super) fn search_path() -> Vec<PathBuf> {
    if let Ok(path) = std::env::var("XCURSOR_PATH") {
        return path
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(expand_home)
            .collect();
    }
    let mut dirs = Vec::new();
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|home| home.join(".local/share")))
    {
        dirs.push(data_home.join("icons"));
    }
    if let Some(home) = home() {
        dirs.push(home.join(".icons"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(|dir| Path::new(dir).join("icons")),
    );
    dirs.push(PathBuf::from("/usr/share/pixmaps"));
    dirs
}

/// The file for cursor `name` in `theme`, following `Inherits`.
fn find_cursor_file(search_path: &[PathBuf], theme: &str, name: &str) -> Option<PathBuf> {
    let mut visited = HashSet::new();
    find_cursor_file_in(search_path, theme, name, 0, &mut visited)
}

fn find_cursor_file_in(
    search_path: &[PathBuf],
    theme: &str,
    name: &str,
    depth: usize,
    visited: &mut HashSet<String>,
) -> Option<PathBuf> {
    if depth > MAX_INHERIT_DEPTH || !visited.insert(theme.to_owned()) {
        return None;
    }
    for dir in search_path {
        let path = dir.join(theme).join("cursors").join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    for dir in search_path {
        let Ok(index) = std::fs::read_to_string(dir.join(theme).join("index.theme")) else {
            continue;
        };
        let Some(inherits) = ini_value(&index, "Inherits") else {
            continue;
        };
        for parent in inherits.split([',', ';']).map(str::trim) {
            if parent.is_empty() {
                continue;
            }
            if let Some(path) = find_cursor_file_in(search_path, parent, name, depth + 1, visited) {
                return Some(path);
            }
        }
    }
    None
}

/// The images in an Xcursor file, as (width, height, ARGB pixels).
pub(super) fn parse_xcursor_images(data: &[u8]) -> Vec<(u32, u32, Vec<u32>)> {
    let u32_at = |offset: usize| -> Option<u32> {
        data.get(offset..offset + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    let mut images = Vec::new();
    if data.get(0..4) != Some(XCURSOR_MAGIC) {
        return images;
    }
    let (Some(header), Some(ntoc)) = (u32_at(4), u32_at(12)) else {
        return images;
    };
    for entry in 0..ntoc as usize {
        let toc = header as usize + entry * 12;
        let (Some(kind), Some(position)) = (u32_at(toc), u32_at(toc + 8)) else {
            break;
        };
        if kind != XCURSOR_IMAGE_TYPE {
            continue;
        }
        // Chunk: header size, type, subtype, version, width, height, xhot,
        // yhot, delay, then the pixels.
        let chunk = position as usize;
        let (Some(chunk_header), Some(width), Some(height)) =
            (u32_at(chunk), u32_at(chunk + 16), u32_at(chunk + 20))
        else {
            continue;
        };
        if width == 0 || height == 0 || width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
            continue;
        }
        let start = chunk + chunk_header as usize;
        let len = (width * height) as usize * 4;
        let Some(bytes) = data.get(start..start + len) else {
            continue;
        };
        let pixels = bytes
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        images.push((width, height, pixels));
    }
    images
}

/// Value of `key` in an X resource database string.
fn xresource(resources: &str, key: &str) -> Option<String> {
    resources.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim() == key).then(|| v.trim().to_owned())
    })
}

/// Value of `key` in an ini-style file (any section).
fn ini_value(ini: &str, key: &str) -> Option<String> {
    ini.lines().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        (k.trim() == key).then(|| v.trim().trim_matches('"').to_owned())
    })
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|home| home.join(".config")))
}

fn expand_home(dir: &str) -> PathBuf {
    match (dir.strip_prefix("~/"), home()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An Xcursor file holding the given images (width, height, pixels).
    fn xcursor_file(images: &[(u32, u32, Vec<u32>)]) -> Vec<u8> {
        let header = 16u32;
        let toc_len = 12 * images.len() as u32;
        let mut out = Vec::new();
        out.extend_from_slice(XCURSOR_MAGIC);
        for v in [header, 0x1_0000, images.len() as u32] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        let mut position = header + toc_len;
        let mut chunks = Vec::new();
        for (width, height, pixels) in images {
            for v in [XCURSOR_IMAGE_TYPE, *width, position] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            let mut chunk = Vec::new();
            for v in [36, XCURSOR_IMAGE_TYPE, *width, 1, *width, *height, 0, 0, 0] {
                chunk.extend_from_slice(&u32::to_le_bytes(v));
            }
            for p in pixels {
                chunk.extend_from_slice(&p.to_le_bytes());
            }
            position += chunk.len() as u32;
            chunks.push(chunk);
        }
        for chunk in chunks {
            out.extend_from_slice(&chunk);
        }
        out
    }

    fn image(side: u32, seed: u32) -> (u32, u32, Vec<u32>) {
        (side, side, (0..side * side).map(|i| i ^ seed).collect())
    }

    #[test]
    fn parses_every_image_in_an_xcursor_file() {
        let images = vec![image(2, 1), image(3, 2)];
        assert_eq!(parse_xcursor_images(&xcursor_file(&images)), images);
        assert!(parse_xcursor_images(b"not a cursor").is_empty());
    }

    #[test]
    fn indexes_theme_images_following_inherits() {
        let dir =
            std::env::temp_dir().join(format!("satellite-cursor-theme-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let base = dir.join("base/cursors");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(dir.join("child")).unwrap();
        std::fs::write(
            dir.join("child/index.theme"),
            "[Icon Theme]\nInherits=base\n",
        )
        .unwrap();
        let hand = vec![image(2, 7), image(4, 7)];
        let arrow = vec![image(2, 9)];
        std::fs::write(base.join("hand2"), xcursor_file(&hand)).unwrap();
        std::os::unix::fs::symlink("hand2", base.join("pointer")).unwrap();
        std::fs::write(base.join("left_ptr"), xcursor_file(&arrow)).unwrap();

        let names = ["hand2", "pointer", "left_ptr", "xterm"];
        let index = CursorImageIndex::build(
            &["child".to_owned()],
            names.iter().copied(),
            &[dir.clone()],
            |a, b| (a == "hand2" || a == "pointer") && (b == "hand2" || b == "pointer"),
        );
        std::fs::remove_dir_all(&dir).unwrap();

        for (w, h, pixels) in &hand {
            assert!(matches!(
                index.lookup(image_hash(*w, *h, pixels)),
                Some("hand2" | "pointer")
            ));
        }
        let (w, h, pixels) = &arrow[0];
        assert_eq!(index.lookup(image_hash(*w, *h, pixels)), Some("left_ptr"));
        let (w, h, pixels) = image(2, 123);
        assert_eq!(index.lookup(image_hash(w, h, &pixels)), None);
    }

    #[test]
    fn an_image_shared_by_different_cursors_is_ambiguous() {
        let mut index = CursorImageIndex::default();
        index.insert(1, "left_ptr", |a, b| a == b);
        index.insert(1, "xterm", |a, b| a == b);
        assert_eq!(index.lookup(1), None);
    }

    #[test]
    fn reads_theme_names_from_resources_and_ini() {
        assert_eq!(
            xresource("Xft.dpi:\t96\nXcursor.theme:\tBreeze\n", "Xcursor.theme").as_deref(),
            Some("Breeze")
        );
        assert_eq!(
            ini_value(
                "[Settings]\ngtk-cursor-theme-name=Adwaita\n",
                "gtk-cursor-theme-name"
            )
            .as_deref(),
            Some("Adwaita")
        );
    }
}
