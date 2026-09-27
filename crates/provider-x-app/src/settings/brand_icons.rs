use std::{collections::BTreeMap, sync::Arc};

use gpui_kit::base::ThemeAppearance;
use gpui_kit::{App, Div, Image, ImageFormat, div, img, prelude::*, px, rgb};
use gpui_omarchy::{self as ui, ActiveTheme};

const ASSETS: &[(&str, &str)] = &[
    (
        "openai",
        include_str!("../../resources/providers/openai.svg"),
    ),
    (
        "anthropic",
        include_str!("../../resources/providers/anthropic.svg"),
    ),
    (
        "deepseek",
        include_str!("../../resources/providers/deepseek-color.svg"),
    ),
    (
        "kimi",
        include_str!("../../resources/providers/kimi-color.svg"),
    ),
    (
        "qwen",
        include_str!("../../resources/providers/qwen-color.svg"),
    ),
    ("zai", include_str!("../../resources/providers/zai.svg")),
    (
        "minimax",
        include_str!("../../resources/providers/minimax-color.svg"),
    ),
    ("xai", include_str!("../../resources/providers/xai.svg")),
    (
        "openrouter",
        include_str!("../../resources/providers/openrouter-color.svg"),
    ),
    (
        "opencode",
        include_str!("../../resources/providers/opencode.svg"),
    ),
    (
        "ollama",
        include_str!("../../resources/providers/ollama.svg"),
    ),
    (
        "lmstudio",
        include_str!("../../resources/providers/lmstudio.svg"),
    ),
];

struct BrandImage {
    light: Arc<Image>,
    dark: Arc<Image>,
}

pub(super) struct ProviderIcons(BTreeMap<&'static str, BrandImage>);

impl Default for ProviderIcons {
    fn default() -> Self {
        Self(
            ASSETS
                .iter()
                .map(|&(family, svg)| {
                    (
                        family,
                        BrandImage {
                            light: image(svg, "#161616"),
                            dark: image(svg, "#F5F5F5"),
                        },
                    )
                })
                .collect(),
        )
    }
}

// Keep SVG source untouched; render at sufficient resolution for Retina displays.
fn image(svg: &str, foreground: &str) -> Arc<Image> {
    let bytes = svg
        .replace("currentColor", foreground)
        .replacen("width=\"1em\"", "width=\"64\"", 1)
        .replacen("height=\"1em\"", "height=\"64\"", 1)
        .into_bytes();
    Arc::new(Image::from_bytes(ImageFormat::Svg, bytes))
}

impl ProviderIcons {
    pub(super) fn render(&self, family: &str, cx: &App) -> Div {
        let frame = div()
            .size(px(24.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center();
        let Some(brand) = self.0.get(family) else {
            return frame.child(ui::icon(ui::IconName::Network).size(px(16.)));
        };
        let image = if cx.omarchy().appearance == ThemeAppearance::Dark {
            &brand.dark
        } else {
            &brand.light
        };
        // White Kimi strokes and OpenRouter's lime mark need a dark backing in both themes.
        frame
            .when(matches!(family, "kimi" | "openrouter"), |frame| {
                frame.bg(rgb(0x0018_181b))
            })
            .child(img(Arc::clone(image)).size(px(18.)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::SvgRenderer;

    #[test]
    fn every_branded_preset_has_a_renderable_embedded_logo_in_both_appearances() {
        let icons = ProviderIcons::default();
        let renderer = SvgRenderer::new(Arc::new(gpui_kit::assets::Assets));
        for family in provider_x_providers::presets() {
            if family.id == "custom" {
                continue;
            }
            let brand = icons.0.get(family.id).expect("preset logo");
            for source in [&brand.light, &brand.dark] {
                let raster = source.to_image_data(renderer.clone()).expect("valid SVG");
                let size = raster.size(0);
                assert!(size.width.0 >= 64, "{}", family.id);
                assert_eq!(size.width, size.height, "{}", family.id);
                let bytes = raster.as_bytes(0).unwrap();
                assert!(
                    bytes.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0),
                    "{}",
                    family.id
                );
                if matches!(
                    family.id,
                    "deepseek" | "kimi" | "qwen" | "minimax" | "openrouter"
                ) {
                    assert!(
                        bytes
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .any(|p| p[3] > 200 && (p[0] != p[1] || p[1] != p[2])),
                        "{} lost its brand colors",
                        family.id
                    );
                }
            }
        }
        assert!(!icons.0.contains_key("custom"));
    }
}
