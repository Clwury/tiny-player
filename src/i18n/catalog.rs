use std::{borrow::Cow, collections::HashMap, sync::OnceLock};

use super::AppLanguage;

struct Catalog {
    exact: HashMap<&'static str, &'static str>,
    templates: Vec<(Template, Template)>,
}

#[derive(Debug)]
enum Segment {
    Text(&'static str),
    Field(String),
}

struct Template {
    segments: Vec<Segment>,
    literal_length: usize,
    localize_fields: bool,
    generic_error_wrapper: bool,
}

impl Template {
    fn new(value: &'static str) -> Self {
        let mut segments = Vec::new();
        let mut remaining = value;
        let mut position = 0;
        let mut literal_length = 0;
        while let Some(start) = remaining.find('{') {
            let literal = &remaining[..start];
            literal_length += literal.len();
            segments.push(Segment::Text(literal));
            remaining = &remaining[start + 1..];
            let end = remaining.find('}').expect("translation field must close");
            let field = remaining[..end].split(':').next().unwrap();
            segments.push(Segment::Field(if field.is_empty() {
                let field = position.to_string();
                position += 1;
                field
            } else {
                field.to_owned()
            }));
            remaining = &remaining[end + 1..];
        }
        literal_length += remaining.len();
        segments.push(Segment::Text(remaining));
        Self {
            segments,
            literal_length,
            localize_fields: value.starts_with("{}失败")
                || value == "{label}不能为空"
                || value.starts_with("加载收藏{}")
                || value.starts_with("缓存内存 ≈")
                || value.starts_with("观看状态已更新，刷新失败"),
            generic_error_wrapper: value.starts_with("{}失败"),
        }
    }

    fn capture<'a>(&self, value: &'a str) -> Option<Vec<(&str, &'a str)>> {
        let mut remaining = value;
        let mut fields = Vec::new();
        for (index, segment) in self.segments.iter().enumerate() {
            match segment {
                Segment::Text(literal) => remaining = remaining.strip_prefix(literal)?,
                Segment::Field(field) => {
                    let next_literal = match &self.segments[index + 1] {
                        Segment::Text(literal) => *literal,
                        Segment::Field(_) => unreachable!(),
                    };
                    let end = if next_literal.is_empty() {
                        remaining.len()
                    } else {
                        remaining.find(next_literal)?
                    };
                    fields.push((field.as_str(), &remaining[..end]));
                    remaining = &remaining[end..];
                }
            }
        }
        remaining.is_empty().then_some(fields)
    }

    fn render(
        &self,
        fields: &[(&str, &str)],
        language: AppLanguage,
        depth: usize,
        localize_fields: bool,
    ) -> String {
        let mut result = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Text(literal) => result.push_str(literal),
                Segment::Field(key) => {
                    let (_, value) = fields
                        .iter()
                        .find(|(field, _)| *field == key)
                        .expect("translation must preserve source fields");
                    // Labels supplied by Emby, paths, IDs and timestamps are
                    // content. Application error/action text and known labels
                    // are recursive.
                    if localize_fields || matches!(key.as_str(), "error" | "message" | "action") {
                        result.push_str(&translate_inner(language, value, depth + 1));
                    } else {
                        result.push_str(value);
                    }
                }
            }
        }
        result
    }
}

impl Catalog {
    fn new(json: &'static str) -> Self {
        let exact: HashMap<&'static str, &'static str> =
            serde_json::from_str(json).expect("bundled translations must be valid JSON");
        let mut templates: Vec<_> = exact
            .iter()
            .filter(|(source, _)| source.contains('{'))
            .map(|(source, translation)| (Template::new(source), Template::new(translation)))
            .collect();
        // Prefer specific messages to generic error wrappers.
        templates.sort_by_key(|(source, _)| std::cmp::Reverse(source.literal_length));
        Self { exact, templates }
    }
}

fn catalog(language: AppLanguage) -> &'static Catalog {
    static ENGLISH: OnceLock<Catalog> = OnceLock::new();
    static TRADITIONAL: OnceLock<Catalog> = OnceLock::new();
    static SIMPLIFIED: OnceLock<Catalog> = OnceLock::new();
    match language {
        AppLanguage::English => ENGLISH.get_or_init(|| Catalog::new(include_str!("en.json"))),
        AppLanguage::ChineseTraditional => {
            TRADITIONAL.get_or_init(|| Catalog::new(include_str!("zh-Hant.json")))
        }
        AppLanguage::ChineseSimplified => {
            SIMPLIFIED.get_or_init(|| Catalog::new(include_str!("zh-Hans.json")))
        }
    }
}

pub(crate) fn translate(language: AppLanguage, value: &str) -> Cow<'_, str> {
    translate_inner(language, value, 0)
}

fn translate_inner(language: AppLanguage, value: &str, depth: usize) -> Cow<'_, str> {
    if depth > 4 {
        return Cow::Borrowed(value);
    }
    let catalog = catalog(language);
    if let Some(translation) = catalog.exact.get(value) {
        return Cow::Owned((*translation).to_owned());
    }
    // Known error prefixes take precedence over a generic "{} failed" wrapper.
    for separator in ["：", ": "] {
        if let Some((prefix, suffix)) = value.split_once(separator)
            && let Some(translated_prefix) = catalog.exact.get(prefix)
        {
            let translated_suffix = translate_inner(language, suffix, depth + 1);
            let separator = if language == AppLanguage::English {
                ": "
            } else {
                separator
            };
            return Cow::Owned(format!("{translated_prefix}{separator}{translated_suffix}"));
        }
    }
    for (source, target) in catalog
        .templates
        .iter()
        .filter(|(source, _)| !source.generic_error_wrapper)
    {
        if let Some(fields) = source.capture(value) {
            return Cow::Owned(target.render(&fields, language, depth, source.localize_fields));
        }
    }
    // Combined errors must be separated before the generic failure wrapper can
    // mistake an earlier message for an action. Specific templates above keep
    // paths and other content fields intact.
    if let Some((prefix, suffix)) = value.split_once('；') {
        let translated_prefix = translate_inner(language, prefix, depth + 1);
        let translated_suffix = translate_inner(language, suffix, depth + 1);
        if translated_prefix != prefix || translated_suffix != suffix {
            let separator = if language == AppLanguage::English {
                "; "
            } else {
                "；"
            };
            return Cow::Owned(format!("{translated_prefix}{separator}{translated_suffix}"));
        }
    }
    for (source, target) in catalog
        .templates
        .iter()
        .filter(|(source, _)| source.generic_error_wrapper)
    {
        if let Some(fields) = source.capture(value) {
            return Cow::Owned(target.render(&fields, language, depth, source.localize_fields));
        }
    }
    for separator in ["：", ": ", " · ", " / "] {
        if let Some((prefix, suffix)) = value.split_once(separator) {
            let translated_prefix = translate_inner(language, prefix, depth + 1);
            let translated_suffix = translate_inner(language, suffix, depth + 1);
            if translated_prefix != prefix || translated_suffix != suffix {
                let separator = if language == AppLanguage::English && separator == "：" {
                    ": "
                } else {
                    separator
                };
                return Cow::Owned(format!("{translated_prefix}{separator}{translated_suffix}"));
            }
        }
    }
    Cow::Borrowed(value)
}

#[cfg(test)]
mod tests;
