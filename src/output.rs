use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::IsTerminal;

pub(crate) fn normalize_source_label(source_label: &str) -> String {
    source_label.replace("::", " ")
}

pub(crate) fn format_output_prefix(source_label: &str, colorize: bool) -> String {
    if !colorize {
        return format!("[{source_label}] ");
    }

    format!("{} ", colorize_label(source_label))
}

pub(crate) fn should_colorize_output() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

pub(crate) fn dim_text(text: &str) -> String {
    format!("\u{1b}[2m{text}\u{1b}[0m")
}

pub(crate) fn output_color_code(process_name: &str) -> u8 {
    const PALETTE: [u8; 6] = [31, 32, 33, 34, 36, 37];
    let mut hasher = DefaultHasher::new();
    process_name.hash(&mut hasher);
    PALETTE[(hasher.finish() as usize) % PALETTE.len()]
}

fn colorize_label(source_label: &str) -> String {
    format!(
        "\u{1b}[1;{}m[{}]\u{1b}[0m",
        output_color_code(source_label),
        source_label
    )
}

#[cfg(test)]
mod tests {
    use super::{dim_text, format_output_prefix, normalize_source_label, output_color_code};

    #[test]
    fn normalize_source_label_rewrites_module_path() {
        assert_eq!(
            normalize_source_label("devloop::processes"),
            "devloop processes"
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
    fn dim_text_wraps_ansi_dim_sequence() {
        assert_eq!(dim_text("ready"), "\u{1b}[2mready\u{1b}[0m");
    }

    #[test]
    fn output_color_code_is_stable_for_same_process() {
        assert_eq!(output_color_code("tunnel"), output_color_code("tunnel"));
    }
}
