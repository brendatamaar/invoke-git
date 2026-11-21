//! Generates `$OUT_DIR/generated.rs` from `docs/design/tokens.json`.
//!
//! The JSON is the source of truth, so the Rust consts can never drift from
//! it: the build reads it every time. Any malformed token fails the build
//! with a message naming the token.

#[path = "support/json.rs"]
mod json;

use json::Value;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::{env, fs};

const TOKENS_JSON: &str = "../../docs/design/tokens.json";

fn main() {
    println!("cargo:rerun-if-changed={TOKENS_JSON}");
    println!("cargo:rerun-if-changed=support/json.rs");
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let path = manifest.join(TOKENS_JSON);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {} failed: {error}", path.display()));
    let root = json::parse(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let code = generate(&root).unwrap_or_else(|error| panic!("tokens.json: {error}"));
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("generated.rs");
    fs::write(&out, code).unwrap_or_else(|error| panic!("write {} failed: {error}", out.display()));
}

fn field<'a>(value: &'a Value, key: &str, what: &str) -> Result<&'a Value, String> {
    value
        .get(key)
        .ok_or_else(|| format!("{what}: missing \"{key}\""))
}

fn text<'a>(value: &'a Value, key: &str, what: &str) -> Result<&'a str, String> {
    field(value, key, what)?
        .as_str()
        .ok_or_else(|| format!("{what}: \"{key}\" must be a string"))
}

fn token_list<'a>(root: &'a Value, group: &str) -> Result<&'a [Value], String> {
    field(field(root, group, "root")?, "tokens", group)?
        .as_array()
        .ok_or_else(|| format!("{group}.tokens must be an array"))
}

/// `bg-app` to `bg_app`.
fn snake(name: &str) -> String {
    name.replace('-', "_")
}

fn upper(name: &str) -> String {
    snake(name).to_uppercase()
}

fn float(value: f64) -> String {
    format!("{:?}", value as f32)
}

fn hex_color(token: &str, hex: &str) -> Result<String, String> {
    let digits = hex
        .strip_prefix('#')
        .filter(|d| d.len() == 6 && d.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| format!("{token}: \"{hex}\" is not #rrggbb"))?;
    let byte = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).expect("checked hex");
    Ok(format!("Rgba::rgb({}, {}, {})", byte(0), byte(2), byte(4)))
}

fn px(token: &str, value: &str) -> Result<f64, String> {
    value
        .strip_suffix("px")
        .and_then(|n| n.trim().parse::<f64>().ok())
        .ok_or_else(|| format!("{token}: \"{value}\" is not <number>px"))
}

fn length(token: &str, value: &str) -> Result<f64, String> {
    if value == "0" {
        Ok(0.0)
    } else {
        px(token, value)
    }
}

/// `0 8px 24px rgba(0,0,0,0.5), 0 0 0 1px rgba(...)` to a `Shadow` literal.
fn shadow(token: &str, value: &str) -> Result<String, String> {
    let mut layers = Vec::new();
    for layer in split_top_level(value, ',') {
        let parts = split_top_level(layer.trim(), ' ')
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>();
        let (color, lengths) = parts
            .split_last()
            .ok_or_else(|| format!("{token}: empty shadow layer"))?;
        if !(2..=4).contains(&lengths.len()) {
            return Err(format!("{token}: shadow layer needs 2 to 4 lengths"));
        }
        let mut numbers = [0.0; 4];
        for (slot, part) in numbers.iter_mut().zip(lengths) {
            *slot = length(token, part)?;
        }
        let inner = color
            .strip_prefix("rgba(")
            .and_then(|c| c.strip_suffix(')'))
            .ok_or_else(|| format!("{token}: shadow colour must be rgba(r,g,b,a)"))?;
        let channels = inner.split(',').map(str::trim).collect::<Vec<_>>();
        if channels.len() != 4 {
            return Err(format!("{token}: rgba() needs 4 values"));
        }
        let channel = |i: usize| {
            channels[i]
                .parse::<u8>()
                .map_err(|_| format!("{token}: bad rgba channel \"{}\"", channels[i]))
        };
        let alpha = channels[3]
            .parse::<f64>()
            .map_err(|_| format!("{token}: bad rgba alpha"))?;
        layers.push(format!(
            "ShadowLayer {{ offset_x: {}, offset_y: {}, blur: {}, spread: {}, color: Rgba::new({}, {}, {}, {}) }}",
            float(numbers[0]),
            float(numbers[1]),
            float(numbers[2]),
            float(numbers[3]),
            channel(0)?,
            channel(1)?,
            channel(2)?,
            float(alpha),
        ));
    }
    Ok(format!("Shadow {{ layers: &[{}] }}", layers.join(", ")))
}

/// Splits on `separator` outside parentheses.
fn split_top_level(value: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    for (at, c) in value.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c == separator && depth == 0 => {
                parts.push(&value[start..at]);
                start = at + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

/// Resolves `{other}` aliases within one theme, following chains.
fn resolve_color(
    tokens: &[Value],
    theme: &str,
    name: &str,
    raw: &str,
    depth: usize,
) -> Result<String, String> {
    let Some(target) = raw.strip_prefix('{').and_then(|r| r.strip_suffix('}')) else {
        return Ok(raw.to_string());
    };
    if depth > 8 {
        return Err(format!("{name}: alias chain too long or cyclic"));
    }
    let token = tokens
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(target))
        .ok_or_else(|| format!("{name}: alias target \"{target}\" does not exist"))?;
    let next = text(field(token, "value", target)?, theme, target)?;
    resolve_color(tokens, theme, target, next, depth + 1)
}

fn generate(root: &Value) -> Result<String, String> {
    let mut out = String::from("// @generated by build.rs from docs/design/tokens.json\n\n");

    // Themes: dark first, as the design README requires.
    let themes = field(field(root, "color", "root")?, "themes", "color")?
        .as_array()
        .ok_or("color.themes must be an array")?;
    let theme_ids = themes
        .iter()
        .map(|t| text(t, "id", "theme"))
        .collect::<Result<Vec<_>, _>>()?;
    if theme_ids != ["dark", "light"] {
        return Err(format!("themes must be [dark, light], found {theme_ids:?}"));
    }

    // Colours and shadows are per theme, so they live in `Palette`.
    let colors = token_list(root, "color")?;
    let shadows = token_list(root, "shadow")?;
    let color_names = colors
        .iter()
        .map(|t| text(t, "name", "colour token"))
        .collect::<Result<Vec<_>, _>>()?;
    let shadow_names = shadows
        .iter()
        .map(|t| text(t, "name", "shadow token"))
        .collect::<Result<Vec<_>, _>>()?;

    out.push_str("/// Every colour of one theme, plus its shadows.\n");
    out.push_str("#[derive(Debug, Clone, Copy, PartialEq)]\npub struct Palette {\n");
    for name in &color_names {
        writeln!(out, "    pub {}: Rgba,", snake(name)).unwrap();
    }
    for name in &shadow_names {
        writeln!(out, "    pub {}: Shadow,", snake(name)).unwrap();
    }
    out.push_str("}\n\n");

    out.push_str("impl Palette {\n");
    out.push_str("    /// Names of every colour token, in `tokens.json` order.\n");
    writeln!(
        out,
        "    pub const COLOR_NAMES: [&'static str; {}] = [{}];\n",
        color_names.len(),
        color_names
            .iter()
            .map(|n| format!("{n:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
    .unwrap();
    out.push_str("    /// Every colour as `(token name, value)`, in `tokens.json` order.\n");
    writeln!(
        out,
        "    pub const fn colors(&self) -> [(&'static str, Rgba); {}] {{\n        [",
        color_names.len()
    )
    .unwrap();
    for name in &color_names {
        writeln!(out, "            ({name:?}, self.{}),", snake(name)).unwrap();
    }
    out.push_str("        ]\n    }\n}\n\n");

    for theme in &theme_ids {
        writeln!(
            out,
            "pub const {}: Palette = Palette {{",
            theme.to_uppercase()
        )
        .unwrap();
        for token in colors {
            let name = text(token, "name", "colour token")?;
            let raw = text(field(token, "value", name)?, theme, name)?;
            let resolved = resolve_color(colors, theme, name, raw, 0)?;
            writeln!(out, "    {}: {},", snake(name), hex_color(name, &resolved)?).unwrap();
        }
        for token in shadows {
            let name = text(token, "name", "shadow token")?;
            let raw = text(field(token, "value", name)?, theme, name)?;
            writeln!(out, "    {}: {},", snake(name), shadow(name, raw)?).unwrap();
        }
        out.push_str("};\n\n");
    }

    // Scalar families: spacing, radius, size. All `<n>px`, emitted as f32.
    for (group, array_name) in [
        ("spacing", "SPACINGS"),
        ("radius", "RADII"),
        ("size", "SIZES"),
    ] {
        let tokens = token_list(root, group)?;
        let mut entries = Vec::new();
        for token in tokens {
            let name = text(token, "name", group)?;
            let value = px(name, text(token, "value", name)?)?;
            writeln!(out, "pub const {}: f32 = {};", upper(name), float(value)).unwrap();
            entries.push(format!("({name:?}, {})", upper(name)));
        }
        writeln!(
            out,
            "/// Every `{group}` token as `(name, px)`, in `tokens.json` order.\npub const {array_name}: [(&str, f32); {}] = [{}];\n",
            entries.len(),
            entries.join(", ")
        )
        .unwrap();
    }

    // Type.
    let type_group = field(root, "type", "root")?;
    let families = field(type_group, "families", "type")?;
    for family in ["sans", "mono"] {
        let stack = text(families, family, "type.families")?;
        let names = split_top_level(stack, ',')
            .into_iter()
            .map(|n| format!("{:?}", n.trim().trim_matches('"')))
            .collect::<Vec<_>>();
        writeln!(
            out,
            "pub const {}_STACK: [&str; {}] = [{}];",
            family.to_uppercase(),
            names.len(),
            names.join(", ")
        )
        .unwrap();
    }
    out.push('\n');
    let mut styles = Vec::new();
    for group in field(type_group, "groups", "type")?
        .as_array()
        .ok_or("type.groups must be an array")?
    {
        let family = match text(group, "family", "type group")? {
            "sans" => "FontFamily::Sans",
            "mono" => "FontFamily::Mono",
            other => return Err(format!("unknown font family \"{other}\"")),
        };
        for style in field(group, "styles", "type group")?
            .as_array()
            .ok_or("type group styles must be an array")?
        {
            let name = text(style, "name", "type style")?;
            let size = px(name, text(style, "fontSize", name)?)?;
            let line = px(name, text(style, "lineHeight", name)?)?;
            let weight = field(style, "fontWeight", name)?
                .as_f64()
                .ok_or_else(|| format!("{name}: fontWeight must be a number"))?;
            let spacing = match style.get("letterSpacing") {
                None => 0.0,
                Some(v) => v
                    .as_str()
                    .and_then(|s| s.strip_suffix("em"))
                    .and_then(|n| n.parse::<f64>().ok())
                    .ok_or_else(|| format!("{name}: letterSpacing must be <number>em"))?,
            };
            writeln!(
                out,
                "pub const TEXT_{}: TextStyle = TextStyle {{ name: {name:?}, family: {family}, font_size: {}, line_height: {}, font_weight: {}, letter_spacing_em: {} }};",
                upper(name),
                float(size),
                float(line),
                weight as u16,
                float(spacing),
            )
            .unwrap();
            styles.push(format!("TEXT_{}", upper(name)));
        }
    }
    writeln!(
        out,
        "\n/// Every text style, in `tokens.json` order.\npub const TEXT_STYLES: [TextStyle; {}] = [{}];",
        styles.len(),
        styles.join(", ")
    )
    .unwrap();
    Ok(out)
}
