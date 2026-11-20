use super::*;
use crate::appstate::OculanteState;
use egui::{Context, FontData, FontDefinitions};
use epaint::FontFamily;
use font_kit::{
    family_name::FamilyName, handle::Handle, properties::Properties, source::SystemSource,
};
use log::warn;
use std::{collections::HashMap, fs::read, sync::Arc};

pub fn apply_theme(state: &mut OculanteState, ctx: &Context) {
    let dark = match state.persistent_settings.theme {
        ColorTheme::Light => false,
        ColorTheme::Dark => true,
        ColorTheme::System => match dark_light::detect() {
            Ok(dark_light::Mode::Light) => false,
            _ => true,
        },
    };

    // Migrate the old Oculante defaults so this fork opens with the Photos
    // palette on an existing installation.
    if state.persistent_settings.accent_color == [255, 0, 75] {
        state.persistent_settings.accent_color = PersistentSettings::default().accent_color;
    }
    if state.persistent_settings.background_color == [30, 30, 30] {
        state.persistent_settings.background_color = PersistentSettings::default().background_color;
    }
    if state.persistent_settings.title_format == "{APP} | {VERSION} | {FULLPATH}" {
        state.persistent_settings.title_format = PersistentSettings::default().title_format;
    }
    if state.persistent_settings.min_window_size == (100, 100) {
        state.persistent_settings.min_window_size = PersistentSettings::default().min_window_size;
    }

    let mut style: egui::Style = (*ctx.style()).clone();
    style.visuals = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    let accent = Color32::from_rgb(
        state.persistent_settings.accent_color[0],
        state.persistent_settings.accent_color[1],
        state.persistent_settings.accent_color[2],
    );
    let text = if dark {
        Color32::from_rgb(238, 243, 250)
    } else {
        Color32::from_rgb(22, 28, 38)
    };
    let muted = if dark {
        Color32::from_rgb(158, 169, 186)
    } else {
        Color32::from_rgb(92, 104, 122)
    };
    let faint = if dark {
        Color32::from_rgba_unmultiplied(255, 255, 255, 26)
    } else {
        Color32::from_rgba_unmultiplied(0, 0, 0, 18)
    };
    let panel = if dark {
        Color32::from_rgba_unmultiplied(18, 22, 31, 246)
    } else {
        Color32::from_rgba_unmultiplied(250, 252, 255, 248)
    };
    let extreme = if dark {
        Color32::from_rgb(12, 16, 24)
    } else {
        Color32::from_rgb(232, 238, 247)
    };

    style.visuals.dark_mode = dark;
    style.visuals.override_text_color = Some(text);
    style.visuals.panel_fill = panel;
    style.visuals.window_fill = panel;
    style.visuals.extreme_bg_color = extreme;
    style.visuals.faint_bg_color = faint;
    style.visuals.code_bg_color = if dark {
        Color32::from_rgba_unmultiplied(255, 255, 255, 10)
    } else {
        Color32::from_rgba_unmultiplied(0, 0, 0, 8)
    };
    style.visuals.hyperlink_color = accent;
    style.visuals.warn_fg_color = Color32::from_rgb(255, 186, 108);
    style.visuals.error_fg_color = Color32::from_rgb(255, 107, 128);
    style.visuals.selection.bg_fill = accent;
    style.visuals.selection.stroke = Stroke::new(1.5_f32, Color32::from_rgb(8, 12, 18));

    // Frosted, rounded widgets.
    for widgets in [
        &mut style.visuals.widgets.noninteractive,
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        widgets.bg_stroke = Stroke::NONE;
        widgets.corner_radius = CornerRadius::same(9);
    }
    style.visuals.widgets.noninteractive.fg_stroke.color = text;
    style.visuals.widgets.noninteractive.bg_fill = panel;
    style.visuals.widgets.inactive.fg_stroke.color = text;
    style.visuals.widgets.inactive.weak_bg_fill = if dark {
        Color32::from_rgba_unmultiplied(255, 255, 255, 12)
    } else {
        Color32::from_rgba_unmultiplied(0, 0, 0, 8)
    };
    style.visuals.widgets.inactive.bg_fill = style.visuals.widgets.inactive.weak_bg_fill;
    style.visuals.widgets.hovered.fg_stroke.color = text;
    style.visuals.widgets.hovered.weak_bg_fill = if dark {
        Color32::from_rgba_unmultiplied(255, 255, 255, 22)
    } else {
        Color32::from_rgba_unmultiplied(0, 0, 0, 14)
    };
    style.visuals.widgets.hovered.bg_fill = style.visuals.widgets.hovered.weak_bg_fill;
    style.visuals.widgets.active.fg_stroke.color = text;
    style.visuals.widgets.active.weak_bg_fill = if dark {
        Color32::from_rgba_unmultiplied(255, 255, 255, 30)
    } else {
        Color32::from_rgba_unmultiplied(0, 0, 0, 20)
    };
    style.visuals.widgets.active.bg_fill = style.visuals.widgets.active.weak_bg_fill;
    style.visuals.widgets.open.fg_stroke.color = text;

    style.spacing.scroll = egui::style::ScrollStyle::solid();
    style.interaction.tooltip_delay = 0.0;
    style.interaction.tooltip_grace_time = 0.2;
    style.animation_time = 0.14;
    style.spacing.icon_width = 20.;
    style.spacing.window_margin = Margin::same(10);
    style.spacing.menu_margin = Margin::same(8);
    style.spacing.item_spacing = vec2(10., 8.);
    style.spacing.button_padding = vec2(10., 5.);
    style.spacing.icon_width_inner = style.spacing.icon_width / 1.5;
    style.spacing.interact_size.y = BUTTON_HEIGHT_SMALL;
    style.spacing.slider_width = 120.;

    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(11.5));
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(22.0));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::monospace(13.0));

    // keep muted text available for existing panels
    style.visuals.widgets.noninteractive.fg_stroke.color = text;
    let _ = muted;

    ctx.set_style(style);
}

/// Attempt to load a system font by any of the given `family_names`, returning the first match.
fn load_font_family(family_names: &[&str]) -> Option<Vec<u8>> {
    let system_source = SystemSource::new();
    for &name in family_names {
        let font_handle = system_source
            .select_best_match(&[FamilyName::Title(name.to_string())], &Properties::new());
        match font_handle {
            Ok(h) => match &h {
                Handle::Memory { bytes, .. } => {
                    info!("Loaded {name} from memory.");
                    return Some(bytes.to_vec());
                }
                Handle::Path { path, .. } => {
                    info!("Loaded {name} from path: {:?}", path);
                    if let Ok(data) = read(path) {
                        return Some(data);
                    }
                }
            },
            Err(e) => debug!("Could not load {}: {:?}", name, e),
        }
    }
    None
}

pub fn load_system_fonts(mut fonts: FontDefinitions) -> FontDefinitions {
    debug!("Attempting to load sys fonts");
    let mut fontdb = HashMap::new();

    fontdb.insert(
        "simplified_chinese",
        vec![
            "Heiti SC",
            "Songti SC",
            "Noto Sans CJK SC", // Good coverage for Simplified Chinese
            "Noto Sans SC",
            "WenQuanYi Zen Hei", // INcludes both Simplified and Traditional Chinese.
            "SimSun",
            "Noto Sans SC",
            "PingFang SC",
            "Source Han Sans CN",
        ],
    );

    fontdb.insert("traditional_chinese", vec!["Source Han Sans HK"]);

    fontdb.insert(
        "japanese",
        vec![
            "Noto Sans JP",
            "Noto Sans CJK JP",
            "Source Han Sans JP",
            "MS Gothic",
        ],
    );

    fontdb.insert("korean", vec!["Source Han Sans KR"]);

    fontdb.insert("taiwanese", vec!["Source Han Sans TW"]);

    fontdb.insert(
        "arabic_fonts",
        vec![
            "Noto Sans Arabic",
            "Amiri",
            "Lateef",
            "Al Tarikh",
            "Segoe UI",
        ],
    );

    for (region, font_names) in fontdb {
        if let Some(font_data) = load_font_family(&font_names) {
            info!("Inserting font {region}");
            fonts
                .font_data
                .insert(region.to_owned(), Arc::new(FontData::from_owned(font_data)));

            fonts
                .families
                .get_mut(&FontFamily::Proportional)
                .unwrap()
                .push(region.to_owned());
        } else {
            warn!("Could not load a font for region {region}. If you experience incorrect file names, try installing one of these fonts: [{}]", font_names.join(", "))
        }
    }
    fonts
}
