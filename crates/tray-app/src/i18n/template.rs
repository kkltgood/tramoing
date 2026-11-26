//! Named placeholders shared by the build-time validator and runtime formatter.

#[derive(Debug, PartialEq, Eq)]
pub enum Part<'a> {
    Text(&'a str),
    Argument(&'a str),
}

/// Braces are escaped as `{{` and `}}`; argument names contain ASCII letters,
/// digits and underscores. Parsing the template once prevents argument contents
/// from becoming additional placeholders.
pub fn parse(template: &str) -> Result<Vec<Part<'_>>, String> {
    let mut parts = Vec::new();
    let mut offset = 0;
    let bytes = template.as_bytes();
    while offset < bytes.len() {
        let remaining = &template[offset..];
        let Some(relative) = remaining.find(['{', '}']) else {
            parts.push(Part::Text(remaining));
            break;
        };
        let at = offset + relative;
        if at > offset {
            parts.push(Part::Text(&template[offset..at]));
        }
        if bytes.get(at + 1) == Some(&bytes[at]) {
            parts.push(Part::Text(&template[at..at + 1]));
            offset = at + 2;
        } else if bytes[at] == b'{' {
            let end = template[at + 1..]
                .find('}')
                .map(|index| at + 1 + index)
                .ok_or_else(|| "unclosed placeholder".to_owned())?;
            let name = &template[at + 1..end];
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || name.as_bytes()[0].is_ascii_digit()
            {
                return Err(format!("invalid placeholder {{{name}}}"));
            }
            parts.push(Part::Argument(name));
            offset = end + 1;
        } else {
            return Err("unescaped closing brace".to_owned());
        }
    }
    Ok(parts)
}
