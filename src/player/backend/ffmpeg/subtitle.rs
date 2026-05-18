use super::*;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct DecodedSubtitleCue {
    pub(super) text: String,
    pub(super) bitmaps: Vec<BackendSubtitleBitmap>,
    pub(super) start_offset_nsecs: u64,
    pub(super) end_offset_nsecs: u64,
    pub(super) pts_nsecs: Option<u64>,
}

impl DecodedSubtitleCue {
    pub(super) fn has_content(&self) -> bool {
        !self.text.trim().is_empty() || !self.bitmaps.is_empty()
    }
}

const EXTERNAL_SUBTITLE_MAX_BYTES: u64 = 8 * 1024 * 1024;
const EXTERNAL_SUBTITLE_TIMEOUT: Duration = Duration::from_secs(12);
const SUBTITLE_BITMAP_MAX_PIXELS: usize = 16 * 1024 * 1024;
const FALLBACK_SUBTITLE_DURATION_NSECS: u64 = 4_000_000_000;

pub(super) fn load_external_subtitle_cues(
    url: &str,
    http_headers: &[(String, String)],
    codec: Option<&str>,
) -> std::result::Result<VecDeque<BackendSubtitleCue>, String> {
    let headers = reqwest_header_pairs(http_headers)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(EXTERNAL_SUBTITLE_TIMEOUT)
        .build()
        .map_err(|error| format!("创建外挂字幕 HTTP 客户端失败：{error}"))?;
    let mut request = client.get(url).header(
        reqwest::header::ACCEPT,
        "text/vtt,application/x-subrip,text/plain,*/*;q=0.7",
    );
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .map_err(|error| format!("下载外挂字幕失败：{error}"))?
        .error_for_status()
        .map_err(|error| format!("下载外挂字幕返回错误状态：{error}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > EXTERNAL_SUBTITLE_MAX_BYTES)
    {
        return Err("外挂字幕文件过大".to_string());
    }
    let bytes = response
        .bytes()
        .map_err(|error| format!("读取外挂字幕失败：{error}"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > EXTERNAL_SUBTITLE_MAX_BYTES {
        return Err("外挂字幕文件过大".to_string());
    }
    let text = String::from_utf8_lossy(bytes.as_ref());
    let cues = parse_external_subtitle_text(&text, codec, url);
    if cues.is_empty() {
        return Err("外挂字幕中没有可用文本 cue".to_string());
    }
    Ok(cues.into())
}

pub(super) fn decoded_subtitle_cues(
    subtitle: &ffi::AVSubtitle,
    canvas_size: Option<RenderSize>,
    emit_empty_cues: bool,
) -> std::result::Result<Vec<DecodedSubtitleCue>, String> {
    let start_offset_nsecs = u64::from(subtitle.start_display_time).saturating_mul(1_000_000);
    let end_offset_nsecs = if subtitle.end_display_time > subtitle.start_display_time
        && subtitle.end_display_time != u32::MAX
    {
        u64::from(subtitle.end_display_time).saturating_mul(1_000_000)
    } else {
        start_offset_nsecs.saturating_add(FALLBACK_SUBTITLE_DURATION_NSECS)
    };
    let pts_nsecs = (subtitle.pts != ffi::AV_NOPTS_VALUE).then(|| {
        u64::try_from(subtitle.pts)
            .unwrap_or_default()
            .saturating_mul(1_000)
    });

    let rect_count =
        usize::try_from(subtitle.num_rects).map_err(|_| "FFmpeg 字幕矩形数量无效".to_string())?;
    if rect_count == 0 || subtitle.rects.is_null() {
        return Ok(empty_decoded_subtitle_cues(
            emit_empty_cues,
            start_offset_nsecs,
            end_offset_nsecs,
            pts_nsecs,
        ));
    }

    let rects = unsafe { slice::from_raw_parts(subtitle.rects, rect_count) };
    let mut text_parts = Vec::new();
    let mut bitmaps = Vec::new();
    for rect in rects {
        let Some(rect) = (unsafe { rect.as_ref() }) else {
            continue;
        };
        match subtitle_rect_content(rect, canvas_size) {
            Ok(Some(SubtitleRectContent::Text(text))) if !text.trim().is_empty() => {
                text_parts.push(text)
            }
            Ok(Some(SubtitleRectContent::Bitmap(bitmap))) => bitmaps.push(bitmap),
            Ok(Some(SubtitleRectContent::Text(_))) => {}
            Ok(None) => {
                tracing::debug!(subtitle_type = ?rect.type_, "ignoring unsupported subtitle rect");
            }
            Err(error) => {
                tracing::debug!(%error, subtitle_type = ?rect.type_, "ignoring invalid subtitle rect");
            }
        }
    }
    if text_parts.is_empty() && bitmaps.is_empty() {
        return Ok(empty_decoded_subtitle_cues(
            emit_empty_cues,
            start_offset_nsecs,
            end_offset_nsecs,
            pts_nsecs,
        ));
    }

    Ok(vec![DecodedSubtitleCue {
        text: text_parts.join("\n"),
        bitmaps,
        start_offset_nsecs,
        end_offset_nsecs,
        pts_nsecs,
    }])
}

fn empty_decoded_subtitle_cues(
    emit_empty_cues: bool,
    start_offset_nsecs: u64,
    end_offset_nsecs: u64,
    pts_nsecs: Option<u64>,
) -> Vec<DecodedSubtitleCue> {
    if emit_empty_cues {
        vec![DecodedSubtitleCue {
            text: String::new(),
            bitmaps: Vec::new(),
            start_offset_nsecs,
            end_offset_nsecs,
            pts_nsecs,
        }]
    } else {
        Vec::new()
    }
}

enum SubtitleRectContent {
    Text(String),
    Bitmap(BackendSubtitleBitmap),
}

fn subtitle_rect_content(
    rect: &ffi::AVSubtitleRect,
    canvas_size: Option<RenderSize>,
) -> std::result::Result<Option<SubtitleRectContent>, String> {
    match rect.type_ {
        ffi::AVSubtitleType::SUBTITLE_TEXT => Ok(c_string(rect.text)
            .map(|text| SubtitleRectContent::Text(normalize_subtitle_text(&text)))),
        ffi::AVSubtitleType::SUBTITLE_ASS => Ok(c_string(rect.ass).map(|text| {
            SubtitleRectContent::Text(normalize_subtitle_text(&strip_ass_override_tags(
                &ass_dialogue_text(&text),
            )))
        })),
        ffi::AVSubtitleType::SUBTITLE_BITMAP => subtitle_rect_bitmap(rect, canvas_size)
            .map(|bitmap| bitmap.map(SubtitleRectContent::Bitmap)),
        _ => Ok(None),
    }
}

fn subtitle_rect_bitmap(
    rect: &ffi::AVSubtitleRect,
    canvas_size: Option<RenderSize>,
) -> std::result::Result<Option<BackendSubtitleBitmap>, String> {
    if rect.w <= 0 || rect.h <= 0 || rect.data[0].is_null() || rect.data[1].is_null() {
        return Ok(None);
    }
    let x = u32::try_from(rect.x).map_err(|_| "FFmpeg 字幕 bitmap x 坐标无效".to_string())?;
    let y = u32::try_from(rect.y).map_err(|_| "FFmpeg 字幕 bitmap y 坐标无效".to_string())?;
    let width = u32::try_from(rect.w).map_err(|_| "FFmpeg 字幕 bitmap 宽度无效".to_string())?;
    let height = u32::try_from(rect.h).map_err(|_| "FFmpeg 字幕 bitmap 高度无效".to_string())?;
    let canvas_width = canvas_size
        .map(|size| size.width)
        .unwrap_or_else(|| x.saturating_add(width))
        .max(x.saturating_add(width));
    let canvas_height = canvas_size
        .map(|size| size.height)
        .unwrap_or_else(|| y.saturating_add(height))
        .max(y.saturating_add(height));
    let width_usize =
        usize::try_from(width).map_err(|_| "FFmpeg 字幕 bitmap 宽度过大".to_string())?;
    let height_usize =
        usize::try_from(height).map_err(|_| "FFmpeg 字幕 bitmap 高度过大".to_string())?;
    let stride = usize::try_from(rect.linesize[0])
        .map_err(|_| "FFmpeg 字幕 bitmap stride 无效".to_string())?;
    if stride < width_usize {
        return Err("FFmpeg 字幕 bitmap stride 小于宽度".to_string());
    }

    let pixel_count = width_usize
        .checked_mul(height_usize)
        .ok_or_else(|| "FFmpeg 字幕 bitmap 尺寸过大".to_string())?;
    if pixel_count > SUBTITLE_BITMAP_MAX_PIXELS {
        return Err("FFmpeg 字幕 bitmap 尺寸过大".to_string());
    }
    let byte_len = pixel_count
        .checked_mul(4)
        .ok_or_else(|| "FFmpeg 字幕 bitmap 缓冲区过大".to_string())?;
    let palette = unsafe { slice::from_raw_parts(rect.data[1], ffi::AVPALETTE_SIZE as usize) };
    let mut bgra = vec![0; byte_len];
    let mut has_visible_pixel = false;
    for row_index in 0..height_usize {
        let row = unsafe {
            slice::from_raw_parts(
                rect.data[0].add(row_index.saturating_mul(stride)),
                width_usize,
            )
        };
        let dst_row_offset = row_index
            .checked_mul(width_usize)
            .and_then(|offset| offset.checked_mul(4))
            .ok_or_else(|| "FFmpeg 字幕 bitmap 行偏移过大".to_string())?;
        for (column_index, palette_index) in row.iter().copied().enumerate() {
            let palette_offset = usize::from(palette_index)
                .checked_mul(4)
                .ok_or_else(|| "FFmpeg 字幕 palette 索引无效".to_string())?;
            let color = u32::from_ne_bytes([
                palette[palette_offset],
                palette[palette_offset + 1],
                palette[palette_offset + 2],
                palette[palette_offset + 3],
            ]);
            let dst_offset = dst_row_offset + column_index * 4;
            bgra[dst_offset] = (color & 0xff) as u8;
            bgra[dst_offset + 1] = ((color >> 8) & 0xff) as u8;
            bgra[dst_offset + 2] = ((color >> 16) & 0xff) as u8;
            bgra[dst_offset + 3] = ((color >> 24) & 0xff) as u8;
            has_visible_pixel |= bgra[dst_offset + 3] != 0;
        }
    }
    if !has_visible_pixel {
        return Ok(None);
    }

    let image = render_image_from_bgra(bgra, width, height)
        .map_err(|error| format!("创建字幕 bitmap 图像失败：{error}"))?;
    Ok(Some(BackendSubtitleBitmap {
        image,
        x,
        y,
        width,
        height,
        canvas_width,
        canvas_height,
    }))
}

fn text_subtitle_cue(text: String, start_nsecs: u64, end_nsecs: u64) -> BackendSubtitleCue {
    BackendSubtitleCue {
        text,
        bitmaps: Vec::new(),
        start_nsecs,
        end_nsecs,
    }
}

fn c_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn ass_dialogue_text(line: &str) -> String {
    let trimmed = line.trim();
    let payload = trimmed
        .strip_prefix("Dialogue:")
        .or_else(|| trimmed.strip_prefix("Dialogue: "))
        .unwrap_or(trimmed)
        .trim_start();
    let mut fields = payload.splitn(10, ',');
    let mut text = "";
    for index in 0..10 {
        let Some(field) = fields.next() else {
            break;
        };
        if index == 9 {
            text = field;
            break;
        }
    }
    if text.is_empty() {
        trimmed.to_string()
    } else {
        text.to_string()
    }
}

fn strip_ass_override_tags(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '{' => in_tag = true,
            '}' if in_tag => in_tag = false,
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output
}

fn normalize_subtitle_text(text: &str) -> String {
    strip_subtitle_markup(text)
        .replace("\\N", "\n")
        .replace("\\n", "\n")
        .replace("\\h", " ")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_external_subtitle_text(
    text: &str,
    codec: Option<&str>,
    url: &str,
) -> Vec<BackendSubtitleCue> {
    if looks_like_ass(codec, url, text) {
        parse_ass_cues(text)
    } else {
        parse_srt_or_vtt_cues(text)
    }
}

fn looks_like_ass(codec: Option<&str>, url: &str, text: &str) -> bool {
    codec
        .is_some_and(|codec| codec.eq_ignore_ascii_case("ass") || codec.eq_ignore_ascii_case("ssa"))
        || url
            .split('?')
            .next()
            .is_some_and(|path| path.ends_with(".ass") || path.ends_with(".ssa"))
        || text.contains("[Events]") && text.contains("Dialogue:")
}

fn parse_srt_or_vtt_cues(text: &str) -> Vec<BackendSubtitleCue> {
    let normalized = normalize_newlines(text);
    let mut cues = Vec::new();
    let mut lines = normalized.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty()
            || line.eq_ignore_ascii_case("WEBVTT")
            || line.starts_with("NOTE")
            || line.starts_with("STYLE")
        {
            continue;
        }
        let timing_line = if line.contains("-->") {
            line
        } else {
            let Some(next) = lines.next() else {
                break;
            };
            next.trim()
        };
        if !timing_line.contains("-->") {
            continue;
        }
        let Some((start_nsecs, end_nsecs)) = parse_timing_line(timing_line) else {
            continue;
        };
        let mut body = Vec::new();
        while let Some(next) = lines.peek().copied() {
            if next.trim().is_empty() {
                lines.next();
                break;
            }
            body.push(next);
            lines.next();
        }
        let text = normalize_subtitle_text(&body.join("\n"));
        if !text.is_empty() && end_nsecs > start_nsecs {
            cues.push(text_subtitle_cue(text, start_nsecs, end_nsecs));
        }
    }
    cues
}

fn parse_ass_cues(text: &str) -> Vec<BackendSubtitleCue> {
    let normalized = normalize_newlines(text);
    let mut fields: Vec<String> = vec![
        "layer".to_string(),
        "start".to_string(),
        "end".to_string(),
        "style".to_string(),
        "name".to_string(),
        "marginl".to_string(),
        "marginr".to_string(),
        "marginv".to_string(),
        "effect".to_string(),
        "text".to_string(),
    ];
    let mut cues = Vec::new();
    for line in normalized.lines().map(str::trim) {
        if let Some(format) = line.strip_prefix("Format:") {
            fields = format
                .split(',')
                .map(|field| field.trim().to_ascii_lowercase())
                .collect();
            continue;
        }
        let Some(dialogue) = line.strip_prefix("Dialogue:") else {
            continue;
        };
        let field_count = fields.len().max(1);
        let parts: Vec<&str> = dialogue.trim_start().splitn(field_count, ',').collect();
        let start = field_value(&fields, &parts, "start").and_then(parse_subtitle_timecode);
        let end = field_value(&fields, &parts, "end").and_then(parse_subtitle_timecode);
        let text = field_value(&fields, &parts, "text")
            .map(|text| normalize_subtitle_text(&strip_ass_override_tags(text)))
            .unwrap_or_default();
        if let (Some(start_nsecs), Some(end_nsecs)) = (start, end)
            && !text.is_empty()
            && end_nsecs > start_nsecs
        {
            cues.push(text_subtitle_cue(text, start_nsecs, end_nsecs));
        }
    }
    cues
}

fn field_value<'a>(fields: &[String], parts: &'a [&'a str], name: &str) -> Option<&'a str> {
    let index = fields.iter().position(|field| field == name)?;
    parts.get(index).copied().map(str::trim)
}

fn parse_timing_line(line: &str) -> Option<(u64, u64)> {
    let (start, end) = line.split_once("-->")?;
    let end = end.split_whitespace().next()?;
    Some((
        parse_subtitle_timecode(start.trim())?,
        parse_subtitle_timecode(end.trim())?,
    ))
}

fn parse_subtitle_timecode(value: &str) -> Option<u64> {
    let value = value.trim().replace(',', ".");
    let mut parts = value.rsplitn(3, ':');
    let seconds = parts.next()?;
    let minutes = parts.next()?;
    let hours = parts.next().unwrap_or("0");
    let (seconds, millis) = seconds
        .split_once('.')
        .map(|(seconds, fraction)| {
            let millis = fraction
                .chars()
                .take(3)
                .collect::<String>()
                .parse::<u64>()
                .unwrap_or(0);
            (
                seconds,
                millis * 10_u64.saturating_pow(3_u32.saturating_sub(fraction.len() as u32)),
            )
        })
        .unwrap_or((seconds, 0));
    let hours = hours.parse::<u64>().ok()?;
    let minutes = minutes.parse::<u64>().ok()?;
    let seconds = seconds.parse::<u64>().ok()?;
    Some(
        hours
            .saturating_mul(3_600_000_000_000)
            .saturating_add(minutes.saturating_mul(60_000_000_000))
            .saturating_add(seconds.saturating_mul(1_000_000_000))
            .saturating_add(millis.saturating_mul(1_000_000)),
    )
}

fn strip_subtitle_markup(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::ptr;

    #[test]
    fn ass_dialogue_text_extracts_final_field() {
        assert_eq!(
            ass_dialogue_text("Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,hello"),
            "hello"
        );
    }

    #[test]
    fn normalize_ass_text_strips_tags_and_line_breaks() {
        assert_eq!(
            normalize_subtitle_text(&strip_ass_override_tags("{\\an8}第一行\\N第二行")),
            "第一行\n第二行"
        );
    }

    #[test]
    fn decodes_bitmap_subtitle_rects_to_render_images() {
        let mut indexes = vec![0_u8, 1, 1, 0];
        let mut palette = vec![0_u8; ffi::AVPALETTE_SIZE as usize];
        palette[0..4].copy_from_slice(&0x00000000_u32.to_ne_bytes());
        palette[4..8].copy_from_slice(&0xff112233_u32.to_ne_bytes());
        let mut rect = ffi::AVSubtitleRect {
            x: 10,
            y: 20,
            w: 2,
            h: 2,
            nb_colors: 2,
            data: [
                indexes.as_mut_ptr(),
                palette.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            ],
            linesize: [2, 0, 0, 0],
            flags: 0,
            type_: ffi::AVSubtitleType::SUBTITLE_BITMAP,
            text: ptr::null_mut(),
            ass: ptr::null_mut(),
        };
        let mut rect_ptr = &mut rect as *mut ffi::AVSubtitleRect;
        let subtitle = ffi::AVSubtitle {
            format: 0,
            start_display_time: 100,
            end_display_time: 900,
            num_rects: 1,
            rects: &mut rect_ptr,
            pts: 1_234,
        };

        let cues = decoded_subtitle_cues(
            &subtitle,
            Some(RenderSize {
                width: 1920,
                height: 1080,
            }),
            false,
        )
        .unwrap();

        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "");
        assert_eq!(cues[0].start_offset_nsecs, 100_000_000);
        assert_eq!(cues[0].end_offset_nsecs, 900_000_000);
        assert_eq!(cues[0].pts_nsecs, Some(1_234_000));
        assert_eq!(cues[0].bitmaps.len(), 1);
        let bitmap = &cues[0].bitmaps[0];
        assert_eq!(
            (bitmap.x, bitmap.y, bitmap.width, bitmap.height),
            (10, 20, 2, 2)
        );
        assert_eq!((bitmap.canvas_width, bitmap.canvas_height), (1920, 1080));
        assert_eq!(
            bitmap.image.as_bytes(0).unwrap(),
            &[
                0, 0, 0, 0, 0x33, 0x22, 0x11, 0xff, 0x33, 0x22, 0x11, 0xff, 0, 0, 0, 0
            ]
        );
    }

    #[test]
    fn decoded_subtitle_uses_fallback_duration_for_unbounded_end_time() {
        let text = CString::new("PGS fallback").unwrap();
        let mut rect = ffi::AVSubtitleRect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            nb_colors: 0,
            data: [ptr::null_mut(); 4],
            linesize: [0; 4],
            flags: 0,
            type_: ffi::AVSubtitleType::SUBTITLE_TEXT,
            text: text.as_ptr() as *mut _,
            ass: ptr::null_mut(),
        };
        let mut rect_ptr = &mut rect as *mut ffi::AVSubtitleRect;
        let subtitle = ffi::AVSubtitle {
            format: 0,
            start_display_time: 250,
            end_display_time: u32::MAX,
            num_rects: 1,
            rects: &mut rect_ptr,
            pts: ffi::AV_NOPTS_VALUE,
        };

        let cues = decoded_subtitle_cues(&subtitle, None, false).unwrap();

        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_offset_nsecs, 250_000_000);
        assert_eq!(cues[0].end_offset_nsecs, 4_250_000_000);
    }

    #[test]
    fn decoded_subtitle_emits_empty_clear_cue_when_requested() {
        let subtitle = ffi::AVSubtitle {
            format: 0,
            start_display_time: 0,
            end_display_time: 0,
            num_rects: 0,
            rects: ptr::null_mut(),
            pts: 12_000,
        };

        assert!(
            decoded_subtitle_cues(&subtitle, None, false)
                .unwrap()
                .is_empty()
        );

        let cues = decoded_subtitle_cues(&subtitle, None, true).unwrap();

        assert_eq!(cues.len(), 1);
        assert!(!cues[0].has_content());
        assert_eq!(cues[0].start_offset_nsecs, 0);
        assert_eq!(cues[0].pts_nsecs, Some(12_000_000));
    }

    #[test]
    fn decoded_subtitle_treats_transparent_bitmap_as_empty_clear_cue() {
        let mut indexes = vec![1_u8; 4];
        let mut palette = vec![0_u8; ffi::AVPALETTE_SIZE as usize];
        palette[4..8].copy_from_slice(&0x00112233_u32.to_ne_bytes());
        let mut rect = ffi::AVSubtitleRect {
            x: 10,
            y: 20,
            w: 2,
            h: 2,
            nb_colors: 2,
            data: [
                indexes.as_mut_ptr(),
                palette.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            ],
            linesize: [2, 0, 0, 0],
            flags: 0,
            type_: ffi::AVSubtitleType::SUBTITLE_BITMAP,
            text: ptr::null_mut(),
            ass: ptr::null_mut(),
        };
        let mut rect_ptr = &mut rect as *mut ffi::AVSubtitleRect;
        let subtitle = ffi::AVSubtitle {
            format: 0,
            start_display_time: 0,
            end_display_time: 0,
            num_rects: 1,
            rects: &mut rect_ptr,
            pts: 12_000,
        };

        assert!(
            decoded_subtitle_cues(&subtitle, None, false)
                .unwrap()
                .is_empty()
        );

        let cues = decoded_subtitle_cues(&subtitle, None, true).unwrap();

        assert_eq!(cues.len(), 1);
        assert!(!cues[0].has_content());
        assert_eq!(cues[0].pts_nsecs, Some(12_000_000));
    }

    #[test]
    fn parses_srt_external_cues() {
        let cues = parse_external_subtitle_text(
            "1\n00:00:01,500 --> 00:00:03,000\n<font>第一行</font>\\N第二行\n",
            Some("srt"),
            "https://example.com/sub.srt",
        );

        assert_eq!(
            cues,
            vec![text_subtitle_cue(
                "第一行\n第二行".to_string(),
                1_500_000_000,
                3_000_000_000,
            )]
        );
    }

    #[test]
    fn parses_ass_external_cues() {
        let cues = parse_external_subtitle_text(
            "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:02.00,0:00:04.50,Default,,0,0,0,,{\\an8}你好\\N世界\n",
            Some("ass"),
            "https://example.com/sub.ass",
        );

        assert_eq!(
            cues,
            vec![text_subtitle_cue(
                "你好\n世界".to_string(),
                2_000_000_000,
                4_500_000_000,
            )]
        );
    }
}
