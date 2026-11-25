//! Checks the generated consts against `docs/design/tokens.json` and checks
//! the usage rules in `docs/design/README.md` that can be measured.
//!
//! The JSON is read here with the shared reader and the values are decoded
//! by code that is independent of `build.rs`, so a decoding bug in one does
//! not hide a bug in the other.

#[path = "../support/json.rs"]
mod json;

use json::Value;
use tokens::{
    FontFamily, MONO_STACK, Palette, RADII, Rgba, SANS_STACK, SIZES, SPACINGS, TEXT_STYLES, Theme,
};

const TOKENS_JSON: &str = include_str!("../../../docs/design/tokens.json");

fn root() -> Value {
    json::parse(TOKENS_JSON).expect("tokens.json parses")
}

fn members<'a>(value: &'a Value, group: &str) -> &'a [Value] {
    value
        .get(group)
        .and_then(|g| g.get("tokens"))
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{group}.tokens"))
}

fn name(token: &Value) -> &str {
    token.get("name").and_then(Value::as_str).expect("name")
}

fn theme_id(theme: Theme) -> &'static str {
    match theme {
        Theme::Dark => "dark",
        Theme::Light => "light",
    }
}

/// The JSON colour for `token` in `theme`, following `{alias}` values.
fn json_color(tokens: &[Value], token: &Value, theme: Theme) -> Rgba {
    let raw = token
        .get("value")
        .and_then(|v| v.get(theme_id(theme)))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{} has no {} value", name(token), theme_id(theme)));
    if let Some(target) = raw.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        let target = tokens
            .iter()
            .find(|t| name(t) == target)
            .unwrap_or_else(|| panic!("alias {target} missing"));
        return json_color(tokens, target, theme);
    }
    let hex = raw.strip_prefix('#').expect("#rrggbb");
    assert_eq!(hex.len(), 6, "{raw}");
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex digit");
    Rgba::rgb(channel(0), channel(2), channel(4))
}

fn px(value: &str) -> f32 {
    value
        .strip_suffix("px")
        .expect("px value")
        .parse()
        .expect("number")
}

fn rust_color(theme: Theme, token: &str) -> Rgba {
    theme
        .palette()
        .colors()
        .iter()
        .find(|(n, _)| *n == token)
        .unwrap_or_else(|| panic!("{token} missing from Palette"))
        .1
}

fn token_value(theme: Theme, token: &str) -> Rgba {
    rust_color(theme, token)
}

fn ratio(theme: Theme, ink: &str, background: &str) -> f64 {
    token_value(theme, ink).contrast_ratio(token_value(theme, background))
}

// ---- JSON and Rust agree ----------------------------------------------

#[test]
fn every_color_token_has_the_json_value_in_both_themes() {
    let root = root();
    let tokens = members(&root, "color");
    assert!(!tokens.is_empty());
    for theme in Theme::ALL {
        for token in tokens {
            assert_eq!(
                rust_color(theme, name(token)),
                json_color(tokens, token, theme),
                "{} in {theme:?}",
                name(token)
            );
        }
    }
}

#[test]
fn rust_has_no_color_the_json_lacks() {
    let root = root();
    let json_names: Vec<&str> = members(&root, "color").iter().map(name).collect();
    assert_eq!(Palette::COLOR_NAMES.to_vec(), json_names);
}

#[test]
fn themes_are_dark_then_light_and_dark_is_default() {
    let root = root();
    let ids: Vec<&str> = root
        .get("color")
        .and_then(|c| c.get("themes"))
        .and_then(Value::as_array)
        .expect("themes")
        .iter()
        .map(|t| t.get("id").and_then(Value::as_str).expect("id"))
        .collect();
    assert_eq!(ids, ["dark", "light"]);
    assert_eq!(Theme::default(), Theme::Dark);
    assert_eq!(Theme::ALL, [Theme::Dark, Theme::Light]);
}

#[test]
fn light_and_dark_define_the_same_keys() {
    let root = root();
    for group in ["color", "shadow"] {
        for token in members(&root, group) {
            let value = token
                .get("value")
                .and_then(Value::as_object)
                .expect("value");
            let keys: Vec<&str> = value.iter().map(|(k, _)| k.as_str()).collect();
            assert_eq!(keys, ["dark", "light"], "{}", name(token));
        }
    }
    // Both generated palettes list the same tokens in the same order.
    let dark = Theme::Dark.palette().colors().map(|(n, _)| n);
    let light = Theme::Light.palette().colors().map(|(n, _)| n);
    assert_eq!(dark, light);
}

#[test]
fn focus_is_an_alias_of_accent() {
    for theme in Theme::ALL {
        let p = theme.palette();
        assert_eq!(p.focus, p.accent);
    }
}

fn scalar_group(root: &Value, group: &str, rust: &[(&str, f32)]) {
    let tokens = members(root, group);
    assert_eq!(tokens.len(), rust.len(), "{group} count");
    for (token, (rust_name, rust_value)) in tokens.iter().zip(rust) {
        assert_eq!(name(token), *rust_name);
        let json = px(token.get("value").and_then(Value::as_str).expect("value"));
        assert_eq!(*rust_value, json, "{group} {rust_name}");
    }
}

#[test]
fn spacing_radius_and_size_match_the_json() {
    let root = root();
    scalar_group(&root, "spacing", &SPACINGS);
    scalar_group(&root, "radius", &RADII);
    scalar_group(&root, "size", &SIZES);
    // Spot-check the named consts, not just the lists.
    assert_eq!(tokens::SPACE_4, 16.0);
    assert_eq!(tokens::RADIUS_MD, 6.0);
    assert_eq!(tokens::ROW_LIST, 28.0);
    assert_eq!(tokens::LANE, 14.0);
    assert_eq!(tokens::DIALOG, 520.0);
    assert_eq!(tokens::NAME_COLUMN, 260.0);
}

#[test]
fn type_styles_match_the_json() {
    let root = root();
    let mut json_styles = Vec::new();
    for group in root
        .get("type")
        .and_then(|t| t.get("groups"))
        .and_then(Value::as_array)
        .expect("groups")
    {
        let family = group.get("family").and_then(Value::as_str).expect("family");
        for style in group
            .get("styles")
            .and_then(Value::as_array)
            .expect("styles")
        {
            json_styles.push((family.to_string(), style.clone()));
        }
    }
    assert_eq!(json_styles.len(), TEXT_STYLES.len());
    for ((family, style), rust) in json_styles.iter().zip(TEXT_STYLES) {
        let str_of = |key: &str| style.get(key).and_then(Value::as_str);
        assert_eq!(str_of("name"), Some(rust.name));
        assert_eq!(
            rust.family,
            if family == "sans" {
                FontFamily::Sans
            } else {
                FontFamily::Mono
            }
        );
        assert_eq!(
            px(str_of("fontSize").unwrap()),
            rust.font_size,
            "{}",
            rust.name
        );
        assert_eq!(
            px(str_of("lineHeight").unwrap()),
            rust.line_height,
            "{}",
            rust.name
        );
        assert_eq!(
            style.get("fontWeight").and_then(Value::as_f64).unwrap() as u16,
            rust.font_weight,
            "{}",
            rust.name
        );
        let spacing = str_of("letterSpacing")
            .map(|s| s.strip_suffix("em").unwrap().parse::<f32>().unwrap())
            .unwrap_or(0.0);
        assert_eq!(spacing, rust.letter_spacing_em, "{}", rust.name);
    }
}

#[test]
fn font_stacks_match_the_json() {
    let root = root();
    let families = root
        .get("type")
        .and_then(|t| t.get("families"))
        .expect("families");
    for (key, stack) in [("sans", &SANS_STACK[..]), ("mono", &MONO_STACK[..])] {
        let joined = families.get(key).and_then(Value::as_str).expect("family");
        let expected: Vec<String> = joined
            .split(',')
            .map(|n| n.trim().trim_matches('"').to_string())
            .collect();
        assert_eq!(stack, expected.as_slice(), "{key}");
    }
}

/// Numbers in `text`, in order, from `[0-9.]` runs.
fn numbers(text: &str) -> Vec<f32> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().expect("number"))
        .collect()
}

#[test]
fn shadows_match_the_json() {
    let root = root();
    let tokens = members(&root, "shadow");
    assert_eq!(tokens.len(), 1);
    assert_eq!(name(&tokens[0]), "shadow-overlay");
    for theme in Theme::ALL {
        let raw = tokens[0]
            .get("value")
            .and_then(|v| v.get(theme_id(theme)))
            .and_then(Value::as_str)
            .unwrap();
        let layers = theme.palette().shadow_overlay.layers;
        // Layers are separated by "), " because the colour ends each layer.
        let texts: Vec<&str> = raw.split("), ").collect();
        assert_eq!(layers.len(), texts.len());
        for (layer, text) in layers.iter().zip(texts) {
            let (lengths, color) = text.split_once("rgba(").expect("rgba");
            let mut l = numbers(lengths);
            l.resize(4, 0.0);
            assert_eq!(
                [layer.offset_x, layer.offset_y, layer.blur, layer.spread],
                [l[0], l[1], l[2], l[3]]
            );
            let c = numbers(color);
            assert_eq!(
                [layer.color.r, layer.color.g, layer.color.b],
                [c[0] as u8, c[1] as u8, c[2] as u8]
            );
            assert_eq!(layer.color.a, c[3]);
        }
    }
}

// ---- README rules -------------------------------------------------------

const SURFACES: [&str; 4] = ["bg-app", "bg-panel", "bg-raised", "bg-hover"];

#[test]
fn text_meets_4_5_on_every_surface() {
    for theme in Theme::ALL {
        for ink in ["ink", "ink-muted"] {
            for bg in SURFACES {
                let r = ratio(theme, ink, bg);
                assert!(r >= 4.5, "{ink} on {bg} in {theme:?}: {r:.2}");
            }
        }
        // ink-faint (line numbers, placeholders) is only promised on bg-panel.
        let r = ratio(theme, "ink-faint", "bg-panel");
        assert!(r >= 4.5, "ink-faint on bg-panel in {theme:?}: {r:.2}");
    }
}

#[test]
fn control_borders_meet_3_to_1_on_both_panels() {
    for theme in Theme::ALL {
        for bg in ["bg-app", "bg-panel"] {
            let r = ratio(theme, "line-control", bg);
            assert!(r >= 3.0, "line-control on {bg} in {theme:?}: {r:.2}");
        }
    }
}

#[test]
fn ink_is_readable_on_every_diff_background() {
    for theme in Theme::ALL {
        for bg in ["add-bg", "add-bg-strong", "remove-bg", "remove-bg-strong"] {
            let r = ratio(theme, "ink", bg);
            assert!(r >= 4.5, "ink on {bg} in {theme:?}: {r:.2}");
        }
    }
}

#[test]
fn diff_signs_are_readable_on_their_lines_and_differ_in_lightness() {
    for theme in Theme::ALL {
        for (ink, bg) in [("add-ink", "add-bg"), ("remove-ink", "remove-bg")] {
            for surface in [bg, "bg-panel"] {
                let r = ratio(theme, ink, surface);
                assert!(r >= 4.5, "{ink} on {surface} in {theme:?}: {r:.2}");
            }
        }
        // "The two inks also differ in lightness, not only hue."
        // The README gives no number; 1.2:1 between the two inks is the bar
        // chosen here (the current light pair is 1.26, the dark pair more).
        let r = ratio(theme, "add-ink", "remove-ink");
        assert!(
            r >= 1.2,
            "add-ink and remove-ink too close in {theme:?}: {r:.2}"
        );
    }
}

#[test]
fn action_and_state_colors_are_readable() {
    for theme in Theme::ALL {
        let pairs = [
            ("on-accent", "accent"),
            ("on-danger", "danger"),
            ("accent", "bg-panel"),
            ("accent", "accent-soft"),
            ("danger", "bg-panel"),
            ("warning", "bg-panel"),
            ("warning", "warning-soft"),
        ];
        for (ink, bg) in pairs {
            let r = ratio(theme, ink, bg);
            assert!(r >= 4.5, "{ink} on {bg} in {theme:?}: {r:.2}");
        }
        // The focus ring is not text but must stand out from the surface.
        let r = ratio(theme, "focus", "bg-panel");
        assert!(r >= 3.0, "focus on bg-panel in {theme:?}: {r:.2}");
    }
}

#[test]
fn lane_colors_meet_3_to_1_on_the_panel_and_are_distinct() {
    for theme in Theme::ALL {
        let p = theme.palette();
        let lanes: Vec<Rgba> = (0..8).map(|i| p.lane(i)).collect();
        for (i, lane) in lanes.iter().enumerate() {
            let r = lane.contrast_ratio(p.bg_panel);
            assert!(r >= 3.0, "lane-{} in {theme:?}: {r:.2}", i + 1);
            for other in &lanes[i + 1..] {
                assert_ne!(lane, other, "duplicate lane colour in {theme:?}");
            }
        }
    }
}

#[test]
fn spacing_is_a_4px_grid_in_increasing_order() {
    let mut previous = 0.0;
    for (name, value) in SPACINGS {
        assert_eq!(value % 4.0, 0.0, "{name} is off the 4px grid");
        assert!(value > previous, "{name} is not increasing");
        previous = value;
    }
}

#[test]
fn radii_increase_from_sm_to_lg() {
    let names: Vec<&str> = RADII.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, ["radius-sm", "radius-md", "radius-lg"]);
    assert!(RADII.windows(2).all(|w| w[0].1 < w[1].1));
}

#[test]
fn type_rules() {
    for style in TEXT_STYLES {
        // "Nothing smaller than 11px, and 11px only for uppercase section labels."
        assert!(style.font_size >= 11.0, "{} is below 11px", style.name);
        if style.font_size < 12.0 {
            assert_eq!(style.name, "label");
        }
        assert!(style.line_height >= style.font_size, "{}", style.name);
    }
    // "13px text on 28px rows."
    assert_eq!(tokens::TEXT_BODY.font_size, 13.0);
    assert_eq!(tokens::ROW_LIST, 28.0);
    // "Diff lines use code on row-diff rows; line height equals row height."
    assert_eq!(tokens::TEXT_CODE.line_height, tokens::ROW_DIFF);
    assert_eq!(tokens::TEXT_CODE.family, FontFamily::Mono);
    assert_eq!(tokens::TEXT_BODY.family, FontFamily::Sans);
    // Interface styles use sans, code styles use mono.
    assert_eq!(tokens::TEXT_CODE_SMALL.family, FontFamily::Mono);
    assert_eq!(FontFamily::Sans.stack()[0], "Inter");
    assert_eq!(FontFamily::Mono.stack()[0], "JetBrains Mono");
}

#[test]
fn only_overlays_have_a_shadow_token() {
    // "Lines, not shadows": the single shadow token is for floating things.
    let root = root();
    let tokens = members(&root, "shadow");
    assert_eq!(tokens.len(), 1);
    assert!(
        tokens[0]
            .get("usage")
            .and_then(Value::as_str)
            .unwrap()
            .contains("Menus, popovers and dialogs only")
    );
}

// ---- the JSON reader ----------------------------------------------------

mod reader {
    use super::json::{Value, parse};
    use proptest::prelude::*;

    fn quote(s: &str) -> String {
        let mut out = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c if (c as u32) > 0xFFFF => {
                    let v = c as u32 - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xD800 + (v >> 10),
                        0xDC00 + (v & 0x3FF)
                    ));
                }
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    fn render(value: &Value) -> String {
        match value {
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => format!("{n:?}"),
            Value::String(s) => quote(s),
            Value::Array(items) => {
                format!(
                    "[{}]",
                    items.iter().map(render).collect::<Vec<_>>().join(", ")
                )
            }
            Value::Object(members) => format!(
                "{{{}}}",
                members
                    .iter()
                    .map(|(k, v)| format!("{}: {}", quote(k), render(v)))
                    .collect::<Vec<_>>()
                    .join(",\n")
            ),
        }
    }

    fn value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            (-1.0e6f64..1.0e6).prop_map(Value::Number),
            any::<String>().prop_map(Value::String),
        ];
        leaf.prop_recursive(4, 32, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::vec((any::<String>(), inner), 0..4).prop_map(Value::Object),
            ]
        })
    }

    proptest! {
        #[test]
        fn render_then_parse_round_trips(v in value()) {
            prop_assert_eq!(parse(&render(&v)), Ok(v));
        }

        #[test]
        fn garbage_never_panics(s in "\\PC{0,40}") {
            let _ = parse(&s);
        }

        #[test]
        fn truncated_documents_are_errors(v in value(), cut in 0usize..1000) {
            let text = render(&v);
            let at = text.char_indices().map(|(i, _)| i).nth(cut % (text.chars().count().max(1))).unwrap_or(0);
            // Any strict prefix of an object or array is invalid JSON.
            if matches!(v, Value::Array(_) | Value::Object(_)) && at < text.len() {
                prop_assert!(parse(&text[..at]).is_err());
            }
        }
    }

    #[test]
    fn decodes_every_escape() {
        let parsed = parse(r#""a\"b\\c\/d\be\ff\ng\rh\ti·j😀""#).unwrap();
        assert_eq!(
            parsed,
            Value::String("a\"b\\c/d\u{8}e\u{c}f\ng\rh\ti\u{b7}j\u{1f600}".into())
        );
        assert_eq!(
            parse("\u{feff}[1, 2.5e1]")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\" 1}",
            "\"\\ud800\"",
            "1 2",
            "\"a\nb\"",
            "nul",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should fail");
        }
    }
}
