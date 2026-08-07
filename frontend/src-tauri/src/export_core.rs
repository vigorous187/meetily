pub(crate) fn sanitize_markdown_name(value: &str) -> String {
    let mut sanitized = String::new();
    let mut previous_was_separator = false;

    for character in value.trim().chars() {
        if character.is_alphanumeric() || character == '_' {
            sanitized.push(character);
            previous_was_separator = false;
        } else if !previous_was_separator {
            sanitized.push('-');
            previous_was_separator = true;
        }
    }

    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "meeting-notes".to_string()
    } else {
        sanitized.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_markdown_name;

    #[test]
    fn sanitizes_titles_for_cross_platform_file_names() {
        assert_eq!(
            sanitize_markdown_name("  Product / Design: Weekly?  "),
            "Product-Design-Weekly"
        );
        assert_eq!(sanitize_markdown_name("../../"), "meeting-notes");
        assert_eq!(sanitize_markdown_name("Résumé 2026"), "Résumé-2026");
    }
}
