//! Build-time catalog validation, also exercised by the application tests.

use std::collections::{BTreeMap, BTreeSet};

use super::template::{self, Part};

pub struct Locale {
    pub code: String,
    pub native_name: String,
    pub file: String,
    pub aliases: Vec<String>,
    pub fallback: Option<String>,
    pub messages: BTreeMap<String, String>,
}

pub fn load(
    registry: &str,
    mut read: impl FnMut(&str) -> Result<String, String>,
) -> Result<Vec<Locale>, String> {
    let registry: serde_json::Value = serde_json::from_str(registry).map_err(|e| e.to_string())?;
    let registry = registry
        .as_array()
        .ok_or("locales.json must contain an array")?;
    if registry.is_empty() {
        return Err("the locale registry is empty".into());
    }
    let mut locales = Vec::new();
    let mut names = BTreeSet::new();
    for value in registry {
        let text = |key| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("locale entry needs a nonempty {key}"))
        };
        let code = text("code")?;
        let native_name = text("native_name")?;
        let file = text("file")?;
        if !file.ends_with(".json") || file.contains(['/', '\\']) || file.starts_with('.') {
            return Err(format!(
                "{code}: file must be a JSON filename inside locales/"
            ));
        }
        let aliases = match value.get("aliases") {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .ok_or("aliases must be an array")?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "aliases must contain strings".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        for name in std::iter::once(&code).chain(aliases.iter()) {
            if !valid_code(name) || !names.insert(name.to_ascii_lowercase()) {
                return Err(format!("invalid or duplicate locale code/alias: {name}"));
            }
        }
        let fallback = value
            .get("fallback")
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "fallback must be a locale code".to_owned())
            })
            .transpose()?;
        let messages: BTreeMap<String, String> =
            serde_json::from_str(&read(&file)?).map_err(|e| format!("{file}: {e}"))?;
        for (key, text) in &messages {
            if key.trim().is_empty() || text.trim().is_empty() {
                return Err(format!("{file}: empty message key or value: {key}"));
            }
            placeholders(text).map_err(|e| format!("{file}: {key}: {e}"))?;
        }
        locales.push(Locale {
            code,
            native_name,
            file,
            aliases,
            fallback,
            messages,
        });
    }
    let english = locales
        .iter()
        .find(|locale| locale.code == "en")
        .ok_or("the registry must include the English fallback (en)")?;
    if english.messages.is_empty() {
        return Err("the English fallback must contain messages".into());
    }
    if !locales.iter().any(|locale| locale.code == "zh-CN") {
        return Err("the registry must retain the existing zh-CN locale".into());
    }
    for locale in &locales {
        for (key, text) in &locale.messages {
            let base = english
                .messages
                .get(key)
                .ok_or_else(|| format!("{}: {key} has no English source", locale.file))?;
            if placeholders(text)? != placeholders(base)? {
                return Err(format!(
                    "{}: {key} placeholders differ from English",
                    locale.file
                ));
            }
        }
        let mut seen = BTreeSet::new();
        let mut current = locale;
        while let Some(fallback) = &current.fallback {
            if !seen.insert(&current.code) {
                return Err(format!("{}: fallback cycle", locale.code));
            }
            current = locales
                .iter()
                .find(|candidate| &candidate.code == fallback)
                .ok_or_else(|| format!("{}: unknown fallback {fallback}", locale.code))?;
        }
    }
    Ok(locales)
}

fn valid_code(code: &str) -> bool {
    code.len() <= 64
        && code.split('-').enumerate().all(|(index, part)| {
            !part.is_empty()
                && part.len() <= 8
                && part.bytes().all(|byte| {
                    if index == 0 {
                        byte.is_ascii_alphabetic()
                    } else {
                        byte.is_ascii_alphanumeric()
                    }
                })
        })
}

fn placeholders(text: &str) -> Result<BTreeSet<&str>, String> {
    Ok(template::parse(text)?
        .into_iter()
        .filter_map(|part| match part {
            Part::Argument(name) => Some(name),
            Part::Text(text) => {
                debug_assert!(!text.is_empty());
                None
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = r#"[
        {"code":"en","native_name":"English","file":"en.json"},
        {"code":"zh-CN","native_name":"简体中文","file":"zh-CN.json"}
    ]"#;

    fn load_test(en: &str, zh: &str) -> Result<Vec<Locale>, String> {
        load(REGISTRY, |file| {
            Ok(if file == "en.json" { en } else { zh }.into())
        })
    }

    #[test]
    fn translations_can_fall_back_but_cannot_change_named_parameters() {
        let en = r#"{"notice":"{name} used {amount}","missing":"English fallback"}"#;
        assert!(load_test(en, r#"{"notice":"{amount} / {name}"}"#).is_ok());
        assert!(load_test(en, r#"{"notice":"{name} 使用了 {bytes}"}"#).is_err());
        assert!(load_test(en, r#"{"unknown":"未知"}"#).is_err());
        assert!(load_test(r#"{"notice":"{name"}"#, "{}").is_err());
        assert!(load_test("{}", "{}").is_err());
        assert!(load_test(r#"{"notice":""}"#, "{}").is_err());
        let locales = load_test(en, "{}").unwrap();
        assert_eq!(locales[0].native_name, "English");
        assert!(locales[0].aliases.is_empty());
    }

    #[test]
    fn registry_rejects_alias_collisions_and_fallback_cycles() {
        let duplicate = REGISTRY.replace("\"English\"", "\"English\",\"aliases\":[\"ZH-cn\"]");
        assert!(load(&duplicate, |_| Ok(r#"{"notice":"text"}"#.into())).is_err());
        let cycle = REGISTRY
            .replace("\"English\"", "\"English\",\"fallback\":\"zh-CN\"")
            .replace("\"简体中文\"", "\"简体中文\",\"fallback\":\"en\"");
        assert!(load(&cycle, |_| Ok(r#"{"notice":"text"}"#.into())).is_err());
    }
}
