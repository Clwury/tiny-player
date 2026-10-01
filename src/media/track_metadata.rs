pub(crate) fn track_metadata_label(language: Option<&str>, title: Option<&str>) -> String {
    let language = chinese_language_name(language);
    match title.map(str::trim).filter(|title| !title.is_empty()) {
        Some(title) => format!("{language} [{title}]"),
        None => language.to_string(),
    }
}

fn chinese_language_name(language: Option<&str>) -> &'static str {
    let code = language
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-");
    let base = code.split('-').next().unwrap_or_default();
    if matches!(base, "zh" | "zho" | "chi") {
        let subtags = code.split('-').skip(1).collect::<Vec<_>>();
        // Explicit script tags take precedence over a region, as in zh-Hant-CN.
        if subtags.contains(&"hans") {
            return "简体中文";
        }
        if subtags.contains(&"hant") {
            return "繁体中文";
        }
        if subtags.iter().any(|tag| matches!(*tag, "cn" | "sg")) {
            return "简体中文";
        }
        if subtags.iter().any(|tag| matches!(*tag, "tw" | "hk" | "mo")) {
            return "繁体中文";
        }
        return "中文";
    }

    // Emby language tags use ISO 639-1 or ISO 639-2, including bibliographic aliases.
    match base {
        "chs" => "简体中文",
        "cht" => "繁体中文",
        "cmn" => "普通话",
        "yue" => "粤语",
        "en" | "eng" => "英语",
        "ja" | "jpn" => "日语",
        "ko" | "kor" => "韩语",
        "fr" | "fra" | "fre" => "法语",
        "de" | "deu" | "ger" => "德语",
        "es" | "spa" => "西班牙语",
        "it" | "ita" => "意大利语",
        "pt" | "por" => "葡萄牙语",
        "ru" | "rus" => "俄语",
        "ar" | "ara" => "阿拉伯语",
        "hi" | "hin" => "印地语",
        "bn" | "ben" => "孟加拉语",
        "th" | "tha" => "泰语",
        "vi" | "vie" => "越南语",
        "id" | "ind" => "印度尼西亚语",
        "ms" | "msa" | "may" => "马来语",
        "nl" | "nld" | "dut" => "荷兰语",
        "no" | "nor" => "挪威语",
        "nb" | "nob" => "书面挪威语",
        "nn" | "nno" => "新挪威语",
        "sv" | "swe" => "瑞典语",
        "da" | "dan" => "丹麦语",
        "fi" | "fin" => "芬兰语",
        "is" | "isl" | "ice" => "冰岛语",
        "pl" | "pol" => "波兰语",
        "cs" | "ces" | "cze" => "捷克语",
        "sk" | "slk" | "slo" => "斯洛伐克语",
        "hu" | "hun" => "匈牙利语",
        "ro" | "ron" | "rum" => "罗马尼亚语",
        "bg" | "bul" => "保加利亚语",
        "uk" | "ukr" => "乌克兰语",
        "el" | "ell" | "gre" => "希腊语",
        "tr" | "tur" => "土耳其语",
        "he" | "heb" => "希伯来语",
        "fa" | "fas" | "per" => "波斯语",
        "ta" | "tam" => "泰米尔语",
        "te" | "tel" => "泰卢固语",
        "tl" | "tgl" => "他加禄语",
        "fil" => "菲律宾语",
        "mul" => "多语言",
        "zxx" => "无语言",
        _ => "未知语言",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_metadata_formats_chinese_language_and_original_title() {
        for (language, expected) in [
            ("eng", "英语 [English SDH]"),
            ("EN_us", "英语 [English SDH]"),
            ("chi", "中文 [English SDH]"),
            ("jpn", "日语 [English SDH]"),
            ("kor", "韩语 [English SDH]"),
            ("fre", "法语 [English SDH]"),
            ("fra", "法语 [English SDH]"),
            ("ger", "德语 [English SDH]"),
            ("deu", "德语 [English SDH]"),
        ] {
            let stream: crate::emby::MediaStream = serde_json::from_value(serde_json::json!({
                "Language": language, "Title": " English SDH ",
                "DisplayTitle": "Chinese Simplified (ASS)"
            }))
            .unwrap();
            assert_eq!(
                track_metadata_label(stream.language.as_deref(), stream.title.as_deref()),
                expected
            );
        }
    }

    #[test]
    fn track_language_distinguishes_chinese_scripts_and_regions() {
        for (code, expected) in [
            ("zho", "中文"),
            ("zh", "中文"),
            ("chs", "简体中文"),
            ("CHT", "繁体中文"),
            ("zh_CN", "简体中文"),
            ("zh-HK", "繁体中文"),
            ("zh-Hans-TW", "简体中文"),
            (" zh-Hant-CN ", "繁体中文"),
        ] {
            assert_eq!(chinese_language_name(Some(code)), expected);
        }
    }

    #[test]
    fn track_metadata_handles_missing_titles_and_unknown_languages() {
        for (json, expected) in [
            (serde_json::json!({"Language": "eng"}), "英语"),
            (
                serde_json::json!({"Language": "jpn", "Title": " \t "}),
                "日语",
            ),
            (
                serde_json::json!({"Language": "und", "Title": "字幕组"}),
                "未知语言 [字幕组]",
            ),
            (
                serde_json::json!({"Title": "Subtitle"}),
                "未知语言 [Subtitle]",
            ),
            (serde_json::json!({"Language": "xyz"}), "未知语言"),
            (serde_json::json!({}), "未知语言"),
        ] {
            let stream: crate::emby::MediaStream = serde_json::from_value(json).unwrap();
            assert_eq!(
                track_metadata_label(stream.language.as_deref(), stream.title.as_deref()),
                expected
            );
        }
    }
}
