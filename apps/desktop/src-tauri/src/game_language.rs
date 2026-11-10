// Standalone game language initialization proposal.
// Assumes caller holds the instance preparation lease and has validated `root`.
// No external dependencies.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

const DEFAULT_LANGUAGE: &str = "en_us";

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
    fn LCIDToLocaleName(locale: u32, name: *mut u16, count: i32, flags: u32) -> i32;
}

/// Public(crate) entry point: initialize options.txt only if entirely absent.
pub(crate) fn initialize(root: &Path, version: &str) -> Result<(), String> {
    let locale = user_default_locale_name();
    initialize_for_locale(root, version, &locale)
}

/// Testable variant that takes an explicit locale string.
pub(crate) fn initialize_for_locale(
    root: &Path,
    version: &str,
    locale: &str,
) -> Result<(), String> {
    // Versions before language selection was introduced have no language option.
    if version == "1.0"
        || version.starts_with("a1.")
        || version.starts_with("b1.")
        || version.starts_with("c0.")
        || version.starts_with("rd-")
    {
        return Ok(());
    }
    let legacy = is_legacy_version(version);
    let language = choose_language(locale, legacy);
    write_options_if_absent(root, &language)
}

/// Pure mapping from a user locale (BCP47-ish) to a Minecraft language ID.
pub(crate) fn choose_language(locale: &str, legacy: bool) -> String {
    let mapped = map_locale(locale);
    if legacy {
        to_legacy_id(&mapped)
    } else {
        mapped
    }
}

fn map_locale(locale: &str) -> String {
    let trimmed = locale.trim();
    if trimmed.is_empty() {
        return DEFAULT_LANGUAGE.to_string();
    }

    let normalized = trimmed.replace('_', "-").to_ascii_lowercase();
    let mut parts = normalized.split('-').filter(|s| !s.is_empty());
    let language = match parts.next() {
        Some(l) => l,
        None => return DEFAULT_LANGUAGE.to_string(),
    };

    let subtags: Vec<&str> = parts.collect();
    let region = subtags
        .iter()
        .copied()
        .find(|s| s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic()));
    let script = subtags
        .iter()
        .copied()
        .find(|s| s.len() == 4 && s.chars().all(|c| c.is_ascii_alphabetic()));

    match language {
        "en" => match region {
            Some("gb") => "en_gb",
            _ => "en_us",
        }
        .to_string(),
        "zh" => match (script, region) {
            (Some("hans"), _) => "zh_cn",
            (Some("hant"), Some("hk")) => "zh_hk",
            (Some("hant"), _) => "zh_tw",
            (_, Some("tw")) => "zh_tw",
            (_, Some("hk")) => "zh_hk",
            (_, Some("cn")) => "zh_cn",
            _ => match script {
                Some("hant") => "zh_tw",
                _ => "zh_cn",
            },
        }
        .to_string(),
        "ja" => "ja_jp".to_string(),
        "ko" => "ko_kr".to_string(),
        "de" => "de_de".to_string(),
        "fr" => "fr_fr".to_string(),
        "es" => "es_es".to_string(),
        "pt" => match region {
            Some("pt") => "pt_pt",
            _ => "pt_br",
        }
        .to_string(),
        "ru" => "ru_ru".to_string(),
        "it" => "it_it".to_string(),
        "nl" => "nl_nl".to_string(),
        "tr" => "tr_tr".to_string(),
        "pl" => "pl_pl".to_string(),
        "uk" => "uk_ua".to_string(),
        _ => DEFAULT_LANGUAGE.to_string(),
    }
}

/// Convert modern language ID (en_us) to legacy form (en_US) for < 1.11.
fn to_legacy_id(modern: &str) -> String {
    match modern {
        "en_us" => "en_US".to_string(),
        "en_gb" => "en_GB".to_string(),
        "zh_cn" => "zh_CN".to_string(),
        "zh_tw" => "zh_TW".to_string(),
        "zh_hk" => "zh_HK".to_string(),
        "ja_jp" => "ja_JP".to_string(),
        "ko_kr" => "ko_KR".to_string(),
        "de_de" => "de_DE".to_string(),
        "fr_fr" => "fr_FR".to_string(),
        "es_es" => "es_ES".to_string(),
        "pt_br" => "pt_BR".to_string(),
        "pt_pt" => "pt_PT".to_string(),
        "ru_ru" => "ru_RU".to_string(),
        "it_it" => "it_IT".to_string(),
        "nl_nl" => "nl_NL".to_string(),
        "tr_tr" => "tr_TR".to_string(),
        "pl_pl" => "pl_PL".to_string(),
        "uk_ua" => "uk_UA".to_string(),
        other => other.to_string(),
    }
}

/// Determine whether the given version predates 1.11.
/// Releases <= 1.10.x are legacy; snapshots/unknown default to modern.
fn is_legacy_version(version: &str) -> bool {
    let v = version.trim();
    if v.is_empty() {
        return false;
    }
    // Language filenames became lowercase in snapshot 16w32a.
    if let Some((year, week)) = v.split_once('w') {
        if let (Ok(year), Some(week)) = (
            year.parse::<u32>(),
            week.get(..2).and_then(|w| w.parse::<u32>().ok()),
        ) {
            return (year, week) < (16, 32);
        }
    }
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() < 2
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    let major: u32 = match parts[0].parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    if major != 1 {
        return false;
    }
    let minor: u32 = match parts[1].parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    if minor >= 11 {
        return false;
    }
    // 1.0 through 1.10 inclusive are legacy.
    true
}

fn write_options_if_absent(root: &Path, language: &str) -> Result<(), String> {
    let options_path = root.join("options.txt");

    // Reject an existing symlink/reparse point at the target path.
    if let Err(_) = crate::game_install::reject_link(&options_path) {
        return Err("options.txt is a link".to_string());
    }

    match fs::symlink_metadata(&options_path) {
        Ok(metadata) if metadata.is_file() => return Ok(()),
        Ok(_) => return Err("game_language_options_not_file".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("game_language_options_metadata".into()),
    }

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&options_path)
        .map_err(|e| format!("failed to create options.txt: {e}"))?;

    let content = format!("lang:{language}\n");
    file.write_all(content.as_bytes())
        .map_err(|e| format!("failed to write options.txt: {e}"))?;
    file.flush()
        .map_err(|e| format!("failed to flush options.txt: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("failed to sync options.txt: {e}"))?;
    Ok(())
}

fn user_default_locale_name() -> String {
    #[cfg(windows)]
    unsafe {
        // LOCALE_NAME_MAX_LENGTH is 85; use 128 for safety.
        let mut buf = [0u16; 128];
        // Display language, rather than the independently chosen regional format.
        let len = LCIDToLocaleName(
            u32::from(GetUserDefaultUILanguage()),
            buf.as_mut_ptr(),
            buf.len() as i32,
            0,
        );
        if len > 0 {
            let n = (len as usize - 1).min(buf.len());
            if let Ok(s) = String::from_utf16(&buf[..n]) {
                return s;
            }
        }
    }
    "en-US".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("gl_test_{tag}_{pid}_{nanos}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn maps_common_locales() {
        assert_eq!(choose_language("en-US", false), "en_us");
        assert_eq!(choose_language("en-GB", false), "en_gb");
        assert_eq!(choose_language("zh-CN", false), "zh_cn");
        assert_eq!(choose_language("zh-TW", false), "zh_tw");
        assert_eq!(choose_language("zh-HK", false), "zh_hk");
        assert_eq!(choose_language("zh-Hant", false), "zh_tw");
        assert_eq!(choose_language("zh-Hant-HK", false), "zh_hk");
        assert_eq!(choose_language("zh-Hans-TW", false), "zh_cn");
        assert_eq!(choose_language("zh-Hant-CN", false), "zh_tw");
        assert_eq!(choose_language("ja-JP", false), "ja_jp");
        assert_eq!(choose_language("ko-KR", false), "ko_kr");
        assert_eq!(choose_language("de-DE", false), "de_de");
        assert_eq!(choose_language("fr-FR", false), "fr_fr");
        assert_eq!(choose_language("es-ES", false), "es_es");
        assert_eq!(choose_language("pt-BR", false), "pt_br");
        assert_eq!(choose_language("pt-PT", false), "pt_pt");
        assert_eq!(choose_language("ru-RU", false), "ru_ru");
        assert_eq!(choose_language("it-IT", false), "it_it");
        assert_eq!(choose_language("nl-NL", false), "nl_nl");
        assert_eq!(choose_language("tr-TR", false), "tr_tr");
        assert_eq!(choose_language("pl-PL", false), "pl_pl");
        assert_eq!(choose_language("uk-UA", false), "uk_ua");
        assert_eq!(choose_language("xx-YY", false), "en_us");
        assert_eq!(choose_language("", false), "en_us");
    }

    #[test]
    fn legacy_ids_for_old_versions() {
        assert_eq!(choose_language("en-US", true), "en_US");
        assert_eq!(choose_language("zh-CN", true), "zh_CN");
        assert_eq!(choose_language("zh-TW", true), "zh_TW");
        assert!(is_legacy_version("1.10.2"));
        assert!(is_legacy_version("1.8.9"));
        assert!(!is_legacy_version("1.11"));
        assert!(!is_legacy_version("1.20.1"));
        assert!(!is_legacy_version("1.14-Pre1"));
        assert!(!is_legacy_version("snapshot"));
        assert!(!is_legacy_version("unknown"));
        assert!(is_legacy_version("16w31a"));
        assert!(!is_legacy_version("16w32a"));
    }

    #[test]
    fn existing_options_preserved() {
        let dir = unique_dir("preserve");
        let path = dir.join("options.txt");
        let original = b"lang:fr_fr\nmalformed".to_vec();
        fs::write(&path, &original).unwrap();
        initialize_for_locale(&dir, "1.20.1", "de-DE").unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_options_preserved() {
        let dir = unique_dir("empty");
        let path = dir.join("options.txt");
        fs::write(&path, b"").unwrap();
        initialize_for_locale(&dir, "1.20.1", "de-DE").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_default_written() {
        let dir = unique_dir("fresh");
        initialize_for_locale(&dir, "1.20.1", "de-DE").unwrap();
        let content = fs::read_to_string(dir.join("options.txt")).unwrap();
        assert_eq!(content, "lang:de_de\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_and_pre_language_versions() {
        let dir = unique_dir("legacy");
        initialize_for_locale(&dir, "1.7.10", "zh-CN").unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("options.txt")).unwrap(),
            "lang:zh_CN\n"
        );
        fs::remove_file(dir.join("options.txt")).unwrap();
        initialize_for_locale(&dir, "b1.7.3", "zh-CN").unwrap();
        assert!(!dir.join("options.txt").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_existing_directory() {
        let dir = unique_dir("dir");
        let path = dir.join("options.txt");
        fs::create_dir(&path).unwrap();
        assert!(initialize_for_locale(&dir, "1.20.1", "de-DE").is_err());
        assert!(path.is_dir());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_options_rejected() {
        use std::os::unix::fs::symlink;
        let dir = unique_dir("link");
        let target = dir.join("target.txt");
        fs::write(&target, b"keep").unwrap();
        let link = dir.join("options.txt");
        if symlink(&target, &link).is_ok() {
            let result = initialize_for_locale(&dir, "1.20.1", "de-DE");
            assert!(result.is_err());
            assert_eq!(fs::read(&target).unwrap(), b"keep");
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
