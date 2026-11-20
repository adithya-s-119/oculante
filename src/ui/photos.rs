//! Photos-style library UI: a frosted-glass collection browser, filmstrip
//! viewer and slideshow. Built on top of Oculante's GPU-accelerated canvas.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Datelike, Local, Timelike};
use notan::egui::{self, *};
use notan::prelude::App;
use rayon::prelude::*;

use crate::appstate::OculanteState;
use crate::icons::*;
use crate::ml::{self, MlLabel, MlService, CATEGORIES};
use crate::thumbnails;
use crate::utils::{is_ext_compatible, next_image, toggle_fullscreen};

const TOP_BAR_HEIGHT: f32 = 60.0;
const SIDEBAR_WIDTH: f32 = 240.0;
const OUTER_MARGIN: f32 = 16.0;
const CARD_GAP: f32 = 12.0;
const MAX_SCAN_DEPTH: u8 = 4;
const MAX_SCAN_FILES: usize = 20_000;
/// Above this many photos we no longer start a whole-library AI pass
/// automatically. Visible photos are still tagged lazily, and the user can
/// explicitly ask for the full pass from the sidebar.
const AUTO_ANALYZE_LIMIT: usize = 800;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PhotosView {
    #[default]
    Gallery,
    Viewer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arrow {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GalleryFilter {
    #[default]
    Collection,
    Favorites,
    Recent,
}

#[derive(Clone, Debug)]
pub struct GalleryEntry {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub modified_ms: u64,
    /// Local calendar day, precomputed so date-group headers do not need to
    /// format a date for every photo on every frame.
    pub day_key: i32,
    /// Lower-cased path, used for instant filename search.
    pub search_name: String,
}

#[derive(Clone, Debug)]
struct AnalysisItem {
    path: PathBuf,
    modified_ms: u64,
    tries: u8,
}

struct ScanResult {
    entries: Vec<GalleryEntry>,
    folder: PathBuf,
    filter: GalleryFilter,
    sort_newest: bool,
}

/// State for the Photos gallery and viewer chrome. This is intentionally
/// separate from Oculante's image state so the two can evolve independently.
pub struct GalleryState {
    pub view: PhotosView,
    pub filter: GalleryFilter,
    pub folder: PathBuf,
    pub entries: Vec<GalleryEntry>,
    pub needs_refresh: bool,
    pub query: String,
    pub search_focus: bool,
    pub search_has_focus: bool,
    pub category: Option<&'static str>,
    pub tile_scale: f32,
    pub sort_newest: bool,
    pub selected: Option<PathBuf>,
    pub scroll_selected: bool,
    pub columns: usize,
    pub backdrop: Option<TextureHandle>,
    backdrop_key: Option<(Option<PathBuf>, u32, u32)>,
    pub backdrop_dirty: bool,
    pub slideshow: bool,
    pub slideshow_interval: f32,
    pub last_advance: Option<Instant>,
    pub slideshow_fullscreen: bool,
    pub slideshow_needs_fit: bool,
    pub folder_picker_open: bool,
    pub picker_path: PathBuf,
    picker_dirs: Vec<PathBuf>,
    picker_for: Option<PathBuf>,
    analysis_queue: VecDeque<AnalysisItem>,
    /// Visible, already-thumbnailed photos that can be classified immediately.
    analysis_candidates: Vec<(PathBuf, u64)>,
    pub analysis_started: bool,
    pub analysis_all_requested: bool,
    last_analysis_tick: Instant,
    scanning: bool,
    scan_rx: Option<Receiver<ScanResult>>,
    scan_tag: Option<(PathBuf, GalleryFilter)>,
    /// Recently used egui image textures, keyed by their `file://` URI. The
    /// egui image loader has no eviction, which otherwise means browsing a
    /// 20k-photo drive can consume gigabytes of GPU memory.
    photo_textures: HashMap<String, u64>,
    photo_texture_seq: u64,
    pub filmstrip_index: Option<usize>,
}

impl Default for GalleryState {
    fn default() -> Self {
        let folder = dirs::picture_dir()
            .filter(|p| p.is_dir())
            .or_else(|| dirs::home_dir())
            .unwrap_or_default();
        Self {
            view: PhotosView::Gallery,
            filter: GalleryFilter::Collection,
            folder,
            entries: Vec::new(),
            needs_refresh: true,
            query: String::new(),
            search_focus: false,
            search_has_focus: false,
            category: None,
            tile_scale: 0.42,
            sort_newest: true,
            selected: None,
            scroll_selected: false,
            columns: 4,
            backdrop: None,
            backdrop_key: None,
            backdrop_dirty: true,
            slideshow: false,
            slideshow_interval: 5.0,
            last_advance: None,
            slideshow_fullscreen: false,
            slideshow_needs_fit: false,
            folder_picker_open: false,
            picker_path: PathBuf::new(),
            picker_dirs: Vec::new(),
            picker_for: None,
            analysis_queue: VecDeque::new(),
            analysis_candidates: Vec::new(),
            analysis_started: false,
            analysis_all_requested: false,
            last_analysis_tick: Instant::now(),
            scanning: false,
            scan_rx: None,
            scan_tag: None,
            photo_textures: HashMap::new(),
            photo_texture_seq: 0,
            filmstrip_index: None,
        }
    }
}

impl GalleryState {
    pub fn folder_name(&self) -> String {
        self.folder
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.folder.display().to_string())
    }

    pub fn title(&self) -> &'static str {
        match self.filter {
            GalleryFilter::Collection => "Collection",
            GalleryFilter::Favorites => "Favourites",
            GalleryFilter::Recent => "Recent",
        }
    }

    /// Start scanning the current folder in the background. The UI keeps
    /// rendering (and remains interactive) while a slow USB drive is indexed.
    pub fn start_scan(&mut self, favourites: &HashSet<PathBuf>, recent: &VecDeque<PathBuf>) {
        if self.scanning {
            self.needs_refresh = true;
            return;
        }
        let folder = self.folder.clone();
        let filter = self.filter;
        let newest = self.sort_newest;
        let favourites = favourites.clone();
        let recent = recent.clone();
        let (tx, rx) = mpsc::channel();

        self.scanning = true;
        self.needs_refresh = false;
        self.scan_rx = Some(rx);
        self.scan_tag = Some((folder.clone(), filter));
        self.analysis_queue.clear();
        self.analysis_candidates.clear();
        self.analysis_started = false;
        self.analysis_all_requested = false;

        let spawned = std::thread::Builder::new()
            .name("photos-scan".into())
            .spawn(move || {
                let entries = scan_entries(&folder, filter, &favourites, &recent, newest);
                let _ = tx.send(ScanResult {
                    entries,
                    folder,
                    filter,
                    sort_newest: newest,
                });
            });
        if spawned.is_err() {
            self.scanning = false;
            self.scan_rx = None;
            self.scan_tag = None;
            self.needs_refresh = true;
        }
    }

    /// Poll a running folder scan. This never blocks on drive I/O.
    pub fn poll_scan(&mut self, ctx: &Context) {
        let mut finished = None;
        if let Some(rx) = self.scan_rx.take() {
            match rx.try_recv() {
                Ok(result) => finished = Some(result),
                Err(mpsc::TryRecvError::Empty) => self.scan_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
        }

        if let Some(result) = finished {
            let refresh_again = self.needs_refresh;
            let tagged_view = self
                .scan_tag
                .as_ref()
                .map(|(folder, filter)| folder == &result.folder && *filter == result.filter)
                .unwrap_or(false);
            self.scanning = false;
            self.scan_tag = None;
            let same_view =
                tagged_view && self.folder == result.folder && self.filter == result.filter;
            if same_view {
                self.entries = result.entries;
                if result.sort_newest != self.sort_newest {
                    self.sort_entries();
                }
                self.needs_refresh = refresh_again;
                self.analysis_queue.clear();
                self.analysis_candidates.clear();
                self.analysis_started = false;
                self.analysis_all_requested = false;
                if let Some(sel) = &self.selected {
                    if !self.entries.iter().any(|e| &e.path == sel) {
                        self.selected = None;
                    }
                }
            } else {
                self.needs_refresh = true;
            }
        }

        if self.scanning {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    fn sort_entries(&mut self) {
        sort_entries(&mut self.entries, self.sort_newest);
    }

    pub fn resort(&mut self) {
        self.sort_entries();
        self.analysis_queue.clear();
        self.analysis_candidates.clear();
        self.analysis_started = false;
        self.analysis_all_requested = false;
    }

    /// Indices into `entries` matching the current search and category filter.
    pub fn visible_indices(&self, ml: &MlService) -> Vec<usize> {
        let q = self.query.trim().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                if let Some(cat) = self.category {
                    let Some(label) = ml.label_for(&entry.path, entry.modified_ms) else {
                        return false;
                    };
                    if !category_matches(cat, &label.category) {
                        return false;
                    }
                }
                if q.is_empty() {
                    return true;
                }
                if entry.search_name.contains(&q) {
                    return true;
                }
                if let Some(label) = ml.label_for(&entry.path, entry.modified_ms) {
                    if label.label.to_lowercase().contains(&q) {
                        return true;
                    }
                    if ml::category_name(&label.category)
                        .to_lowercase()
                        .contains(&q)
                        || label.category.contains(&q)
                    {
                        return true;
                    }
                    if label
                        .top
                        .iter()
                        .any(|(name, _)| name.to_lowercase().contains(&q))
                    {
                        return true;
                    }
                }
                false
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn category_counts(&self, ml: &MlService) -> HashMap<&'static str, usize> {
        let mut counts: HashMap<&'static str, usize> = HashMap::new();
        for entry in &self.entries {
            if let Some(label) = ml.label_for(&entry.path, entry.modified_ms) {
                *counts
                    .entry(ml::category_static(&label.category))
                    .or_default() += 1;
            }
        }
        counts
    }

    pub fn analyzed_count(&self, ml: &MlService) -> usize {
        self.entries
            .iter()
            .filter(|e| ml.is_analyzed(&e.path, e.modified_ms))
            .count()
    }
}

/// Everything the Photos chrome needs to know about the current colour scheme.
#[derive(Clone, Copy)]
struct Palette {
    dark: bool,
    text: Color32,
    muted: Color32,
    faint: Color32,
    glass: Color32,
    glass_strong: Color32,
    stroke: Color32,
    base: Color32,
    accent: Color32,
    accent_soft: Color32,
    heart: Color32,
}

fn palette(visuals: &Visuals, state: &OculanteState) -> Palette {
    let dark = visuals.dark_mode;
    let accent = Color32::from_rgb(
        state.persistent_settings.accent_color[0],
        state.persistent_settings.accent_color[1],
        state.persistent_settings.accent_color[2],
    );
    Palette {
        dark,
        text: if dark {
            Color32::from_rgb(240, 245, 252)
        } else {
            Color32::from_rgb(18, 24, 34)
        },
        muted: if dark {
            Color32::from_rgb(154, 166, 184)
        } else {
            Color32::from_rgb(92, 104, 122)
        },
        faint: if dark {
            Color32::from_rgba_unmultiplied(255, 255, 255, 70)
        } else {
            Color32::from_rgba_unmultiplied(0, 0, 0, 70)
        },
        glass: if dark {
            Color32::from_rgba_unmultiplied(17, 21, 30, 196)
        } else {
            Color32::from_rgba_unmultiplied(255, 255, 255, 196)
        },
        glass_strong: if dark {
            Color32::from_rgba_unmultiplied(14, 17, 25, 232)
        } else {
            Color32::from_rgba_unmultiplied(255, 255, 255, 235)
        },
        stroke: if dark {
            Color32::from_rgba_unmultiplied(255, 255, 255, 24)
        } else {
            Color32::from_rgba_unmultiplied(0, 0, 0, 24)
        },
        base: if dark {
            Color32::from_rgb(8, 11, 18)
        } else {
            Color32::from_rgb(235, 241, 249)
        },
        accent,
        accent_soft: Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 42),
        heart: Color32::from_rgb(255, 102, 148),
    }
}

fn bold_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

fn paint_shadow(painter: &Painter, rect: Rect, radius: u8, strength: u8) {
    for i in 0..4 {
        let grow = 2.0 + i as f32 * 3.0;
        let y = 3.0 + i as f32 * 2.0;
        painter.rect_filled(
            rect.translate(vec2(0.0, y)).expand(grow),
            CornerRadius::same(radius.saturating_add(4)),
            Color32::from_black_alpha(strength.saturating_sub(i as u8 * 4)),
        );
    }
}

fn paint_glass(painter: &Painter, rect: Rect, radius: u8, palette: Palette, strong: bool) {
    paint_shadow(painter, rect, radius, 34);
    painter.rect_filled(
        rect,
        CornerRadius::same(radius),
        if strong {
            palette.glass_strong
        } else {
            palette.glass
        },
    );
    painter.rect_stroke(
        rect,
        CornerRadius::same(radius),
        Stroke::new(1.0_f32, palette.stroke),
        StrokeKind::Inside,
    );
    // A thin highlight along the top edge sells the glass effect.
    let y = rect.top() + 0.75;
    painter.line_segment(
        [
            pos2(rect.left() + radius as f32, y),
            pos2(rect.right() - radius as f32, y),
        ],
        Stroke::new(
            1.0_f32,
            if palette.dark {
                Color32::from_white_alpha(34)
            } else {
                Color32::from_white_alpha(190)
            },
        ),
    );
}

fn glass_frame(palette: Palette, radius: u8) -> Frame {
    Frame::new()
        .fill(palette.glass_strong)
        .stroke(Stroke::new(1.0_f32, palette.stroke))
        .corner_radius(CornerRadius::same(radius))
        .inner_margin(Margin::same(14))
}

fn icon_button(ui: &mut Ui, palette: Palette, icon: &str, tip: &str) -> Response {
    icon_button_sized(ui, palette, icon, tip, 18.0, 34.0, false)
}

fn icon_button_sized(
    ui: &mut Ui,
    palette: Palette,
    icon: &str,
    tip: &str,
    icon_size: f32,
    size: f32,
    active: bool,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id, response.hovered(), 0.12);
    if active {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), palette.accent_soft);
    } else if hover > 0.01 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(10),
            if palette.dark {
                Color32::from_white_alpha((28.0 * hover) as u8)
            } else {
                Color32::from_black_alpha((18.0 * hover) as u8)
            },
        );
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        FontId::proportional(icon_size),
        if active { palette.accent } else { palette.text },
    );
    response.on_hover_text(tip)
}

fn pill_button(ui: &mut Ui, palette: Palette, text: impl Into<String>, filled: bool) -> Response {
    let text = text.into();
    let button = egui::Button::new(RichText::new(text).size(13.0).color(if filled {
        Color32::from_rgb(7, 12, 18)
    } else {
        palette.text
    }))
    .fill(if filled {
        palette.accent
    } else if palette.dark {
        Color32::from_rgba_unmultiplied(255, 255, 255, 14)
    } else {
        Color32::from_rgba_unmultiplied(0, 0, 0, 10)
    })
    .stroke(if filled {
        Stroke::NONE
    } else {
        Stroke::new(1.0_f32, palette.stroke)
    })
    .corner_radius(CornerRadius::same(16))
    .min_size(vec2(0.0, 32.0));
    ui.add(button)
}

fn nav_item(
    ui: &mut Ui,
    palette: Palette,
    icon: &str,
    label: &str,
    count: Option<usize>,
    selected: bool,
) -> Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, 40.0), Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id, response.hovered(), 0.12);
    if selected {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), palette.accent_soft);
        ui.painter().rect_filled(
            Rect::from_min_size(rect.left_top() + vec2(0.0, 9.0), vec2(3.0, 22.0)),
            CornerRadius::same(2),
            palette.accent,
        );
    } else if hover > 0.01 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(10),
            Color32::from_white_alpha((18.0 * hover) as u8),
        );
    }
    ui.painter().text(
        pos2(rect.left() + 30.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon,
        FontId::proportional(17.0),
        if selected {
            palette.accent
        } else {
            palette.muted
        },
    );
    ui.painter().text(
        pos2(rect.left() + 46.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(14.5),
        palette.text,
    );
    if let Some(count) = count {
        ui.painter().text(
            pos2(rect.right() - 12.0, rect.center().y),
            Align2::RIGHT_CENTER,
            number(count),
            FontId::proportional(11.5),
            palette.muted,
        );
    }
    response
}

fn photo_count(n: usize) -> String {
    format!("{} {}", number(n), if n == 1 { "photo" } else { "photos" })
}

fn number(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn day_label(time: SystemTime) -> String {
    let now = Local::now();
    let dt: DateTime<Local> = time.into();
    let date = dt.date_naive();
    let today = now.date_naive();
    if date == today {
        "Today".into()
    } else if today.pred_opt() == Some(date) {
        "Yesterday".into()
    } else if date.year() == today.year() {
        dt.format("%B").to_string()
    } else {
        dt.format("%B %Y").to_string()
    }
}

fn full_date(time: SystemTime) -> String {
    let dt: DateTime<Local> = time.into();
    format!(
        "{}, {} {}, {}",
        dt.format("%A"),
        dt.format("%B"),
        dt.day(),
        dt.year()
    )
}

fn time_label(time: SystemTime) -> String {
    let dt: DateTime<Local> = time.into();
    let hour = dt.hour();
    let minute = dt.minute();
    format!("{hour:02}:{minute:02}")
}

fn category_matches(selected: &str, category: &str) -> bool {
    if selected == "animals" {
        matches!(category, "cats" | "dogs" | "birds" | "insects" | "animals")
    } else {
        selected == category
    }
}

fn scan_entries(
    folder: &Path,
    filter: GalleryFilter,
    favourites: &HashSet<PathBuf>,
    recent: &VecDeque<PathBuf>,
    sort_newest: bool,
) -> Vec<GalleryEntry> {
    let mut paths: Vec<PathBuf> = match filter {
        GalleryFilter::Collection => {
            let mut found = Vec::new();
            collect_images(folder, 0, &mut found);
            found
        }
        GalleryFilter::Favorites => favourites.iter().cloned().collect(),
        GalleryFilter::Recent => recent.iter().cloned().collect(),
    };
    paths.sort();
    paths.dedup();

    let mut entries: Vec<GalleryEntry> = paths
        .into_par_iter()
        .filter(|p| p.is_file())
        .map(|path| {
            let meta = std::fs::metadata(&path).ok();
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let modified_ms = meta.as_ref().map(ml::modified_ms).unwrap_or_default();
            let day_key = {
                let dt: DateTime<Local> = modified.into();
                dt.date_naive().num_days_from_ce()
            };
            let search_name = path.to_string_lossy().to_lowercase();
            GalleryEntry {
                path,
                modified,
                modified_ms,
                day_key,
                search_name,
            }
        })
        .collect();
    sort_entries(&mut entries, sort_newest);
    entries
}

fn sort_entries(entries: &mut [GalleryEntry], newest: bool) {
    if newest {
        entries.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| a.path.cmp(&b.path))
        });
    } else {
        entries.sort_by(|a, b| {
            a.modified
                .cmp(&b.modified)
                .then_with(|| a.path.cmp(&b.path))
        });
    }
}

fn collect_images(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
    if depth > MAX_SCAN_DEPTH || out.len() >= MAX_SCAN_FILES {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dirs = Vec::new();
    for entry in read_dir.flatten() {
        if out.len() >= MAX_SCAN_FILES {
            break;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name.starts_with('$') {
            continue;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            dirs.push(path);
        } else if ft.is_file() && is_ext_compatible(&path) {
            out.push(path);
        }
    }
    dirs.sort();
    for d in dirs {
        collect_images(&d, depth + 1, out);
    }
}

fn open_photo(state: &mut OculanteState, path: &Path) {
    // Keep the filmstrip and arrow keys in the same order the user sees.
    let visible = state
        .gallery
        .visible_indices(&state.ml)
        .into_iter()
        .filter_map(|i| state.gallery.entries.get(i).map(|e| e.path.clone()))
        .collect::<Vec<_>>();
    if visible.is_empty() {
        return;
    }
    let index = visible.iter().position(|p| p == path).unwrap_or_default();
    state.scrubber.entries = visible;
    state.scrubber.index = index;
    state.scrubber.fixed_paths = true;
    state.scrubber.wrap = true;
    if state.ml.enabled {
        if let Some(thumbnail) = state.thumbnails.existing_analysis(path) {
            state
                .ml
                .request(path, &thumbnail, ml::modified_ms_for(path));
        }
    }
    state.current_path = Some(path.to_path_buf());
    state.current_image = None;
    state.is_loaded = false;
    state.player.load(path);
    state.gallery.selected = Some(path.to_path_buf());
    state.gallery.view = PhotosView::Viewer;
    state.gallery.slideshow = false;
    state.gallery.last_advance = None;
    state.gallery.filmstrip_index = None;
    state.gallery.backdrop_dirty = true;
    if let Some(parent) = path.parent() {
        state.volatile_settings.last_open_directory = parent.to_path_buf();
    }
}

fn open_folder(state: &mut OculanteState, folder: PathBuf) {
    state.gallery.folder = folder.clone();
    state.gallery.filter = GalleryFilter::Collection;
    state.gallery.category = None;
    state.gallery.query.clear();
    state.gallery.needs_refresh = true;
    state.gallery.entries.clear();
    state.gallery.analysis_started = false;
    state.gallery.analysis_all_requested = false;
    state.gallery.analysis_queue.clear();
    state.gallery.analysis_candidates.clear();
    state.gallery.selected = None;
    state.volatile_settings.last_open_directory = folder.clone();
    state.volatile_settings.folder_bookmarks.insert(folder);
}

fn start_folder_picker(state: &mut OculanteState) {
    state.gallery.folder_picker_open = true;
    state.gallery.picker_path = state.gallery.folder.clone();
    state.gallery.picker_for = None;
}

fn browse_for_files(state: &mut OculanteState) {
    #[cfg(feature = "file_open")]
    {
        crate::filebrowser::browse_for_image_path(state);
    }
    #[cfg(not(feature = "file_open"))]
    {
        state.filebrowser_id = Some("OPEN".into());
        state.redraw = true;
    }
}

fn set_favorite(state: &mut OculanteState, path: &Path) {
    let inserted = state
        .volatile_settings
        .favourite_images
        .insert(path.to_path_buf());
    if !inserted {
        state.volatile_settings.favourite_images.remove(path);
    }
    if state.gallery.filter == GalleryFilter::Favorites {
        state.gallery.needs_refresh = true;
    }
}

fn is_favorite(state: &OculanteState, path: &Path) -> bool {
    state.volatile_settings.favourite_images.contains(path)
}

fn ensure_backdrop(ctx: &Context, state: &mut OculanteState) {
    let key = state
        .current_image
        .as_ref()
        .map(|i| (state.current_path.clone(), i.width(), i.height()));
    if !state.gallery.backdrop_dirty
        && state.gallery.backdrop.is_some()
        && state.gallery.backdrop_key == key
    {
        return;
    }

    let (size, pixels) = if let Some(image) = &state.current_image {
        // A small downscale plus a blur gives a beautifully soft, very cheap
        // backdrop. It also avoids needing window transparency/acrylic.
        let small = image.thumbnail(320, 200).to_rgba8();
        let blurred = image::imageops::blur(&small, 4.5);
        let size = [blurred.width() as usize, blurred.height() as usize];
        let pixels = blurred
            .pixels()
            .map(|p| Color32::from_rgba_unmultiplied(p[0], p[1], p[2], 255))
            .collect::<Vec<_>>();
        (size, pixels)
    } else {
        default_backdrop()
    };

    let image = ColorImage { size, pixels };
    state.gallery.backdrop =
        Some(ctx.load_texture("photos-backdrop", image, TextureOptions::LINEAR));
    state.gallery.backdrop_key = key;
    state.gallery.backdrop_dirty = false;
}

fn default_backdrop() -> ([usize; 2], Vec<Color32>) {
    let w = 320usize;
    let h = 180usize;
    let mut pixels = Vec::with_capacity(w * h);
    let blobs = [
        (0.18f32, 0.22f32, 0.55f32, [44.0, 120.0, 255.0]),
        (0.82f32, 0.34f32, 0.48f32, [148.0, 86.0, 255.0]),
        (0.52f32, 0.95f32, 0.62f32, [32.0, 188.0, 196.0]),
    ];
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let vignette = 1.0 - ((fx - 0.5).powi(2) + (fy - 0.5).powi(2)) * 0.9;
            let mut c = [10.0, 14.0, 23.0];
            for (bx, by, radius, color) in blobs {
                let d = ((fx - bx).powi(2) + (fy - by).powi(2)).sqrt();
                let strength = (1.0 - d / radius).max(0.0).powf(2.0) * 0.62;
                for i in 0..3 {
                    c[i] += color[i] * strength;
                }
            }
            let noise = (((x * 37 + y * 17) % 19) as f32 - 9.0) * 0.18;
            pixels.push(Color32::from_rgb(
                (c[0] * vignette + noise).clamp(0.0, 255.0) as u8,
                (c[1] * vignette + noise).clamp(0.0, 255.0) as u8,
                (c[2] * vignette + noise).clamp(0.0, 255.0) as u8,
            ));
        }
    }
    ([w, h], pixels)
}

fn paint_backdrop(ui: &Ui, state: &OculanteState, palette: Palette) {
    let rect = ui.max_rect();
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, palette.base);
    if let Some(texture) = &state.gallery.backdrop {
        painter.image(
            texture.id(),
            rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            if palette.dark {
                Color32::from_white_alpha(172)
            } else {
                Color32::from_white_alpha(120)
            },
        );
    }

    // Darken towards the bottom so captions and the grid stay legible.
    let steps = 28;
    for i in 0..steps {
        let t = i as f32 / steps as f32;
        let y0 = rect.top() + rect.height() * t;
        let y1 = rect.top() + rect.height() * (t + 1.0 / steps as f32);
        let alpha = if palette.dark {
            (30.0 + t * 150.0) as u8
        } else {
            (8.0 + t * 60.0) as u8
        };
        painter.rect_filled(
            Rect::from_min_max(pos2(rect.left(), y0), pos2(rect.right(), y1)),
            0.0,
            if palette.dark {
                Color32::from_black_alpha(alpha)
            } else {
                Color32::from_white_alpha(alpha)
            },
        );
    }

    let glow = Color32::from_rgba_unmultiplied(
        palette.accent.r(),
        palette.accent.g(),
        palette.accent.b(),
        if palette.dark { 26 } else { 20 },
    );
    painter.circle_filled(pos2(rect.right() - 120.0, rect.top() + 80.0), 270.0, glow);
    painter.circle_filled(
        pos2(rect.left() + 80.0, rect.bottom() - 60.0),
        220.0,
        Color32::from_rgba_unmultiplied(
            palette.heart.r(),
            palette.heart.g(),
            palette.heart.b(),
            if palette.dark { 18 } else { 14 },
        ),
    );
}

/// Entry point called from main's draw loop.
pub fn photos_ui(ctx: &Context, app: &mut App, state: &mut OculanteState) {
    state.gallery.photo_texture_seq = state.gallery.photo_texture_seq.wrapping_add(1);
    match state.gallery.view {
        PhotosView::Gallery => gallery_ui(ctx, app, state),
        PhotosView::Viewer => viewer_ui(ctx, app, state),
    }
    evict_photo_textures(ctx, state);
}

const MAX_PHOTO_TEXTURES: usize = 180;
const RETAIN_PHOTO_TEXTURES: usize = 130;

fn evict_photo_textures(ctx: &Context, state: &mut OculanteState) {
    if state.gallery.photo_textures.len() <= MAX_PHOTO_TEXTURES {
        return;
    }
    let current = state.gallery.photo_texture_seq;
    let mut stale: Vec<(String, u64)> = state
        .gallery
        .photo_textures
        .iter()
        .filter(|(_, seq)| **seq != current)
        .map(|(uri, seq)| (uri.clone(), *seq))
        .collect();
    stale.sort_by_key(|(_, seq)| *seq);
    let remove = state
        .gallery
        .photo_textures
        .len()
        .saturating_sub(RETAIN_PHOTO_TEXTURES)
        .min(stale.len());
    for (uri, _) in stale.into_iter().take(remove) {
        ctx.forget_image(uri.as_str());
        state.gallery.photo_textures.remove(&uri);
    }
}

fn gallery_ui(ctx: &Context, _app: &mut App, state: &mut OculanteState) {
    state.gallery.poll_scan(ctx);
    if state.gallery.needs_refresh && !state.gallery.scanning {
        let favourites = state.volatile_settings.favourite_images.clone();
        let recent = state.volatile_settings.recent_images.clone();
        state.thumbnails.clear_known();
        state.gallery.start_scan(&favourites, &recent);
        ctx.request_repaint_after(Duration::from_millis(120));
    }

    // Keyboard: Escape clears filters, "/" or Ctrl+F focuses search, Enter
    // opens the selected photo.
    let escape = ctx.input(|i| i.key_pressed(Key::Escape));
    let focus_search = ctx.input(|i| {
        (i.modifiers.command && i.key_pressed(Key::F))
            || (!i.modifiers.command && i.key_pressed(Key::Slash))
    });
    let enter = ctx.input(|i| i.key_pressed(Key::Enter));
    let text_editing = state.gallery.search_has_focus || state.settings_enabled;
    let arrow = ctx.input(|i| {
        if i.key_pressed(Key::ArrowLeft) {
            Some(Arrow::Left)
        } else if i.key_pressed(Key::ArrowRight) {
            Some(Arrow::Right)
        } else if i.key_pressed(Key::ArrowUp) {
            Some(Arrow::Up)
        } else if i.key_pressed(Key::ArrowDown) {
            Some(Arrow::Down)
        } else {
            None
        }
    });
    if focus_search && !state.settings_enabled {
        state.gallery.search_focus = true;
    }
    if escape {
        if state.gallery.folder_picker_open {
            state.gallery.folder_picker_open = false;
        } else if !state.gallery.query.is_empty() {
            state.gallery.query.clear();
            state.gallery.search_focus = true;
        } else if state.gallery.category.is_some() {
            state.gallery.category = None;
        }
    }
    if let Some(arrow) = arrow.filter(|_| !text_editing) {
        let visible = state.gallery.visible_indices(&state.ml);
        if !visible.is_empty() {
            let current = state
                .gallery
                .selected
                .as_deref()
                .and_then(|selected| {
                    visible
                        .iter()
                        .position(|i| state.gallery.entries[*i].path == selected)
                })
                .unwrap_or(0);
            let columns = state.gallery.columns.max(1);
            let next = match arrow {
                Arrow::Left => current.saturating_sub(1),
                Arrow::Right => (current + 1).min(visible.len() - 1),
                Arrow::Up => current.saturating_sub(columns),
                Arrow::Down => (current + columns).min(visible.len() - 1),
            };
            state.gallery.selected = Some(state.gallery.entries[visible[next]].path.clone());
            state.gallery.scroll_selected = true;
        }
    }
    if enter && !text_editing && !state.gallery.folder_picker_open {
        if let Some(path) = state.gallery.selected.clone() {
            open_photo(state, &path);
        }
    }

    ensure_backdrop(ctx, state);
    state.gallery.analysis_candidates.clear();
    let palette = palette(&ctx.style().visuals, state);

    egui::CentralPanel::default()
        .frame(Frame::NONE)
        .show(ctx, |ui| {
            paint_backdrop(ui, state, palette);
            let screen = ui.max_rect();
            let inner = screen.shrink(OUTER_MARGIN);
            let top_rect = Rect::from_min_size(inner.min, vec2(inner.width(), TOP_BAR_HEIGHT));

            ui.allocate_new_ui(
                UiBuilder::new()
                    .max_rect(top_rect)
                    .layout(Layout::left_to_right(Align::Center)),
                |ui| {
                    top_bar(ui, state, palette);
                },
            );

            let body_top = top_rect.bottom() + CARD_GAP;
            let body_height = (inner.bottom() - body_top).max(100.0);
            let side_rect = Rect::from_min_size(
                pos2(inner.left(), body_top),
                vec2(SIDEBAR_WIDTH, body_height),
            );
            let main_rect = Rect::from_min_size(
                pos2(side_rect.right() + CARD_GAP, body_top),
                vec2(
                    (inner.right() - side_rect.right() - CARD_GAP).max(100.0),
                    body_height,
                ),
            );

            ui.allocate_new_ui(UiBuilder::new().max_rect(side_rect), |ui| {
                sidebar(ui, state, palette);
            });
            ui.allocate_new_ui(UiBuilder::new().max_rect(main_rect), |ui| {
                collection(ui, state, palette);
            });
        });

    folder_picker(ctx, state);
    process_analysis(ctx, state);
}

fn top_bar(ui: &mut Ui, state: &mut OculanteState, palette: Palette) {
    paint_glass(
        ui.painter(),
        ui.max_rect().shrink2(vec2(0.0, 2.0)),
        18,
        palette,
        false,
    );
    let inner = ui.max_rect().shrink2(vec2(18.0, 10.0));
    ui.allocate_new_ui(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            paint_logo(ui, palette);
            ui.add_space(9.0);
            ui.label(
                RichText::new("Photos")
                    .size(18.0)
                    .family(FontFamily::Name("bold".into()))
                    .color(palette.text),
            );
            ui.add_space(22.0);

            // Search box.
            let frame = Frame::new()
                .fill(if palette.dark {
                    Color32::from_rgba_unmultiplied(255, 255, 255, 14)
                } else {
                    Color32::from_rgba_unmultiplied(0, 0, 0, 8)
                })
                .stroke(if state.gallery.search_has_focus {
                    Stroke::new(1.5_f32, palette.accent)
                } else {
                    Stroke::new(1.0_f32, palette.stroke)
                })
                .corner_radius(CornerRadius::same(17))
                .inner_margin(Margin::symmetric(12, 2));
            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new(SEARCH).size(13.0).color(palette.muted));
                    let response = ui.add(
                        TextEdit::singleline(&mut state.gallery.query)
                            .hint_text("Search photos or categories…")
                            .desired_width(250.0)
                            .frame(false)
                            .text_color(palette.text),
                    );
                    state.gallery.search_has_focus = response.has_focus();
                    if state.gallery.search_focus {
                        response.request_focus();
                        state.gallery.search_focus = false;
                    }
                    if !state.gallery.query.is_empty()
                        && ui
                            .add(
                                egui::Button::new(RichText::new(X).size(11.0).color(palette.muted))
                                    .frame(false),
                            )
                            .on_hover_text("Clear search")
                            .clicked()
                    {
                        state.gallery.query.clear();
                        state.gallery.search_focus = true;
                    }
                });
            });

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, palette, GEAR, "Preferences").clicked() {
                    state.settings_enabled = true;
                }
                ui.add_space(4.0);
                if icon_button(ui, palette, SYNC, "Refresh library").clicked() {
                    state.gallery.needs_refresh = true;
                    state.gallery.analysis_started = false;
                    state.gallery.analysis_queue.clear();
                }
                ui.add_space(4.0);
                if icon_button(ui, palette, IMAGE, "Open photos").clicked() {
                    browse_for_files(state);
                }
                ui.add_space(4.0);
                if pill_button(ui, palette, format!("{FOLDERFILL}  Open folder"), false)
                    .on_hover_text("Choose a folder to browse")
                    .clicked()
                {
                    start_folder_picker(state);
                }
                ui.add_space(8.0);
                let analyzing = state.ml.enabled
                    && state.ml.error().is_none()
                    && (state.ml.pending_len() > 0 || !state.gallery.analysis_queue.is_empty());
                if analyzing {
                    let (rect, _) = ui.allocate_exact_size(vec2(86.0, 24.0), Sense::hover());
                    ui.painter()
                        .rect_filled(rect, CornerRadius::same(12), palette.accent_soft);
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "AI analysing…",
                        FontId::proportional(11.0),
                        palette.accent,
                    );
                }
            });
        },
    );
}

fn paint_logo(ui: &mut Ui, palette: Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(
        Rect::from_min_size(rect.left_top() + vec2(2.0, 2.0), vec2(21.0, 21.0)),
        CornerRadius::same(7),
        Color32::from_rgba_unmultiplied(96, 205, 255, 210),
    );
    painter.rect_filled(
        Rect::from_min_size(rect.left_top() + vec2(9.0, 9.0), vec2(19.0, 19.0)),
        CornerRadius::same(7),
        Color32::from_rgba_unmultiplied(167, 113, 255, 190),
    );
    painter.circle_filled(rect.center(), 6.5, Color32::from_white_alpha(230));
    painter.circle_filled(
        rect.center() + vec2(2.2, -2.2),
        2.0,
        palette.accent.gamma_multiply(0.9),
    );
}

fn sidebar(ui: &mut Ui, state: &mut OculanteState, palette: Palette) {
    paint_glass(ui.painter(), ui.max_rect(), 18, palette, false);
    let inner = ui.max_rect().shrink2(vec2(12.0, 16.0));
    ui.allocate_new_ui(UiBuilder::new().max_rect(inner), |ui| {
        let card_h = ai_card_reserved_height(state);
        let list_h = (ui.available_height() - card_h - 8.0).max(120.0);
        ScrollArea::vertical()
            .id_salt("photos-sidebar")
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                sidebar_library(ui, state, palette);
            });
        ui.add_space(6.0);
        ai_card(ui, state, palette);
    });
}

fn ai_card_reserved_height(state: &OculanteState) -> f32 {
    let total = state.gallery.entries.len();
    if state.ml.enabled
        && state.ml.error().is_none()
        && !state.gallery.scanning
        && total > AUTO_ANALYZE_LIMIT
        && !state.gallery.analysis_all_requested
    {
        156.0
    } else {
        132.0
    }
}

fn sidebar_library(ui: &mut Ui, state: &mut OculanteState, palette: Palette) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(
            RichText::new("LIBRARY")
                .size(10.5)
                .color(palette.muted)
                .family(FontFamily::Name("bold".into())),
        );
        ui.add_space(7.0);

        let favorites_count = state.volatile_settings.favourite_images.len();
        let recent_count = state.volatile_settings.recent_images.len();
        if nav_item(
            ui,
            palette,
            HOUSE,
            "Collection",
            Some(state.gallery.entries.len()),
            state.gallery.filter == GalleryFilter::Collection,
        )
        .clicked()
        {
            state.gallery.filter = GalleryFilter::Collection;
            state.gallery.needs_refresh = true;
            state.gallery.entries.clear();
            state.gallery.category = None;
        }
        if nav_item(
            ui,
            palette,
            HEARTFILL,
            "Favourites",
            Some(favorites_count),
            state.gallery.filter == GalleryFilter::Favorites,
        )
        .clicked()
        {
            state.gallery.filter = GalleryFilter::Favorites;
            state.gallery.needs_refresh = true;
            state.gallery.entries.clear();
            state.gallery.category = None;
        }
        if nav_item(
            ui,
            palette,
            CLOCK,
            "Recent",
            Some(recent_count),
            state.gallery.filter == GalleryFilter::Recent,
        )
        .clicked()
        {
            state.gallery.filter = GalleryFilter::Recent;
            state.gallery.needs_refresh = true;
            state.gallery.entries.clear();
            state.gallery.category = None;
        }

        ui.add_space(16.0);
        ui.label(
            RichText::new("FOLDERS")
                .size(10.5)
                .color(palette.muted)
                .family(FontFamily::Name("bold".into())),
        );
        ui.add_space(6.0);

        let mut folders: Vec<(String, PathBuf)> = Vec::new();
        if let Some(p) = dirs::picture_dir() {
            folders.push(("Pictures".to_string(), p));
        }
        if let Some(p) = dirs::desktop_dir() {
            folders.push(("Desktop".to_string(), p));
        }
        if let Some(p) = dirs::download_dir() {
            folders.push(("Downloads".to_string(), p));
        }
        for bookmark in &state.volatile_settings.folder_bookmarks {
            let name = bookmark
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| bookmark.display().to_string());
            if !folders.iter().any(|(_, p)| same_path(p, bookmark)) {
                folders.push((name, bookmark.clone()));
            }
        }
        // Keep the folder currently on screen reachable from the sidebar,
        // even if it was opened from the command line or a file drop.
        let current = state.gallery.folder.clone();
        if current.is_dir() && !folders.iter().any(|(_, p)| same_path(p, &current)) {
            let name = current
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| current.display().to_string());
            folders.insert(0, (name, current));
        }

        let mut chosen_folder = None;
        for (name, path) in folders.iter().take(8) {
            if folder_item(
                ui,
                palette,
                name,
                &path.display().to_string(),
                same_path(&state.gallery.folder, path),
            )
            .clicked()
            {
                chosen_folder = Some(path.clone());
            }
        }
        if ui
            .add(
                egui::Button::new(
                    RichText::new(format!("{PLUS}  Add folder"))
                        .size(12.5)
                        .color(palette.muted),
                )
                .frame(false),
            )
            .clicked()
        {
            start_folder_picker(state);
        }
        if let Some(path) = chosen_folder {
            open_folder(state, path);
        }
    });
}

fn same_path(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

fn folder_item(
    ui: &mut Ui,
    palette: Palette,
    name: &str,
    tooltip: &str,
    selected: bool,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id, response.hovered(), 0.12);
    if selected {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(8), palette.accent_soft);
    } else if hover > 0.01 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(8),
            Color32::from_white_alpha((16.0 * hover) as u8),
        );
    }
    ui.painter().text(
        pos2(rect.left() + 16.0, rect.center().y),
        Align2::CENTER_CENTER,
        FOLDERFILL,
        FontId::proportional(15.0),
        if selected {
            palette.accent
        } else {
            palette.muted
        },
    );
    let galley =
        ui.painter()
            .layout_no_wrap(name.to_string(), FontId::proportional(13.5), palette.text);
    ui.painter().galley(
        pos2(rect.left() + 30.0, rect.center().y - galley.size().y / 2.0),
        galley,
        palette.text,
    );
    response.on_hover_text(tooltip)
}

fn ai_card(ui: &mut Ui, state: &mut OculanteState, palette: Palette) {
    let card = Frame::new()
        .fill(if palette.dark {
            Color32::from_rgba_unmultiplied(255, 255, 255, 9)
        } else {
            Color32::from_rgba_unmultiplied(0, 0, 0, 7)
        })
        .stroke(Stroke::new(1.0_f32, palette.stroke))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(11));
    card.show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Smart categories")
                    .size(12.5)
                    .family(FontFamily::Name("bold".into()))
                    .color(palette.text),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut enabled = state.ml.enabled;
                if ui
                    .add(egui::Checkbox::new(&mut enabled, ""))
                    .on_hover_text("Run the tiny offline image classifier")
                    .changed()
                {
                    state.ml.set_enabled(enabled);
                    state.gallery.analysis_started = false;
                    state.gallery.analysis_all_requested = false;
                    state.gallery.analysis_queue.clear();
                    state.gallery.analysis_candidates.clear();
                    if !enabled {
                        state.gallery.category = None;
                    }
                }
            });
        });
        let analyzed = state.gallery.analyzed_count(&state.ml);
        let total = state.gallery.entries.len();
        if !state.ml.enabled {
            ui.label(
                RichText::new("Off · no images are analysed")
                    .size(10.5)
                    .color(palette.muted),
            );
            return;
        }
        if let Some(error) = state.ml.error() {
            ui.label(
                RichText::new("Classifier unavailable")
                    .size(10.5)
                    .color(Color32::from_rgb(255, 136, 136)),
            )
            .on_hover_text(error.to_string());
            return;
        }
        if state.gallery.scanning {
            ui.label(
                RichText::new("Scanning folder before tagging…")
                    .size(10.5)
                    .color(palette.muted),
            );
            return;
        }
        if total == 0 {
            ui.label(
                RichText::new("Nothing to analyse")
                    .size(10.5)
                    .color(palette.muted),
            );
            return;
        }

        let large = total > AUTO_ANALYZE_LIMIT;
        if large && analyzed < total && !state.gallery.analysis_all_requested {
            ui.label(
                RichText::new(format!(
                    "{} of {} tagged as you browse",
                    number(analyzed),
                    number(total)
                ))
                .size(10.5)
                .color(palette.muted),
            );
            ui.add_space(6.0);
            if pill_button(
                ui,
                palette,
                format!("Analyze all {} photos", number(total)),
                true,
            )
            .on_hover_text(
                "Large folders are left alone by default to keep browsing fast. \
                     A full pass can take a long time on a USB drive.",
            )
            .clicked()
            {
                state.gallery.analysis_all_requested = true;
                state.gallery.analysis_started = false;
                state.gallery.analysis_queue.clear();
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new("Visible photos are tagged automatically")
                    .size(9.5)
                    .color(palette.faint),
            );
            return;
        }

        let still_working = !state.gallery.analysis_queue.is_empty()
            || state.ml.pending_len() > 0
            || !state.ml.is_ready();
        if analyzed < total && still_working {
            ui.label(
                RichText::new(format!(
                    "Tagging {} of {} photos",
                    number(analyzed),
                    number(total)
                ))
                .size(10.5)
                .color(palette.muted),
            );
            ui.add_space(4.0);
            let progress = analyzed as f32 / total as f32;
            ui.add(
                egui::ProgressBar::new(progress)
                    .desired_width(ui.available_width())
                    .desired_height(5.0)
                    .fill(palette.accent)
                    .show_percentage(),
            );
            if large {
                ui.add_space(5.0);
                if pill_button(ui, palette, "Stop full analysis", false).clicked() {
                    state.gallery.analysis_all_requested = false;
                    state.gallery.analysis_started = false;
                    state.gallery.analysis_queue.clear();
                }
            }
        } else {
            ui.label(
                RichText::new(format!("{} tagged offline", photo_count(analyzed)))
                    .size(10.5)
                    .color(palette.muted),
            );
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new("Trees · cats · dogs · food · more")
                .size(10.0)
                .color(palette.faint),
        );
    });
}

#[derive(Clone)]
enum GridRow {
    Header { label: String },
    Tiles { start: usize, count: usize },
}

fn collection(ui: &mut Ui, state: &mut OculanteState, palette: Palette) {
    let visible = state.gallery.visible_indices(&state.ml);
    let counts = state.gallery.category_counts(&state.ml);
    let inner = ui.max_rect();
    let header_h = 78.0;
    let chips_h = if state.ml.enabled { 44.0 } else { 0.0 };

    let header_rect = Rect::from_min_size(inner.min, vec2(inner.width(), header_h));
    ui.allocate_new_ui(UiBuilder::new().max_rect(header_rect), |ui| {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(state.gallery.title())
                        .size(28.0)
                        .family(FontFamily::Name("bold".into()))
                        .color(palette.text),
                );
                ui.add_space(9.0);
                let count_text = if state.gallery.scanning {
                    "Scanning…".to_string()
                } else {
                    photo_count(visible.len())
                };
                let (badge, _) = ui.allocate_exact_size(vec2(86.0, 22.0), Sense::hover());
                ui.painter().rect_filled(
                    badge,
                    CornerRadius::same(11),
                    if palette.dark {
                        Color32::from_rgba_unmultiplied(255, 255, 255, 18)
                    } else {
                        Color32::from_rgba_unmultiplied(0, 0, 0, 12)
                    },
                );
                ui.painter().text(
                    badge.center(),
                    Align2::CENTER_CENTER,
                    count_text,
                    FontId::proportional(11.0),
                    palette.muted,
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let sort_label = if state.gallery.sort_newest {
                        format!("Newest first  {CHEVRON_DOWN}")
                    } else {
                        format!("Oldest first  {CHEVRON_DOWN}")
                    };
                    if pill_button(ui, palette, sort_label, false)
                        .on_hover_text("Change sort order")
                        .clicked()
                    {
                        state.gallery.sort_newest = !state.gallery.sort_newest;
                        state.gallery.resort();
                    }
                    ui.add_space(6.0);
                    ui.label(RichText::new("Size").size(11.5).color(palette.muted));
                    ui.spacing_mut().slider_width = 90.0;
                    ui.add(
                        egui::Slider::new(&mut state.gallery.tile_scale, 0.0..=1.0)
                            .show_value(false),
                    );
                });
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(FOLDERFILL).size(11.0).color(palette.muted));
                ui.label(
                    RichText::new(state.gallery.folder.display().to_string())
                        .size(11.5)
                        .color(palette.muted),
                );
                if state.gallery.filter == GalleryFilter::Favorites {
                    ui.label(
                        RichText::new("  ·  favourites from everywhere")
                            .size(11.0)
                            .color(palette.faint),
                    );
                } else if state.gallery.filter == GalleryFilter::Recent {
                    ui.label(
                        RichText::new("  ·  recently opened")
                            .size(11.0)
                            .color(palette.faint),
                    );
                }
            });
        });
    });

    let chips_rect = Rect::from_min_size(
        pos2(inner.left(), header_rect.bottom()),
        vec2(inner.width(), chips_h),
    );
    if state.ml.enabled {
        ui.allocate_new_ui(UiBuilder::new().max_rect(chips_rect), |ui| {
            category_chips(ui, state, palette, &counts);
        });
    }

    let grid_top = chips_rect.bottom() + 6.0;
    let grid_rect = Rect::from_min_size(
        pos2(inner.left(), grid_top),
        vec2(inner.width(), (inner.bottom() - grid_top).max(60.0)),
    );
    ui.allocate_new_ui(UiBuilder::new().max_rect(grid_rect), |ui| {
        if state.gallery.scanning && state.gallery.entries.is_empty() {
            folder_loading(ui, state, palette);
        } else if state.gallery.entries.is_empty() {
            empty_state(ui, state, palette, true);
        } else if visible.is_empty() {
            empty_state(ui, state, palette, false);
        } else {
            photo_grid(ui, state, palette, &visible);
        }
    });
}

fn category_chips(
    ui: &mut Ui,
    state: &mut OculanteState,
    palette: Palette,
    counts: &HashMap<&'static str, usize>,
) {
    let show_counts = state.gallery.query.trim().is_empty();
    ScrollArea::horizontal()
        .auto_shrink([false, true])
        .id_salt("photos-categories")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(1.0);
                if chip(
                    ui,
                    palette,
                    "All",
                    show_counts.then_some(state.gallery.entries.len()),
                    state.gallery.category.is_none(),
                )
                .clicked()
                {
                    state.gallery.category = None;
                }
                for cat in CATEGORIES {
                    let count = if cat.id == "animals" {
                        ["cats", "dogs", "birds", "insects", "animals"]
                            .iter()
                            .filter_map(|id| counts.get(id))
                            .sum()
                    } else {
                        counts.get(cat.id).copied().unwrap_or(0)
                    };
                    if count == 0 {
                        continue;
                    }
                    if chip(
                        ui,
                        palette,
                        cat.short,
                        show_counts.then_some(count),
                        state.gallery.category == Some(cat.id),
                    )
                    .on_hover_text(cat.name)
                    .clicked()
                    {
                        state.gallery.category = if state.gallery.category == Some(cat.id) {
                            None
                        } else {
                            Some(cat.id)
                        };
                    }
                }
            });
        });
}

fn chip(
    ui: &mut Ui,
    palette: Palette,
    label: &str,
    count: Option<usize>,
    selected: bool,
) -> Response {
    let text = match count {
        Some(c) if c > 0 => format!("{label}  {c}"),
        _ => label.to_string(),
    };
    let button = egui::Button::new(RichText::new(text).size(12.0).color(if selected {
        Color32::from_rgb(8, 12, 18)
    } else {
        palette.text
    }))
    .fill(if selected {
        palette.accent
    } else if palette.dark {
        Color32::from_rgba_unmultiplied(255, 255, 255, 13)
    } else {
        Color32::from_rgba_unmultiplied(0, 0, 0, 8)
    })
    .stroke(Stroke::new(1.0_f32, palette.stroke))
    .corner_radius(CornerRadius::same(15))
    .min_size(vec2(0.0, 30.0));
    ui.add(button)
}

fn folder_loading(ui: &mut Ui, state: &OculanteState, palette: Palette) {
    let rect = ui.max_rect();
    let card = Rect::from_center_size(rect.center(), vec2(rect.width().min(460.0), 170.0));
    paint_glass(ui.painter(), card, 20, palette, false);
    let inner = card.shrink2(vec2(28.0, 22.0));
    ui.allocate_new_ui(UiBuilder::new().max_rect(inner), |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(12.0);
            ui.spinner();
            ui.add_space(8.0);
            ui.label(
                RichText::new("Scanning folder…")
                    .size(18.0)
                    .family(FontFamily::Name("bold".into()))
                    .color(palette.text),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(state.gallery.folder.display().to_string())
                    .size(11.0)
                    .color(palette.muted),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("Large USB drives are indexed in the background.")
                    .size(10.5)
                    .color(palette.faint),
            );
        });
    });
}

fn empty_state(ui: &mut Ui, state: &mut OculanteState, palette: Palette, folder_empty: bool) {
    let rect = ui.max_rect();
    let card = Rect::from_center_size(rect.center(), vec2(rect.width().min(560.0), 330.0));
    paint_glass(ui.painter(), card, 20, palette, false);
    let inner = card.shrink2(vec2(38.0, 26.0));
    ui.allocate_new_ui(UiBuilder::new().max_rect(inner), |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(20.0);
            ui.painter().text(
                pos2(ui.max_rect().center().x, ui.cursor().top() + 30.0),
                Align2::CENTER_CENTER,
                if folder_empty { FOLDERFILL } else { SEARCH },
                FontId::proportional(46.0),
                palette.accent,
            );
            ui.add_space(64.0);
            let (title, body) = match state.gallery.filter {
                GalleryFilter::Favorites => (
                    "No favourites yet",
                    "Tap the heart on any photo to keep it here.",
                ),
                GalleryFilter::Recent => {
                    ("Nothing recent yet", "Photos you open will show up here.")
                }
                GalleryFilter::Collection if folder_empty => (
                    "No photos here yet",
                    "Choose a folder, or browse to a photo to get started.",
                ),
                GalleryFilter::Collection => {
                    ("No photos match", "Try another search, category or folder.")
                }
            };
            ui.label(
                RichText::new(title)
                    .size(22.0)
                    .family(FontFamily::Name("bold".into()))
                    .color(palette.text),
            );
            ui.add_space(6.0);
            ui.label(RichText::new(body).size(12.5).color(palette.muted));
            ui.add_space(20.0);
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 300.0).max(0.0) / 2.0);
                if state.gallery.filter != GalleryFilter::Collection {
                    if pill_button(ui, palette, "Go to collection", true).clicked() {
                        state.gallery.filter = GalleryFilter::Collection;
                        state.gallery.needs_refresh = true;
                    }
                    if pill_button(ui, palette, "Browse files", false).clicked() {
                        browse_for_files(state);
                    }
                } else if !state.gallery.query.is_empty() || state.gallery.category.is_some() {
                    if pill_button(ui, palette, "Clear filters", true).clicked() {
                        state.gallery.query.clear();
                        state.gallery.category = None;
                    }
                    if pill_button(ui, palette, "Browse files", false).clicked() {
                        browse_for_files(state);
                    }
                } else {
                    if pill_button(ui, palette, format!("{FOLDERFILL}  Choose folder"), true)
                        .clicked()
                    {
                        start_folder_picker(state);
                    }
                    if pill_button(ui, palette, "Browse files", false).clicked() {
                        browse_for_files(state);
                    }
                }
            });
        });
    });
}

fn photo_grid(ui: &mut Ui, state: &mut OculanteState, palette: Palette, visible: &[usize]) {
    let width = ui.available_width();
    let top = ui.cursor().top();
    let height = (ui.max_rect().bottom() - top).max(100.0);
    let gap = 10.0;
    let desired = 150.0 + state.gallery.tile_scale * 140.0;
    let columns = (((width + gap) / (desired + gap)).floor() as usize).max(1);
    let tile_w = ((width - gap * (columns.saturating_sub(1)) as f32) / columns as f32).max(60.0);
    let tile_h = tile_w * 0.75;
    state.gallery.columns = columns;

    let mut rows: Vec<GridRow> = Vec::new();
    let mut current_day = i32::MIN;
    let mut chunk_start = 0usize;
    for (pos, entry_idx) in visible.iter().enumerate() {
        let entry = &state.gallery.entries[*entry_idx];
        if entry.day_key != current_day {
            if pos > chunk_start {
                rows.push(GridRow::Tiles {
                    start: chunk_start,
                    count: pos - chunk_start,
                });
            }
            rows.push(GridRow::Header {
                label: day_label(entry.modified),
            });
            current_day = entry.day_key;
            chunk_start = pos;
        }
        if (pos - chunk_start + 1) % columns == 0 {
            rows.push(GridRow::Tiles {
                start: chunk_start,
                count: pos - chunk_start + 1,
            });
            chunk_start = pos + 1;
        }
    }
    if chunk_start < visible.len() {
        rows.push(GridRow::Tiles {
            start: chunk_start,
            count: visible.len() - chunk_start,
        });
    }

    let row_gap = 8.0;
    let header_h = 46.0;
    let tile_row_h = tile_h + row_gap;
    let total_h: f32 = rows
        .iter()
        .map(|r| match r {
            GridRow::Header { .. } => header_h,
            GridRow::Tiles { .. } => tile_row_h,
        })
        .sum();

    let scroll_id = (
        "photos-grid",
        state.gallery.filter as u8,
        state.gallery.folder.clone(),
        state.gallery.query.clone(),
        state.gallery.category,
        state.gallery.sort_newest,
    );

    // Keep keyboard selection in view.
    let selected_pos = state.gallery.selected.as_deref().and_then(|selected| {
        visible
            .iter()
            .position(|i| state.gallery.entries[*i].path == selected)
    });
    let selected_offset = selected_pos.and_then(|pos| {
        let mut y = 0.0f32;
        for row in &rows {
            match row {
                GridRow::Header { .. } => y += header_h,
                GridRow::Tiles { start, count } => {
                    if pos >= *start && pos < start + count {
                        let row_in_chunk = (pos - start) / columns;
                        return Some(y + row_in_chunk as f32 * tile_row_h);
                    }
                    y += tile_row_h;
                }
            }
        }
        None
    });

    let mut scroll = ScrollArea::vertical()
        .auto_shrink([false, false])
        .id_salt(scroll_id)
        .max_height(height);
    if state.gallery.scroll_selected {
        if let Some(y) = selected_offset {
            scroll = scroll.vertical_scroll_offset((y - height / 2.0 + tile_h / 2.0).max(0.0));
        }
        state.gallery.scroll_selected = false;
    }
    scroll.show_viewport(ui, |ui, viewport| {
        ui.set_min_width(width);
        let (content_rect, _) = ui.allocate_exact_size(vec2(width, total_h + 8.0), Sense::hover());
        let mut y = 0.0;
        for row in &rows {
            match row {
                GridRow::Header { label } => {
                    if y + header_h >= viewport.min.y - header_h && y <= viewport.max.y + header_h {
                        let row_rect = Rect::from_min_size(
                            content_rect.min + vec2(0.0, y),
                            vec2(width, header_h),
                        );
                        let galley = ui.painter().layout_no_wrap(
                            label.clone(),
                            bold_font(17.0),
                            palette.text,
                        );
                        ui.painter().galley(
                            pos2(row_rect.left() + 2.0, row_rect.bottom() - 20.0),
                            galley,
                            palette.text,
                        );
                    }
                    y += header_h;
                }
                GridRow::Tiles { start, count } => {
                    if y + tile_row_h >= viewport.min.y - tile_row_h
                        && y <= viewport.max.y + tile_row_h
                    {
                        for i in 0..*count {
                            let vis_idx = start + i;
                            let Some(entry_idx) = visible.get(vis_idx) else {
                                continue;
                            };
                            let (path, modified, modified_ms) = {
                                let e = &state.gallery.entries[*entry_idx];
                                (e.path.clone(), e.modified, e.modified_ms)
                            };
                            let tile_rect = Rect::from_min_size(
                                content_rect.min + vec2(i as f32 * (tile_w + gap), y),
                                vec2(tile_w, tile_h),
                            );
                            let selected =
                                state.gallery.selected.as_deref() == Some(path.as_path());
                            let favourite = is_favorite(state, &path);
                            let label = state.ml.label_for(&path, modified_ms).cloned();
                            photo_tile(
                                ui,
                                state,
                                palette,
                                &path,
                                modified,
                                modified_ms,
                                tile_rect,
                                selected,
                                favourite,
                                label.as_ref(),
                            );
                        }
                    }
                    y += tile_row_h;
                }
            }
        }
    });
}

fn photo_tile(
    ui: &mut Ui,
    state: &mut OculanteState,
    palette: Palette,
    path: &Path,
    modified: SystemTime,
    modified_ms: u64,
    rect: Rect,
    selected: bool,
    favourite: bool,
    label: Option<&MlLabel>,
) {
    let id = ui.id().with(("photo-tile", path));
    let response = ui.interact(rect, id, Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(id.with("hover"), response.hovered(), 0.14);
    let radius = 13;

    paint_shadow(ui.painter(), rect, radius, 40);
    ui.painter()
        .rect_filled(rect, CornerRadius::same(radius), Color32::from_gray(36));

    match state.thumbnails.get_gallery(path) {
        Ok(thumb) => {
            let uri = format!("file://{}", thumb.display());
            egui::Image::new(uri.clone())
                .corner_radius(CornerRadius::same(radius))
                .show_loading_spinner(false)
                .paint_at(ui, rect);
            let seq = state.gallery.photo_texture_seq;
            state.gallery.photo_textures.insert(uri, seq);
            if state.ml.enabled && !state.ml.is_analyzed(path, modified_ms) {
                state
                    .gallery
                    .analysis_candidates
                    .push((path.to_path_buf(), modified_ms));
            }
        }
        Err(_) => {
            ui.ctx().request_repaint_after(Duration::from_millis(180));
            let time = ui.ctx().input(|i| i.time) as f32;
            let sheen = (time * 1.8).sin() * 0.5 + 0.5;
            let base = if palette.dark {
                Color32::from_rgba_unmultiplied(34, 39, 50, 255)
            } else {
                Color32::from_rgba_unmultiplied(214, 222, 234, 255)
            };
            ui.painter()
                .rect_filled(rect, CornerRadius::same(radius), base);
            ui.painter().circle_filled(
                rect.center() + vec2(-30.0 + sheen * 60.0, 0.0),
                44.0,
                palette.accent.gamma_multiply(0.035),
            );
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_uppercase())
                .unwrap_or_default();
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                ext,
                FontId::proportional(13.0),
                palette.muted,
            );
        }
    }

    // Hover/touch overlay with the file name, date and AI tag.
    if hover > 0.02 {
        let overlay = Rect::from_min_max(
            pos2(rect.left(), rect.bottom() - rect.height() * 0.46),
            rect.right_bottom(),
        );
        let alpha = (hover * 175.0) as u8;
        ui.painter().rect_filled(
            overlay,
            CornerRadius {
                nw: 0,
                ne: 0,
                sw: radius,
                se: radius,
            },
            Color32::from_black_alpha(alpha),
        );
        if hover > 0.45 {
            let name = path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            let text_alpha = ((hover - 0.45) / 0.55 * 255.0) as u8;
            let galley = ui.painter().layout(
                name,
                FontId::proportional(12.5),
                Color32::from_white_alpha(text_alpha),
                rect.width() - 22.0,
            );
            ui.painter().galley(
                pos2(rect.left() + 11.0, rect.bottom() - galley.size().y - 22.0),
                galley,
                Color32::WHITE,
            );
            let sub = if let Some(label) = label {
                if label.hinted {
                    format!("{} · filename hint", ml::category_name(&label.category))
                } else {
                    format!(
                        "{} · {:.0}%",
                        ml::category_name(&label.category),
                        label.confidence * 100.0
                    )
                }
            } else {
                format!(
                    "{} · {}",
                    full_date(modified).split(',').next().unwrap_or_default(),
                    time_label(modified)
                )
            };
            ui.painter().text(
                pos2(rect.left() + 11.0, rect.bottom() - 8.0),
                Align2::LEFT_BOTTOM,
                sub,
                FontId::proportional(10.5),
                Color32::from_rgba_unmultiplied(
                    palette.accent.r(),
                    palette.accent.g(),
                    palette.accent.b(),
                    text_alpha,
                ),
            );
        }
    }

    // Selection/favourite affordances.
    let check_center = rect.left_top() + vec2(18.0, 18.0);
    if selected {
        ui.painter()
            .circle_filled(check_center, 11.0, palette.accent);
        ui.painter().text(
            check_center,
            Align2::CENTER_CENTER,
            CHECK,
            FontId::proportional(11.0),
            Color32::from_rgb(8, 12, 18),
        );
    } else if hover > 0.12 {
        ui.painter().circle_stroke(
            check_center,
            10.0,
            Stroke::new(1.5_f32, Color32::from_white_alpha((hover * 160.0) as u8)),
        );
    }

    let heart_center = rect.right_top() + vec2(-18.0, 18.0);
    if favourite || hover > 0.2 {
        ui.painter()
            .circle_filled(heart_center, 13.0, Color32::from_black_alpha(115));
        ui.painter().text(
            heart_center,
            Align2::CENTER_CENTER,
            if favourite { HEARTFILL } else { HEART },
            FontId::proportional(14.0),
            if favourite {
                palette.heart
            } else {
                Color32::from_white_alpha((hover * 230.0) as u8)
            },
        );
        let heart_response = ui.interact(
            Rect::from_center_size(heart_center, Vec2::splat(30.0)),
            id.with("favourite"),
            Sense::click(),
        );
        if heart_response.clicked() {
            set_favorite(state, path);
            return;
        }
    }

    if selected {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(radius),
            Stroke::new(2.2_f32, palette.accent),
            StrokeKind::Inside,
        );
    } else if hover > 0.1 {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(radius),
            Stroke::new(1.0_f32, Color32::from_white_alpha((hover * 110.0) as u8)),
            StrokeKind::Inside,
        );
    }

    if response.clicked() {
        state.gallery.selected = Some(path.to_path_buf());
    }
    if response.double_clicked() {
        open_photo(state, path);
    }
}

fn process_analysis(ctx: &Context, state: &mut OculanteState) {
    if !state.ml.enabled || state.gallery.view != PhotosView::Gallery || state.gallery.scanning {
        return;
    }

    let total = state.gallery.entries.len();
    let analyze_all = total <= AUTO_ANALYZE_LIMIT || state.gallery.analysis_all_requested;

    // Cheap, high-priority work: photos that are visible on screen already
    // have a thumbnail, so they can be classified without touching the drive.
    let candidates = std::mem::take(&mut state.gallery.analysis_candidates);
    let mut scheduled = 0usize;
    for (path, modified_ms) in candidates {
        if scheduled >= 4 {
            break;
        }
        if state.ml.is_analyzed(&path, modified_ms) {
            continue;
        }
        if let Some(thumbnail) = state.thumbnails.existing_analysis(&path) {
            state.ml.request(&path, &thumbnail, modified_ms);
            scheduled += 1;
        }
    }

    if !analyze_all {
        // Large library: stop here. Classification happens only for photos
        // the user has actually looked at.
        if scheduled > 0 || state.ml.pending_len() > 0 {
            ctx.request_repaint_after(Duration::from_millis(300));
        }
        return;
    }

    if !state.gallery.analysis_started {
        let items = state
            .gallery
            .entries
            .iter()
            .filter(|e| !state.ml.is_analyzed(&e.path, e.modified_ms))
            .map(|e| AnalysisItem {
                path: e.path.clone(),
                modified_ms: e.modified_ms,
                tries: 0,
            })
            .collect::<VecDeque<_>>();
        state.gallery.analysis_queue = items;
        state.gallery.analysis_started = true;
    }

    // Background passes are throttled, even if the pointer is generating
    // extra frames. Keep at most a small inference backlog so "Stop" takes
    // effect quickly even on slow CPUs.
    if state.ml.pending_len() < 16
        && state.gallery.last_analysis_tick.elapsed() >= Duration::from_millis(220)
    {
        state.gallery.last_analysis_tick = Instant::now();
        let mut examined = 0usize;
        while examined < 16 && scheduled < 4 {
            let Some(mut item) = state.gallery.analysis_queue.pop_front() else {
                break;
            };
            examined += 1;
            if state.ml.is_analyzed(&item.path, item.modified_ms) || item.tries > 50 {
                continue;
            }

            // Existing gallery/_ml thumbnails are free to use even when the
            // thumbnail pool is completely busy.
            if let Some(thumbnail) = state.thumbnails.existing_analysis(&item.path) {
                state.ml.request(&item.path, &thumbnail, item.modified_ms);
                scheduled += 1;
                continue;
            }

            // Leave at least one worker for the visible grid. If the pool is
            // fully busy, rotate the item and try a cached one instead.
            if state.thumbnails.active_jobs() + 1 >= thumbnails::MAX_THREADS {
                state.gallery.analysis_queue.push_back(item);
                continue;
            }
            match state.thumbnails.get_analysis(&item.path) {
                Ok(thumbnail) => {
                    state.ml.request(&item.path, &thumbnail, item.modified_ms);
                    scheduled += 1;
                }
                Err(_) => {
                    item.tries += 1;
                    state.gallery.analysis_queue.push_back(item);
                }
            }
        }
    }

    if !state.gallery.analysis_queue.is_empty() || state.ml.pending_len() > 0 {
        ctx.request_repaint_after(Duration::from_millis(180));
    }
}

fn folder_picker(ctx: &Context, state: &mut OculanteState) {
    if !state.gallery.folder_picker_open {
        return;
    }
    let palette = palette(&ctx.style().visuals, state);
    let mut open = true;
    let mut choose: Option<PathBuf> = None;
    let mut navigate: Option<PathBuf> = None;
    egui::Window::new("Choose a folder")
        .id(Id::new("photos-folder-picker"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(560.0)
        .default_height(480.0)
        .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
        .frame(glass_frame(palette, 18))
        .show(ctx, |ui| {
            let path = state.gallery.picker_path.clone();
            ui.horizontal(|ui| {
                if pill_button(ui, palette, format!("{CHEVRON_UP}  Up"), false).clicked() {
                    if let Some(parent) = path.parent() {
                        navigate = Some(parent.to_path_buf());
                    }
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new(path.display().to_string())
                        .size(12.0)
                        .color(palette.text),
                );
            });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                let places = [
                    ("Pictures", dirs::picture_dir()),
                    ("Desktop", dirs::desktop_dir()),
                    ("Downloads", dirs::download_dir()),
                    ("Home", dirs::home_dir()),
                ];
                for (name, place) in places {
                    if let Some(place) = place {
                        if place.is_dir() && pill_button(ui, palette, name, false).clicked() {
                            navigate = Some(place);
                        }
                    }
                }
                #[cfg(target_os = "windows")]
                for letter in b'A'..=b'Z' {
                    let drive = PathBuf::from(format!("{}:\\", letter as char));
                    if drive.is_dir()
                        && pill_button(ui, palette, format!("{}:", letter as char), false).clicked()
                    {
                        navigate = Some(drive);
                    }
                }
                for bookmark in state.volatile_settings.folder_bookmarks.iter().take(6) {
                    let name = bookmark
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| bookmark.display().to_string());
                    if pill_button(ui, palette, name, false).clicked() {
                        navigate = Some(bookmark.clone());
                    }
                }
            });
            ui.add_space(8.0);
            Frame::new()
                .fill(if palette.dark {
                    Color32::from_rgba_unmultiplied(0, 0, 0, 90)
                } else {
                    Color32::from_rgba_unmultiplied(255, 255, 255, 110)
                })
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ScrollArea::vertical()
                        .id_salt("photos-picker-list")
                        .max_height(270.0)
                        .show(ui, |ui| {
                            if state.gallery.picker_for.as_ref() != Some(&path) {
                                let mut dirs = std::fs::read_dir(&path)
                                    .map(|entries| {
                                        entries
                                            .flatten()
                                            .filter(|e| {
                                                e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                                                    && !e
                                                        .file_name()
                                                        .to_string_lossy()
                                                        .starts_with('.')
                                            })
                                            .map(|e| e.path())
                                            .collect::<Vec<_>>()
                                    })
                                    .unwrap_or_default();
                                dirs.sort();
                                state.gallery.picker_dirs = dirs;
                                state.gallery.picker_for = Some(path.clone());
                            }
                            let dirs = state.gallery.picker_dirs.clone();
                            for dir in dirs {
                                let name = dir
                                    .file_name()
                                    .map(|f| f.to_string_lossy().to_string())
                                    .unwrap_or_else(|| dir.display().to_string());
                                if folder_item(
                                    ui,
                                    palette,
                                    &name,
                                    &dir.display().to_string(),
                                    false,
                                )
                                .clicked()
                                {
                                    navigate = Some(dir);
                                }
                            }
                        });
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if pill_button(ui, palette, "Open this folder", true).clicked() {
                        choose = Some(state.gallery.picker_path.clone());
                    }
                    if pill_button(ui, palette, "Cancel", false).clicked() {
                        state.gallery.folder_picker_open = false;
                    }
                });
            });
        });
    if let Some(path) = navigate {
        if path.is_dir() {
            state.gallery.picker_path = path;
            state.gallery.picker_for = None;
        }
    }
    if let Some(path) = choose {
        open_folder(state, path);
        state.gallery.folder_picker_open = false;
    }
    if !open {
        state.gallery.folder_picker_open = false;
    }
}

fn viewer_ui(ctx: &Context, app: &mut App, state: &mut OculanteState) {
    let palette = palette(&ctx.style().visuals, state);

    if state.gallery.slideshow_needs_fit {
        state.reset_image = true;
        state.gallery.slideshow_needs_fit = false;
    }

    // Do not let a focused toolbar button swallow Escape/Space. Only active
    // text editors (settings or the edit panel) really need to keep the keys.
    let text_editing = (state.settings_enabled || state.persistent_settings.edit_enabled)
        && !state.gallery.slideshow;
    if !text_editing {
        let escape = ctx.input(|i| i.key_pressed(Key::Escape));
        let space = ctx.input(|i| i.key_pressed(Key::Space));
        if escape {
            if state.gallery.slideshow {
                stop_slideshow(app, state, true);
            } else {
                back_to_gallery(app, state);
            }
        } else if space && !state.gallery.slideshow {
            start_slideshow(app, state);
        }
    }

    if state.gallery.slideshow {
        slideshow_pill(ctx, app, state, palette);
        ctx.request_repaint_after(Duration::from_millis(220));
        return;
    }

    if !state.persistent_settings.zen_mode {
        viewer_top_bar(ctx, app, state, palette);
        viewer_filmstrip(ctx, state, palette);
    }

    if state.current_image.is_none() && state.current_path.is_some() && !state.is_loaded {
        egui::Area::new(Id::new("photos-loading"))
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .order(Order::Foreground)
            .show(ctx, |ui| {
                Frame::new()
                    .fill(palette.glass_strong)
                    .stroke(Stroke::new(1.0_f32, palette.stroke))
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(Margin::symmetric(18, 12))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new("Loading photo…")
                                    .size(12.5)
                                    .color(palette.text),
                            );
                        });
                    });
            });
    }
}

fn viewer_top_bar(ctx: &Context, app: &mut App, state: &mut OculanteState, palette: Palette) {
    egui::TopBottomPanel::top("photos-viewer-top")
        .exact_height(74.0)
        .show_separator_line(false)
        .frame(Frame::NONE)
        .show(ctx, |ui| {
            let rect = ui.max_rect().shrink2(vec2(14.0, 9.0));
            paint_glass(ui.painter(), rect, 18, palette, true);
            let inner = rect.shrink2(vec2(14.0, 8.0));
            ui.allocate_new_ui(
                UiBuilder::new()
                    .max_rect(inner)
                    .layout(Layout::left_to_right(Align::Center)),
                |ui| {
                    if icon_button(ui, palette, CARET_LEFT, "Back to collection").clicked() {
                        back_to_gallery(app, state);
                    }
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        let name = state
                            .current_path
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|f| f.to_string_lossy().to_string())
                            .unwrap_or_else(|| "Photos".into());
                        ui.add(
                            egui::Label::new(
                                RichText::new(name)
                                    .size(14.5)
                                    .family(FontFamily::Name("bold".into()))
                                    .color(palette.text),
                            )
                            .truncate(),
                        );
                        let mut subtitle = String::new();
                        if !state.scrubber.entries.is_empty() {
                            subtitle.push_str(&format!(
                                "{} of {}",
                                state.scrubber.index + 1,
                                state.scrubber.entries.len()
                            ));
                        }
                        if let Some(path) = &state.current_path {
                            if let Ok(meta) = std::fs::metadata(path) {
                                if let Ok(modified) = meta.modified() {
                                    if !subtitle.is_empty() {
                                        subtitle.push_str("  ·  ");
                                    }
                                    subtitle.push_str(&full_date(modified));
                                }
                            }
                            if let Some(label) = state.ml.label_for(path, ml::modified_ms_for(path))
                            {
                                if !subtitle.is_empty() {
                                    subtitle.push_str("  ·  ");
                                }
                                if label.hinted {
                                    subtitle.push_str(&format!(
                                        "{} · filename hint",
                                        ml::category_name(&label.category)
                                    ));
                                } else {
                                    subtitle.push_str(&format!(
                                        "{} · {} · {:.0}%",
                                        ml::category_name(&label.category),
                                        label.label,
                                        label.confidence * 100.0
                                    ));
                                }
                            }
                        }
                        ui.label(RichText::new(subtitle).size(10.5).color(palette.muted));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_button(ui, palette, GEAR, "Preferences").clicked() {
                            state.settings_enabled = true;
                        }
                        if icon_button(ui, palette, ARROWS_OUT_SIMPLE, "Fullscreen").clicked() {
                            toggle_fullscreen(app, state);
                        }
                        if icon_button_sized(
                            ui,
                            palette,
                            INFO,
                            "Image information",
                            17.0,
                            34.0,
                            state.persistent_settings.info_enabled,
                        )
                        .clicked()
                        {
                            state.persistent_settings.info_enabled =
                                !state.persistent_settings.info_enabled;
                            state.reset_image = true;
                        }
                        if icon_button_sized(
                            ui,
                            palette,
                            PENCIL_SIMPLE_LINE,
                            "Edit photo",
                            17.0,
                            34.0,
                            state.persistent_settings.edit_enabled,
                        )
                        .clicked()
                        {
                            state.persistent_settings.edit_enabled =
                                !state.persistent_settings.edit_enabled;
                            state.reset_image = true;
                        }
                        let favourite = state
                            .current_path
                            .as_deref()
                            .map(|p| is_favorite(state, p))
                            .unwrap_or(false);
                        let heart = if favourite { HEARTFILL } else { HEART };
                        let heart_color = if favourite {
                            palette.heart
                        } else {
                            palette.text
                        };
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(heart).size(18.0).color(heart_color),
                                )
                                .frame(false)
                                .min_size(Vec2::splat(34.0)),
                            )
                            .on_hover_text(if favourite {
                                "Remove from favourites"
                            } else {
                                "Add to favourites"
                            })
                            .clicked()
                        {
                            if let Some(path) = state.current_path.clone() {
                                set_favorite(state, &path);
                            }
                        }
                        ui.add_space(2.0);
                        if pill_button(ui, palette, "Slideshow", false).clicked() {
                            start_slideshow(app, state);
                        }
                        ui.add_space(6.0);
                    });
                },
            );
        });
}

fn viewer_filmstrip(ctx: &Context, state: &mut OculanteState, palette: Palette) {
    let paths = state.scrubber.entries.clone();
    let current = state.scrubber.index;
    egui::TopBottomPanel::bottom("photos-filmstrip")
        .exact_height(104.0)
        .show_separator_line(false)
        .frame(Frame::NONE)
        .show(ctx, |ui| {
            let rect = ui.max_rect().shrink2(vec2(14.0, 9.0));
            paint_glass(ui.painter(), rect, 16, palette, true);
            let inner = rect.shrink2(vec2(12.0, 8.0));
            ui.allocate_new_ui(UiBuilder::new().max_rect(inner), |ui| {
                let item_w = 98.0;
                let total_w = (paths.len() as f32 * item_w).max(1.0);
                let mut load_index = None;
                ScrollArea::horizontal()
                    .id_salt("photos-filmstrip-scroll")
                    .auto_shrink([false, false])
                    .show_viewport(ui, |ui, viewport| {
                        let (content, _) =
                            ui.allocate_exact_size(vec2(total_w, 69.0), Sense::hover());
                        let start =
                            ((viewport.min.x / item_w).floor() as isize - 1).max(0) as usize;
                        let end =
                            (((viewport.max.x / item_w).ceil() as usize) + 2).min(paths.len());
                        for i in start..end {
                            let path = &paths[i];
                            let thumb_rect = Rect::from_min_size(
                                content.min + vec2(i as f32 * item_w, 0.0),
                                vec2(92.0, 69.0),
                            );
                            let response =
                                ui.interact(thumb_rect, Id::new(("film", i)), Sense::click());
                            ui.painter().rect_filled(
                                thumb_rect,
                                CornerRadius::same(8),
                                Color32::from_gray(30),
                            );
                            if let Ok(thumb) = state.thumbnails.get(path) {
                                let uri = format!("file://{}", thumb.display());
                                egui::Image::new(uri.clone())
                                    .corner_radius(CornerRadius::same(8))
                                    .show_loading_spinner(false)
                                    .paint_at(ui, thumb_rect);
                                let seq = state.gallery.photo_texture_seq;
                                state.gallery.photo_textures.insert(uri, seq);
                            } else {
                                ui.ctx().request_repaint_after(Duration::from_millis(180));
                            }
                            if i == current {
                                ui.painter().rect_stroke(
                                    thumb_rect,
                                    CornerRadius::same(8),
                                    Stroke::new(2.0_f32, palette.accent),
                                    StrokeKind::Inside,
                                );
                            } else if response.hovered() {
                                ui.painter().rect_stroke(
                                    thumb_rect,
                                    CornerRadius::same(8),
                                    Stroke::new(1.0_f32, Color32::from_white_alpha(120)),
                                    StrokeKind::Inside,
                                );
                            }
                            if response.clicked() && i != current {
                                load_index = Some(i);
                            }
                        }
                        if state.gallery.filmstrip_index != Some(current) {
                            let focus_rect = Rect::from_min_size(
                                content.min + vec2(current as f32 * item_w, 0.0),
                                vec2(92.0, 69.0),
                            );
                            ui.scroll_to_rect(focus_rect.expand(60.0), Some(Align::Center));
                            state.gallery.filmstrip_index = Some(current);
                        }
                    });
                if let Some(index) = load_index {
                    if let Some(path) = paths.get(index).cloned() {
                        state.scrubber.index = index;
                        state.current_path = Some(path.clone());
                        state.current_image = None;
                        state.is_loaded = false;
                        state.player.load(&path);
                        state.gallery.filmstrip_index = None;
                        state.gallery.last_advance = Some(Instant::now());
                    }
                }
            });
        });
}

fn slideshow_pill(ctx: &Context, app: &mut App, state: &mut OculanteState, palette: Palette) {
    egui::Area::new(Id::new("photos-slideshow-pill"))
        .anchor(Align2::CENTER_TOP, vec2(0.0, 18.0))
        .order(Order::Foreground)
        .show(ctx, |ui| {
            Frame::new()
                .fill(palette.glass_strong)
                .stroke(Stroke::new(1.0_f32, palette.stroke))
                .corner_radius(CornerRadius::same(22))
                .inner_margin(Margin::symmetric(14, 7))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Slideshow")
                                .size(12.0)
                                .family(FontFamily::Name("bold".into()))
                                .color(palette.text),
                        );
                        for interval in [3.0f32, 5.0, 10.0] {
                            let selected =
                                (state.gallery.slideshow_interval - interval).abs() < 0.1;
                            if chip(ui, palette, &format!("{interval:.0}s"), None, selected)
                                .clicked()
                            {
                                state.gallery.slideshow_interval = interval;
                                state.gallery.last_advance = Some(Instant::now());
                            }
                        }
                        if pill_button(ui, palette, "Pause", false).clicked() {
                            stop_slideshow(app, state, false);
                        }
                        if pill_button(ui, palette, "Exit", false).clicked() {
                            back_to_gallery(app, state);
                        }
                    });
                });
        });
}

fn start_slideshow(app: &mut App, state: &mut OculanteState) {
    if state.current_path.is_none() {
        return;
    }
    state.gallery.slideshow = true;
    state.gallery.last_advance = Some(Instant::now());
    state.gallery.slideshow_needs_fit = true;
    state.scrubber.wrap = true;
    state.gallery.slideshow_fullscreen = !app.window().is_fullscreen();
    if state.gallery.slideshow_fullscreen {
        toggle_fullscreen(app, state);
    }
}

fn stop_slideshow(app: &mut App, state: &mut OculanteState, exit_fullscreen: bool) {
    state.gallery.slideshow = false;
    state.gallery.last_advance = None;
    state.gallery.slideshow_needs_fit = true;
    if exit_fullscreen {
        if state.gallery.slideshow_fullscreen && app.window().is_fullscreen() {
            toggle_fullscreen(app, state);
        }
        state.gallery.slideshow_fullscreen = false;
    }
}

fn back_to_gallery(app: &mut App, state: &mut OculanteState) {
    stop_slideshow(app, state, true);
    state.gallery.view = PhotosView::Gallery;
    state.gallery.filmstrip_index = None;
    state.redraw = true;
}

/// Called from main's update loop to advance slideshows even with the lazy
/// event loop.
pub fn tick_slideshow(state: &mut OculanteState) {
    if !state.gallery.slideshow || state.gallery.view != PhotosView::Viewer {
        return;
    }
    if !state.is_loaded {
        return;
    }
    let due = state
        .gallery
        .last_advance
        .map(|t| t.elapsed().as_secs_f32() >= state.gallery.slideshow_interval)
        .unwrap_or(true);
    if due {
        state.gallery.last_advance = Some(Instant::now());
        next_image(state);
        state.redraw = true;
    }
}
