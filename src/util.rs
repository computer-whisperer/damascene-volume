/// Extract the `name` field from a `{"name":"..."}` JSON blob — the
/// shape PipeWire stores in `default.audio.sink` / `default.audio.source`
/// on the `default` metadata, and the same shape some clients still
/// write into a stream's `target.object` prop.
///
/// Requires literal braces. A bare string without braces is rejected:
/// for `target.object`, the serial path covers `Spa:Id`-typed values
/// already, and a bare name would be ambiguous with garbage.
pub fn parse_name_json(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let key_pos = trimmed.find("\"name\"")?;
    let after = &trimmed[key_pos + 6..];
    let colon = after.find(':')?;
    let after_colon = &after[colon + 1..];
    let open = after_colon.find('"')?;
    let rest = &after_colon[open + 1..];
    let close = rest.find('"')?;
    Some(&rest[..close])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_name_json_only_accepts_braced_name() {
        // Canonical JSON form (matches what `wpctl set-default` writes).
        assert_eq!(
            parse_name_json(r#"{"name":"alsa_output.foo"}"#),
            Some("alsa_output.foo"),
        );
        // Whitespace tolerated.
        assert_eq!(
            parse_name_json(r#"{ "name": "alsa_output.foo" }"#),
            Some("alsa_output.foo"),
        );
        // Bare strings are NOT accepted — see the doc comment for why.
        assert_eq!(parse_name_json("alsa_output.bar"), None);
        assert_eq!(parse_name_json(r#"{"serial":42}"#), None);
        assert_eq!(parse_name_json(""), None);
    }
}
