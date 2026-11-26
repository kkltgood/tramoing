//! Embedded translations and language selection. Language preferences are independent
//! of font availability; adding a language only requires a catalog and registry entry.

#[cfg(test)]
mod catalog;
mod platform;
mod template;

use crate::settings::LangMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lang(usize);

struct Locale {
    code: &'static str,
    native_name: &'static str,
    aliases: &'static [&'static str],
    fallback: Option<usize>,
    messages: &'static [(&'static str, &'static str)],
}

include!(concat!(env!("OUT_DIR"), "/locales.rs"));

impl Lang {
    pub const EN: Self = Self(EN_INDEX);
    pub const ZH_CN: Self = Self(ZH_CN_INDEX);

    pub fn from_code(code: &str) -> Option<Self> {
        let code = normalize_code(code)?;
        exact_language(&code, LOCALES).map(Self)
    }

    pub fn code(self) -> &'static str {
        LOCALES[self.0].code
    }

    pub fn native_name(self) -> &'static str {
        LOCALES[self.0].native_name
    }

    pub fn all() -> &'static [Self] {
        ALL_LANGS
    }

    pub fn t(self, key: &'static str) -> &'static str {
        lookup(LOCALES, self.0, EN_INDEX, key).unwrap_or(key)
    }

    pub fn format(self, key: &'static str, args: &[(&str, &str)]) -> String {
        format_template(self.t(key), args)
    }
}

pub fn resolve(mode: LangMode) -> Lang {
    match mode {
        LangMode::System => system_language(),
        LangMode::Selected(language) => language,
    }
}

pub fn system_language() -> Lang {
    static SYSTEM_LANGUAGE: std::sync::OnceLock<Lang> = std::sync::OnceLock::new();
    *SYSTEM_LANGUAGE.get_or_init(|| preferred_language(&platform::preferred_languages()))
}

fn preferred_language(preferences: &[String]) -> Lang {
    preferences
        .iter()
        .find_map(|code| match_language(code, LOCALES).map(Lang))
        .unwrap_or(Lang::EN)
}

fn normalize_code(code: &str) -> Option<String> {
    let code = code.trim().replace('_', "-").to_ascii_lowercase();
    if code.is_empty()
        || code.len() > 64
        || !code.split('-').all(|part| {
            !part.is_empty()
                && part.len() <= 8
                && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
    {
        return None;
    }
    Some(code)
}

fn exact_language(code: &str, locales: &[Locale]) -> Option<usize> {
    locales.iter().position(|locale| {
        locale.code.eq_ignore_ascii_case(code)
            || locale
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(code))
    })
}

fn match_language(code: &str, locales: &[Locale]) -> Option<usize> {
    let normalized = normalize_code(code)?;
    let mut candidate = normalized.as_str();
    loop {
        if let Some(index) = exact_language(candidate, locales) {
            return Some(index);
        }
        if let Some((parent, _)) = candidate.rsplit_once('-') {
            candidate = parent;
        } else {
            // A catalog may register only a regional variant (for example pt-BR).
            // Its language still matches when no more specific entry is available.
            return locales.iter().position(|locale| {
                locale
                    .code
                    .split('-')
                    .next()
                    .is_some_and(|base| base.eq_ignore_ascii_case(candidate))
            });
        }
    }
}

fn lookup(locales: &[Locale], language: usize, english: usize, key: &str) -> Option<&'static str> {
    let message = |locale: &Locale| {
        locale
            .messages
            .binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .map(|index| locale.messages[index].1)
    };
    let mut current = Some(language);
    // The build rejects cycles; the bound also keeps this helper safe for test catalogs.
    for _ in 0..locales.len() {
        let Some(index) = current else { break };
        let locale = locales.get(index)?;
        if let Some(text) = message(locale) {
            return Some(text);
        }
        current = locale.fallback;
    }
    message(&locales[english])
}

fn format_template(text: &str, args: &[(&str, &str)]) -> String {
    let Ok(parts) = template::parse(text) else {
        // Build-time validation guarantees valid embedded templates.
        return text.into();
    };
    let mut formatted = String::with_capacity(text.len());
    for part in parts {
        match part {
            template::Part::Text(text) => formatted.push_str(text),
            template::Part::Argument(name) => {
                if let Some((_, value)) = args.iter().find(|(key, _)| *key == name) {
                    formatted.push_str(value);
                } else {
                    formatted.push('{');
                    formatted.push_str(name);
                    formatted.push('}');
                }
            }
        }
    }
    formatted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_codes_and_legacy_aliases_are_case_insensitive() {
        assert_eq!(Lang::from_code("en"), Some(Lang::EN));
        assert_eq!(Lang::from_code(" EN_us "), Some(Lang::EN));
        assert_eq!(Lang::from_code("zh"), Some(Lang::ZH_CN));
        assert_eq!(Lang::from_code("zh_hans_CN"), Some(Lang::ZH_CN));
        assert_eq!(Lang::from_code("missing"), None);
        assert_eq!(Lang::from_code("../../en"), None);
        for lang in Lang::all() {
            assert_eq!(Lang::from_code(lang.code()), Some(*lang));
            assert!(!lang.native_name().is_empty());
        }
    }

    #[test]
    fn system_preferences_match_regions_and_preserve_priority() {
        let pick = |preferences: &[&str]| {
            preferred_language(
                &preferences
                    .iter()
                    .map(|code| (*code).to_owned())
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(pick(&["fr-FR", "en-AU", "zh-CN"]), Lang::EN);
        assert_eq!(pick(&["zh-Hans-SG", "en-US"]), Lang::ZH_CN);
        assert_eq!(pick(&["en-AU", "zh-CN"]), Lang::EN);
        assert_eq!(pick(&["unknown"]), Lang::EN);
        assert_eq!(pick(&[]), Lang::EN);
        assert_eq!(resolve(LangMode::Selected(Lang::ZH_CN)), Lang::ZH_CN);
    }

    #[test]
    fn lookup_uses_declared_fallback_then_english() {
        let locales = [
            Locale {
                code: "en",
                native_name: "English",
                aliases: &[],
                fallback: None,
                messages: &[("only.english", "English"), ("translated", "Base")],
            },
            Locale {
                code: "pt-BR",
                native_name: "Português",
                aliases: &[],
                fallback: None,
                messages: &[("translated", "Português")],
            },
            Locale {
                code: "pt-PT",
                native_name: "Português",
                aliases: &[],
                fallback: Some(1),
                messages: &[],
            },
        ];
        assert_eq!(match_language("pt-AO", &locales), Some(1));
        assert_eq!(match_language("pt-PT", &locales), Some(2));
        assert_eq!(lookup(&locales, 2, 0, "translated"), Some("Português"));
        assert_eq!(lookup(&locales, 2, 0, "only.english"), Some("English"));
        assert_eq!(lookup(&locales, 2, 0, "unknown"), None);
    }

    #[test]
    fn named_parameters_can_reorder_and_never_expand_argument_contents() {
        assert_eq!(
            format_template(
                "{amount} / {name} / {name}",
                &[("name", "应用 {amount}"), ("amount", "12 MB")]
            ),
            "12 MB / 应用 {amount} / 应用 {amount}"
        );
        assert_eq!(
            format_template("{{{name}}}", &[("name", "{literal}")]),
            "{{literal}}"
        );
        assert_eq!(format_template("{missing}", &[]), "{missing}");
        assert!(template::parse("{not valid}").is_err());
        assert!(template::parse("unclosed {").is_err());
        assert!(template::parse("closing }").is_err());
    }

    #[test]
    fn literal_message_keys_in_application_sources_have_an_english_translation() {
        fn inspect(directory: &std::path::Path, missing: &mut Vec<String>) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    // The infrastructure tests deliberately exercise missing keys.
                    if path.file_name().is_some_and(|name| name != "i18n") {
                        inspect(&path, missing);
                    }
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let source = std::fs::read_to_string(&path).unwrap();
                    for method in [".t(", ".format("] {
                        for rest in source.split(method).skip(1) {
                            let Some(quoted) = rest.trim_start().strip_prefix('"') else {
                                continue;
                            };
                            let Some((key, _)) = quoted.split_once('"') else {
                                continue;
                            };
                            if lookup(LOCALES, EN_INDEX, EN_INDEX, key).is_none() {
                                missing.push(format!("{}: {key}", path.display()));
                            }
                        }
                    }
                }
            }
        }
        let mut missing = Vec::new();
        inspect(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut missing,
        );
        assert!(
            missing.is_empty(),
            "Missing English message keys:\n{}",
            missing.join("\n")
        );
    }
}
