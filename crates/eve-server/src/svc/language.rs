//! Language selection metadata. Clients load translations from their own resources.

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::rowset;

// Languages shipped with the original 45017 client, plus implicit English.
const LANGUAGES: [(&str, &str, &str); 9] = [
    ("EN", "English", "English"),
    ("DE", "German", "Deutsch"),
    ("ES", "Spanish", "Español"),
    ("FR", "French", "Français"),
    ("IT", "Italian", "Italiano"),
    ("JA", "Japanese", "日本語"),
    ("KO", "Korean", "한국어"),
    ("PT", "Portuguese", "Português"),
    ("RU", "Russian", "Русский"),
];

/// A supported client language ID, normalized to its canonical form.
pub fn canonical_language(language: &str) -> Option<&'static str> {
    LANGUAGES.iter().map(|(id, _, _)| *id).find(|id| language.eq_ignore_ascii_case(id))
}

/// The client falls back to its implicit English text for unsupported languages.
pub fn resolve_language(language: &str) -> &'static str {
    canonical_language(language).unwrap_or("EN")
}

fn languages() -> Value {
    let lines = LANGUAGES.iter().map(|(id, name, translated)| Value::List(vec![
        Value::bytes(*id), Value::text(*name), Value::text(*translated),
    ])).collect();
    // gameui.GetLanguages reads row.languageID (not row[0]).
    rowset(&["languageID", "languageName", "translatedLanguageName"], lines)
}

pub async fn language_svc(call: &Call, _ctx: &Ctx<'_>) -> Option<Outcome> {
    reply(call)
}

fn reply(call: &Call) -> Option<Outcome> {
    Some(match call.method.as_str() {
        "GetLanguages" if call.args.is_empty() && call.kwargs.is_empty() => Outcome::Value(languages()),
        "GetLanguages" => Outcome::custom("This language method takes no arguments."),
        "DbgGetLanguagePack" => Outcome::custom("Debug language downloads are not supported. Translations are loaded from the client's installed resources."),
        // These legacy stubs have no calls in the 45017 source/disassembly.
        "GetMessagesByLanguageID" | "GetTextsForGroup" => Outcome::custom(
            "This language operation is unavailable in this client version."),
        _ => return None,
    })
}
