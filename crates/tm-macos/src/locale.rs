//! The user's ordered language preferences, independent of fonts and region formats.

use objc2_foundation::NSLocale;

pub fn preferred_languages() -> Vec<String> {
    NSLocale::preferredLanguages()
        .iter()
        .map(|language| language.to_string())
        .collect()
}
