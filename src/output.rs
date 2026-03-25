use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::IsTerminal;

use crate::config::OutputBodyStyle;

pub(crate) fn normalize_source_label(source_label: &str) -> String {
    source_label.replace("::", " ")
}

pub(crate) fn normalize_internal_log_label(source_label: &str) -> String {
    let normalized = normalize_source_label(source_label);
    if normalized.starts_with("devloop ") {
        normalized
    } else {
        format!("devloop {normalized}")
    }
}

pub(crate) fn format_output_prefix(source_label: &str, colorize: bool) -> String {
    format_output_prefix_with_style(source_label, colorize, OutputBodyStyle::Plain)
}

pub(crate) fn format_output_prefix_with_style(
    source_label: &str,
    colorize: bool,
    body_style: OutputBodyStyle,
) -> String {
    if !colorize {
        return format!("[{source_label}] ");
    }

    format!("{} ", colorize_label(source_label, body_style))
}

pub(crate) fn should_colorize_output() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

#[cfg(test)]
pub(crate) fn style_output_text(text: &str, body_style: OutputBodyStyle, colorize: bool) -> String {
    if !colorize {
        return text.to_owned();
    }

    match body_style {
        OutputBodyStyle::Plain => text.to_owned(),
        OutputBodyStyle::Dim => format!("\u{1b}[2m{text}\u{1b}[0m"),
    }
}

pub(crate) fn dim_start(colorize: bool) -> &'static str {
    if colorize { "\u{1b}[2m" } else { "" }
}

pub(crate) fn style_reset(colorize: bool) -> &'static str {
    if colorize { "\u{1b}[0m" } else { "" }
}

pub(crate) fn output_color_code(process_name: &str) -> u8 {
    const PALETTE: [u8; 5] = [31, 32, 33, 34, 36];
    let mut hasher = DefaultHasher::new();
    process_name.hash(&mut hasher);
    PALETTE[(hasher.finish() as usize) % PALETTE.len()]
}

fn colorize_label(source_label: &str, body_style: OutputBodyStyle) -> String {
    let dim = matches!(body_style, OutputBodyStyle::Dim);
    let dim_code = if dim { "2;" } else { "" };
    format!(
        "\u{1b}[{}1;{}m[{}]\u{1b}[0m",
        dim_code,
        output_color_code(source_label),
        source_label
    )
}

#[cfg(test)]
mod tests {
    use super::{
        dim_start, format_output_prefix, format_output_prefix_with_style,
        normalize_internal_log_label, normalize_source_label, output_color_code, style_output_text,
        style_reset,
    };
    use crate::config::OutputBodyStyle;

    #[test]
    fn normalize_source_label_rewrites_module_path() {
        assert_eq!(
            normalize_source_label("devloop::processes"),
            "devloop processes"
        );
    }

    #[test]
    fn normalize_internal_log_label_prefixes_non_devloop_targets() {
        assert_eq!(
            normalize_internal_log_label("hyper_util::client::legacy::connect::http"),
            "devloop hyper_util client legacy connect http"
        );
    }

    #[test]
    fn normalize_internal_log_label_keeps_devloop_targets_stable() {
        assert_eq!(
            normalize_internal_log_label("devloop::engine"),
            "devloop engine"
        );
    }

    #[test]
    fn format_output_prefix_falls_back_without_color() {
        assert_eq!(
            format_output_prefix("tunnel cloudflared", false),
            "[tunnel cloudflared] "
        );
    }

    #[test]
    fn format_output_prefix_colors_label() {
        let rendered = format_output_prefix("tunnel cloudflared", true);

        assert!(rendered.contains("[tunnel cloudflared]"));
        assert!(rendered.starts_with("\u{1b}[1;"));
        assert!(rendered.ends_with(" "));
    }

    #[test]
    fn format_output_prefix_can_dim_label() {
        let rendered =
            format_output_prefix_with_style("tunnel cloudflared", true, OutputBodyStyle::Dim);

        assert!(rendered.contains("[tunnel cloudflared]"));
        assert!(rendered.starts_with("\u{1b}[2;1;"));
        assert!(rendered.ends_with(" "));
    }

    #[test]
    fn output_color_code_is_stable_for_same_process() {
        assert_eq!(output_color_code("tunnel"), output_color_code("tunnel"));
    }

    #[test]
    fn output_color_palette_avoids_bright_white() {
        for source in [
            "server cargo",
            "css_watch tailwindcss",
            "tunnel cloudflared",
        ] {
            assert_ne!(output_color_code(source), 37);
        }
    }

    #[test]
    fn style_output_text_defaults_to_plain() {
        assert_eq!(
            style_output_text("ready", OutputBodyStyle::Plain, true),
            "ready"
        );
    }

    #[test]
    fn style_output_text_can_dim_body() {
        assert_eq!(
            style_output_text("ready", OutputBodyStyle::Dim, true),
            "\u{1b}[2mready\u{1b}[0m"
        );
    }

    #[test]
    fn dim_helpers_emit_sequences_when_colorized() {
        assert_eq!(dim_start(true), "\u{1b}[2m");
        assert_eq!(style_reset(true), "\u{1b}[0m");
    }
}
