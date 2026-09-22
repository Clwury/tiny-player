use std::{collections::HashSet, sync::OnceLock};

use serde::Deserialize;

#[derive(Deserialize)]
struct Catalog {
    icons: Vec<Icon>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct Icon {
    pub(crate) name: String,
    pub(crate) url: String,
    #[serde(skip)]
    key: String,
}

pub(crate) fn all_icons() -> &'static [Icon] {
    static ICONS: OnceLock<Vec<Icon>> = OnceLock::new();
    ICONS.get_or_init(|| {
        serde_json::from_str::<Catalog>(include_str!("../../assets/lige-emby-icon.json"))
            .expect("bundled Emby icon catalog must be valid")
            .icons
    })
}

fn catalog() -> &'static [Icon] {
    static MATCHING_ICONS: OnceLock<Vec<Icon>> = OnceLock::new();
    MATCHING_ICONS.get_or_init(|| {
        let mut icons = all_icons().to_vec();
        // For the same normalized name, prefer Emby artwork first, then the
        // original icon over numbered variants.
        icons.sort_by_key(|icon| {
            (
                !icon.name.to_ascii_lowercase().contains("emby"),
                icon.name.contains('('),
                icon.name.len(),
            )
        });
        let mut names = HashSet::new();
        icons.retain_mut(|icon| {
            icon.key = name_tokens(&icon.name).concat();
            !icon.key.is_empty() && names.insert(icon.key.clone())
        });
        icons
    })
}

pub(crate) fn match_icon_url(server_name: &str) -> Option<&'static str> {
    best_match(server_name, catalog()).map(|icon| icon.url.as_str())
}

fn best_match<'a>(server_name: &str, icons: &'a [Icon]) -> Option<&'a Icon> {
    let tokens = name_tokens(server_name);
    let key = tokens.concat();
    if key.is_empty() {
        return None;
    }

    let mut best = None;
    let mut best_score = 0.0;
    let mut runner_up: f64 = 0.0;
    for icon in icons {
        let score = match_score(&key, &tokens, &icon.key);
        if score > best_score {
            runner_up = best_score;
            best_score = score;
            best = Some(icon);
        } else {
            runner_up = runner_up.max(score);
        }
    }
    // Similar names should not silently pick an unrelated server's artwork.
    best.filter(|_| best_score == 1.0 || best_score - runner_up >= 0.03)
}

fn match_score(server: &str, tokens: &[String], icon: &str) -> f64 {
    if server == icon {
        return 1.0;
    }
    let icon_len = icon.chars().count();
    let server_len = server.chars().count();
    if icon_len >= 3 && tokens.iter().any(|token| token == icon) {
        return 0.97;
    }
    let coverage = icon_len as f64 / server_len as f64;
    if icon_len >= 4 && coverage >= 0.6 && server.contains(icon) {
        return 0.92 + coverage * 0.04;
    }
    if icon_len >= 5 && server_len >= 5 {
        let similarity = strsim::normalized_damerau_levenshtein(server, icon);
        if similarity >= 0.85 {
            return similarity * 0.95;
        }
    }
    0.0
}

fn name_tokens(name: &str) -> Vec<String> {
    let lowercase = name.to_lowercase();
    let mut remainder = lowercase.as_str();
    let mut name = String::new();
    while let Some(start) = remainder.find('(') {
        name.push_str(&remainder[..start]);
        let suffix = &remainder[start + 1..];
        let Some(end) = suffix.find(')') else {
            remainder = suffix;
            break;
        };
        if suffix[..end].is_empty() || !suffix[..end].chars().all(|ch| ch.is_ascii_digit()) {
            name.push(' ');
            name.push_str(&suffix[..end]);
            name.push(' ');
        }
        remainder = &suffix[end + 1..];
    }
    name.push_str(remainder);
    name.split(|ch: char| !ch.is_alphanumeric())
        // Treat attached and separated suffixes alike: OkEmby and Ok-emby.
        .map(|token| token.strip_suffix("emby").unwrap_or(token))
        .filter(|token| {
            !token.is_empty()
                && !matches!(
                    *token,
                    "emby" | "server" | "media" | "服务器" | "影视" | "影音" | "媒体库"
                )
        })
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_catalog_preserves_every_icon_and_variant_in_source_order() {
        let source: Catalog =
            serde_json::from_str(include_str!("../../assets/lige-emby-icon.json")).unwrap();
        assert_eq!(all_icons().len(), source.icons.len());
        for (actual, expected) in all_icons().iter().zip(&source.icons) {
            assert_eq!(actual.name, expected.name);
            assert_eq!(actual.url, expected.url);
        }
        assert_eq!(all_icons()[0].name, "emby");
        assert!(
            all_icons()
                .iter()
                .any(|icon| icon.name == "chinamobilemcloud(1)")
        );
        assert!(all_icons().len() > catalog().len());
    }

    fn icon_file(name: &str) -> Option<&str> {
        match_icon_url(name).and_then(|url| url.rsplit('/').next())
    }

    #[test]
    fn matches_names_ignoring_case_spacing_and_emby_suffix() {
        for name in ["AlphaTV", "alpha tv", "🎬 ALPHA-TV Emby", "AlphaTV(2)-emby"] {
            assert_eq!(icon_file(name), Some("AlphaTV-emby.png"), "{name}");
        }
    }

    #[test]
    fn matches_emby_suffixes_without_a_separator() {
        for name in [
            "OkEmby",
            "okemby",
            "OKEMBY",
            "Ok-emby",
            "Ok Emby",
            "OkEmby(2)",
        ] {
            assert_eq!(icon_file(name), Some("Ok-emby.png"), "{name}");
        }
        assert_eq!(icon_file("AlphaTVEmby"), Some("AlphaTV-emby.png"));
        assert_eq!(icon_file("UHDEmby"), Some("UHD-emby.png"));
        assert_eq!(icon_file("onemby"), Some("onemby.png"));
    }

    #[test]
    fn prefers_emby_artwork_when_multiple_icons_match_the_same_name() {
        for name in ["UHD", "uhd", "UHD-emby", "UHD Emby | 主线路"] {
            assert_eq!(icon_file(name), Some("UHD-emby.png"), "{name}");
        }
        assert_eq!(icon_file("moli"), Some("moli-emby.png"));
        assert_eq!(icon_file("now"), Some("now.png"));
    }

    #[test]
    fn matches_a_small_spelling_error_in_a_distinctive_name() {
        assert_eq!(icon_file("Termni us"), Some("Terminus-emby.png"));
    }

    #[test]
    fn unknown_generic_and_short_partial_names_use_the_default_icon() {
        for name in [
            "",
            "Emby",
            "Emby Server",
            "我的私人影院",
            "xx",
            "family videos",
            "OkEmbyPlus",
            "MyOkEmby",
        ] {
            assert_eq!(match_icon_url(name), None, "{name}");
        }
    }

    #[test]
    fn ambiguous_similar_names_use_the_default_icon() {
        let icons = ["starcats", "starcuts"].map(|key| Icon {
            name: key.into(),
            url: format!("https://example.com/{key}.png"),
            key: key.into(),
        });
        assert!(best_match("starcots", &icons).is_none());
        assert_eq!(best_match("starcats", &icons).unwrap().name, "starcats");
    }

    #[test]
    fn bundled_catalog_has_unique_matching_keys_and_public_https_urls() {
        let icons = catalog();
        assert!(icons.len() > 100);
        let mut keys = HashSet::new();
        for icon in icons {
            assert!(keys.insert(&icon.key));
            let url = url::Url::parse(&icon.url).unwrap();
            assert_eq!(url.scheme(), "https");
            assert!(url.username().is_empty());
            assert!(url.password().is_none());
        }
    }
}
