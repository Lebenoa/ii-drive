use axum::Json;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde_json::json;

/// Locale codes are file names: keep them to `[a-zA-Z0-9_-]+` so the path
/// below can never escape `locales_dir`, whatever the OS makes of dots or
/// separators. Also caps length against absurd requests.
fn valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 16
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// File stems in `locales_dir` that are metadata rather than a language:
/// `manifest.json` is the repo-side catalog of available languages.
fn is_reserved_stem(stem: &str) -> bool {
    stem == "manifest"
}

/// GET /locales/manifest.json — languages available for the web UI to
/// download: every `{code}.json` in `locales_dir`. The display name comes
/// from the file's `_meta.name` (written by whoever ships the translation);
/// without it the code itself is shown. Unauthenticated: the login page
/// needs translations too.
pub async fn manifest() -> impl IntoResponse {
    let dir = std::path::Path::new(&crate::config::get().locales_dir).to_owned();
    let mut languages = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let is_json = path.extension().is_some_and(|e| e == "json");
            if !is_json {
                continue;
            }
            let Some(code) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if is_reserved_stem(code) || !valid_code(code) {
                continue;
            }
            let name = read_name(&path).unwrap_or_else(|| code.to_string());
            languages.push(json!({ "code": code, "name": name }));
        }
    }
    languages.sort_by(|a, b| a["code"].as_str().cmp(&b["code"].as_str()));
    Json(json!({ "languages": languages }))
}

/// Reads `_meta.name` out of a locale file; None when absent or unparsable.
fn read_name(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("_meta")?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

/// GET /locales/{file}.json — one translation dictionary (e.g. `th.json`),
/// downloaded by the web UI only after the user picks that language. The
/// extension is part of the matched segment (axum allows one plain
/// parameter per segment), so it is split off and verified here. `no-cache`
/// keeps edits and newly dropped files visible without a hard refresh.
pub async fn locale(Path(file): Path<String>) -> Result<impl IntoResponse, StatusCode> {
    let Some((lang, ext)) = file.rsplit_once('.') else {
        return Err(StatusCode::NOT_FOUND);
    };
    if ext != "json" || !valid_code(lang) {
        return Err(StatusCode::NOT_FOUND);
    }
    let path = std::path::Path::new(&crate::config::get().locales_dir).join(format!("{lang}.json"));
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        text,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_tight() {
        assert!(valid_code("en"));
        assert!(valid_code("zh-Hans-TW"));
        assert!(valid_code("pt_BR"));
        // Traversal and junk never pass.
        assert!(!valid_code("../secret"));
        assert!(!valid_code("en.json"));
        assert!(!valid_code(""));
        assert!(!valid_code("a%b"));
        assert!(!valid_code("0123456789abcdefg"));
    }

    #[test]
    fn manifest_stem_is_reserved() {
        // The catalog file sits in the same folder as the dictionaries; the
        // scan must not offer it as a language.
        assert!(is_reserved_stem("manifest"));
        assert!(!is_reserved_stem("th"));
        assert!(!is_reserved_stem("en"));
    }
}
