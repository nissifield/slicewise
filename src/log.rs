//! Logfmt line helper (design §8). Callers must never pass environment values.

/// Print one logfmt line to stderr.
pub fn emit(level: &str, event: &str, fields: &[(&str, &str)]) {
    eprintln!("{}", line(level, event, fields));
}

/// `level=... event=... key=value...` with the keys in caller order.
fn line(level: &str, event: &str, fields: &[(&str, &str)]) -> String {
    let mut out = format!("level={} event={}", quote(level), quote(event));
    for (key, value) in fields {
        out.push(' ');
        out.push_str(key);
        out.push('=');
        out.push_str(&quote(value));
    }
    out
}

fn quote(value: &str) -> String {
    let plain = !value.is_empty()
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '=' || c.is_control());
    if plain {
        return value.to_string();
    }
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn puts_level_event_then_keys_in_caller_order() {
        let line = line(
            "info",
            "brake_on",
            &[
                ("to", "6549012480"),
                ("target", "app-slicewise-batch.slice"),
            ],
        );
        assert_eq!(
            line,
            "level=info event=brake_on to=6549012480 target=app-slicewise-batch.slice"
        );
    }

    #[test]
    fn quotes_spaces_quotes_equals_empty_and_control() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("two words"), "\"two words\"");
        assert_eq!(quote("a=b"), "\"a=b\"");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote("a\nb"), "\"a\\nb\"");
        assert_eq!(quote("back\\slash"), "back\\slash");
    }

    #[test]
    fn reason_with_spaces_is_quoted() {
        let line = line(
            "info",
            "brake_on",
            &[("reason", "pool headroom 498M < margin 614M")],
        );
        assert_eq!(
            line,
            "level=info event=brake_on reason=\"pool headroom 498M < margin 614M\""
        );
    }
}
