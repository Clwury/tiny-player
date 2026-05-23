use anyhow::{Context, Result, anyhow, bail};
use dolby_vision::rpu::dovi_rpu::DoviRpu;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HevcStreamFormat {
    ByteStream,
    #[allow(dead_code)]
    LengthPrefixed {
        length_size: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoviFrameMetadata {
    pub profile: u8,
    pub profile5: bool,
    pub rpu_nalu: Vec<u8>,
    pub rpu_payload: Vec<u8>,
}

impl DoviFrameMetadata {
    pub fn from_rpu_payload(rpu_payload: &[u8]) -> Result<Self> {
        let rpu = DoviRpu::parse_rpu(rpu_payload).context("解析 Dolby Vision RPU payload 失败")?;
        let profile5 = dovi_rpu_is_profile5(&rpu);
        let rpu_payload = rpu
            .write_rpu()
            .context("写出 Dolby Vision RPU payload 失败")?;
        Ok(Self {
            profile: rpu.dovi_profile,
            profile5,
            rpu_nalu: Vec::new(),
            rpu_payload,
        })
    }

    pub fn from_unspec62_nalu(nalu: &[u8]) -> Result<Self> {
        let nalu = trim_annex_b_trailing_zeroes(nalu);
        let (rpu, parsed_nalu) = parse_unspec62_nalu_with_trailing_payload_fallback(nalu)
            .context("解析 Dolby Vision UNSPEC62 NALU 失败")?;
        let profile5 = dovi_rpu_is_profile5(&rpu);
        let rpu_payload = rpu
            .write_rpu()
            .context("写出 Dolby Vision RPU payload 失败")?;

        Ok(Self {
            profile: rpu.dovi_profile,
            profile5,
            rpu_nalu: parsed_nalu.to_vec(),
            rpu_payload,
        })
    }

    pub fn is_profile5(&self) -> bool {
        self.profile5
    }

    pub fn parse_rpu(&self) -> Result<DoviRpu> {
        DoviRpu::parse_rpu(&self.rpu_payload).context("解析 Dolby Vision RPU 失败")
    }
}

pub fn dovi_rpu_is_profile5(rpu: &DoviRpu) -> bool {
    rpu.dovi_profile == 5 || (rpu.header.rpu_type == 2 && rpu.header.vdr_rpu_profile == 0)
}

fn parse_unspec62_nalu_with_trailing_payload_fallback(nalu: &[u8]) -> Result<(DoviRpu, &[u8])> {
    match DoviRpu::parse_unspec62_nalu(nalu) {
        Ok(rpu) => Ok((rpu, nalu)),
        Err(error) => {
            let original_error = error;
            for end in (25..nalu.len()).rev() {
                if nalu[end - 1] != 0x80 {
                    continue;
                }
                let candidate = &nalu[..end];
                if let Ok(rpu) = DoviRpu::parse_unspec62_nalu(candidate) {
                    return Ok((rpu, candidate));
                }
            }
            Err(original_error)
        }
    }
}

#[derive(Default)]
pub struct DoviRpuExtractor;

impl DoviRpuExtractor {
    pub fn extract_from_hevc_access_unit(
        &mut self,
        data: &[u8],
        stream_format: HevcStreamFormat,
    ) -> Result<Option<DoviFrameMetadata>> {
        match stream_format {
            HevcStreamFormat::ByteStream => extract_from_annex_b(data),
            HevcStreamFormat::LengthPrefixed { length_size } => {
                extract_from_length_prefixed(data, length_size)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoviRpuStripResult {
    pub data: Vec<u8>,
    pub metadata: Option<DoviFrameMetadata>,
    pub stream_format: HevcStreamFormat,
    pub nal_count: usize,
    pub stripped_nal_count: usize,
    pub stripped_bytes: usize,
}

pub fn strip_dovi_rpu_nalus(data: &[u8]) -> Option<DoviRpuStripResult> {
    if starts_with_start_code(data)
        && let Some(result) = strip_annex_b_dovi_rpu_nalus(data)
    {
        return Some(result);
    }

    for length_size in [4, 3, 2, 1] {
        if let Some(result) = strip_length_prefixed_dovi_rpu_nalus(data, length_size) {
            return Some(result);
        }
    }

    if !starts_with_start_code(data) {
        return strip_annex_b_dovi_rpu_nalus(data);
    }

    None
}

fn extract_from_annex_b(data: &[u8]) -> Result<Option<DoviFrameMetadata>> {
    let mut offset = 0;
    while let Some(start) = find_start_code(data, offset) {
        let nalu_start = start.next_offset;
        let next = find_start_code(data, nalu_start)
            .map(|start| start.offset)
            .unwrap_or(data.len());
        if let Some(metadata) = metadata_from_nalu(&data[nalu_start..next])? {
            return Ok(Some(metadata));
        }
        offset = next;
    }

    Ok(None)
}

fn strip_annex_b_dovi_rpu_nalus(data: &[u8]) -> Option<DoviRpuStripResult> {
    let mut result = Vec::with_capacity(data.len());
    let mut offset = 0;
    let mut copied_until = 0;
    let mut nal_count = 0usize;
    let mut stripped_nal_count = 0usize;
    let mut stripped_bytes = 0usize;
    let mut metadata = None;

    while let Some(start) = find_start_code(data, offset) {
        let nalu_start = start.next_offset;
        let next = find_start_code(data, nalu_start)
            .map(|start| start.offset)
            .unwrap_or(data.len());
        nal_count = nal_count.saturating_add(1);
        let nalu = trim_annex_b_trailing_zeroes(&data[nalu_start..next]);
        if nalu_type(nalu) == Some(62) {
            if metadata.is_none() {
                capture_metadata_from_nalu(&mut metadata, nalu);
            }
            result.extend_from_slice(&data[copied_until..start.offset]);
            stripped_nal_count = stripped_nal_count.saturating_add(1);
            stripped_bytes = stripped_bytes.saturating_add(next.saturating_sub(start.offset));
        } else {
            result.extend_from_slice(&data[copied_until..next]);
        }
        copied_until = next;
        offset = next;
    }

    if stripped_nal_count == 0 {
        return None;
    }
    result.extend_from_slice(&data[copied_until..]);
    Some(DoviRpuStripResult {
        data: result,
        metadata,
        stream_format: HevcStreamFormat::ByteStream,
        nal_count,
        stripped_nal_count,
        stripped_bytes,
    })
}

fn extract_from_length_prefixed(
    data: &[u8],
    length_size: usize,
) -> Result<Option<DoviFrameMetadata>> {
    if !(1..=4).contains(&length_size) {
        bail!("invalid HEVC NAL length size");
    }

    let mut offset = 0;
    while offset < data.len() {
        let length_end = offset
            .checked_add(length_size)
            .ok_or_else(|| anyhow!("invalid HEVC NAL length"))?;
        if length_end > data.len() {
            bail!("truncated HEVC NAL length");
        }

        let mut nalu_len = 0usize;
        for byte in &data[offset..length_end] {
            nalu_len = (nalu_len << 8) | usize::from(*byte);
        }
        let nalu_start = length_end;
        let nalu_end = nalu_start
            .checked_add(nalu_len)
            .ok_or_else(|| anyhow!("invalid HEVC NAL length"))?;
        if nalu_end > data.len() {
            bail!("truncated HEVC NAL data");
        }

        let nalu = trim_annex_b_trailing_zeroes(&data[nalu_start..nalu_end]);
        if let Some(metadata) = metadata_from_nalu(nalu)? {
            return Ok(Some(metadata));
        }
        offset = nalu_end;
    }

    Ok(None)
}

fn strip_length_prefixed_dovi_rpu_nalus(
    data: &[u8],
    length_size: usize,
) -> Option<DoviRpuStripResult> {
    let mut offset = 0;
    let mut result = Vec::with_capacity(data.len());
    let mut nal_count = 0usize;
    let mut stripped_nal_count = 0usize;
    let mut stripped_bytes = 0usize;
    let mut metadata = None;

    while offset < data.len() {
        let length_end = offset.checked_add(length_size)?;
        if length_end > data.len() {
            return None;
        }
        let nalu_len = read_be_nal_len(&data[offset..length_end])?;
        if nalu_len == 0 {
            return None;
        }
        let nalu_start = length_end;
        let nalu_end = nalu_start.checked_add(nalu_len)?;
        if nalu_end > data.len() {
            return None;
        }

        nal_count = nal_count.saturating_add(1);
        let nalu = trim_annex_b_trailing_zeroes(&data[nalu_start..nalu_end]);
        if nalu_type(nalu) == Some(62) {
            if metadata.is_none() {
                capture_metadata_from_nalu(&mut metadata, nalu);
            }
            stripped_nal_count = stripped_nal_count.saturating_add(1);
            stripped_bytes = stripped_bytes.saturating_add(nalu_end.saturating_sub(offset));
        } else {
            result.extend_from_slice(&data[offset..nalu_end]);
        }
        offset = nalu_end;
    }

    if stripped_nal_count == 0 {
        return None;
    }
    Some(DoviRpuStripResult {
        data: result,
        metadata,
        stream_format: HevcStreamFormat::LengthPrefixed { length_size },
        nal_count,
        stripped_nal_count,
        stripped_bytes,
    })
}

fn capture_metadata_from_nalu(metadata: &mut Option<DoviFrameMetadata>, nalu: &[u8]) {
    match metadata_from_nalu(nalu) {
        Ok(Some(parsed)) => *metadata = Some(parsed),
        Ok(None) => {}
        Err(error) => {
            let prefix_len = nalu.len().min(16);
            tracing::debug!(
                error = %error,
                nalu_len = nalu.len(),
                nalu_type = ?nalu_type(nalu),
                nalu_prefix = ?&nalu[..prefix_len],
                "failed to parse stripped Dolby Vision RPU NAL"
            );
        }
    }
}

fn metadata_from_nalu(nalu: &[u8]) -> Result<Option<DoviFrameMetadata>> {
    if nalu_type(nalu) == Some(62) {
        return DoviFrameMetadata::from_unspec62_nalu(nalu).map(Some);
    }

    if is_rpu_payload_candidate(nalu) {
        return DoviFrameMetadata::from_rpu_payload(nalu).map(Some);
    }

    Ok(None)
}

fn nalu_type(nalu: &[u8]) -> Option<u8> {
    nalu.first().map(|header| (header >> 1) & 0x3f)
}

fn read_be_nal_len(bytes: &[u8]) -> Option<usize> {
    let mut len = 0usize;
    for byte in bytes {
        len = len.checked_shl(8)?.checked_add(usize::from(*byte))?;
    }
    Some(len)
}

fn trim_annex_b_trailing_zeroes(nalu: &[u8]) -> &[u8] {
    let mut end = nalu.len();
    while end > 0 && nalu[end - 1] == 0 {
        end -= 1;
    }
    &nalu[..end]
}

fn is_rpu_payload_candidate(data: &[u8]) -> bool {
    if data.len() < 5 {
        return false;
    }

    matches!(
        &data[..5],
        [0, 0, 0, 1, 25]
            | [0, 0, 1, 25, 8]
            | [0, 1, 25, 8, 9]
            | [124, 1, 25, 8, 9]
            | [1, 25, 8, 9, _]
            | [25, 8, 9, _, _]
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StartCode {
    offset: usize,
    next_offset: usize,
}

fn find_start_code(data: &[u8], mut offset: usize) -> Option<StartCode> {
    while offset + 3 <= data.len() {
        if data[offset] == 0 && data[offset + 1] == 0 {
            if data[offset + 2] == 1 {
                return Some(StartCode {
                    offset,
                    next_offset: offset + 3,
                });
            }
            if offset + 4 <= data.len() && data[offset + 2] == 0 && data[offset + 3] == 1 {
                return Some(StartCode {
                    offset,
                    next_offset: offset + 4,
                });
            }
        }
        offset += 1;
    }

    None
}

fn starts_with_start_code(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_access_unit_without_rpu_returns_none() {
        let mut extractor = DoviRpuExtractor;
        let data = [
            0, 0, 0, 1, 0x40, 0x01, 0xaa, 0xbb, 0, 0, 1, 0x26, 0x01, 0xcc,
        ];

        let metadata = extractor
            .extract_from_hevc_access_unit(&data, HevcStreamFormat::ByteStream)
            .unwrap();

        assert!(metadata.is_none());
    }

    #[test]
    fn length_prefixed_access_unit_without_rpu_returns_none() {
        let mut extractor = DoviRpuExtractor;
        let data = [0, 0, 0, 3, 0x40, 0x01, 0xaa, 0, 0, 0, 3, 0x26, 0x01, 0xbb];

        let metadata = extractor
            .extract_from_hevc_access_unit(
                &data,
                HevcStreamFormat::LengthPrefixed { length_size: 4 },
            )
            .unwrap();

        assert!(metadata.is_none());
    }

    #[test]
    fn length_prefixed_raw_rpu_payload_extracts_metadata() {
        let mut extractor = DoviRpuExtractor;
        let payload = profile5_rpu_payload();
        let mut data = Vec::new();
        data.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        data.extend_from_slice(&payload);

        let metadata = extractor
            .extract_from_hevc_access_unit(
                &data,
                HevcStreamFormat::LengthPrefixed { length_size: 4 },
            )
            .unwrap()
            .unwrap();

        assert_eq!(metadata.profile, 5);
        assert!(metadata.is_profile5());
        assert_eq!(metadata.rpu_payload, payload);
    }

    #[test]
    fn unspec62_nalu_extracts_metadata_with_nonzero_trailing_payload() {
        let payload = profile5_rpu_payload();
        let mut nalu = DoviRpu::parse_rpu(&payload)
            .unwrap()
            .write_hevc_unspec62_nalu()
            .unwrap();
        let original_nalu = nalu.clone();
        nalu.extend_from_slice(&[0xaa, 0xbb, 0xcc]);

        let metadata = DoviFrameMetadata::from_unspec62_nalu(&nalu).unwrap();

        assert_eq!(metadata.profile, 5);
        assert!(metadata.is_profile5());
        assert_eq!(metadata.rpu_nalu, original_nalu);
        assert_eq!(metadata.rpu_payload, payload);
    }

    #[test]
    fn profile5_detection_accepts_ffmpeg_style_rpu_header() {
        let payload = profile5_rpu_payload();
        let mut rpu = DoviRpu::parse_rpu(&payload).unwrap();
        rpu.dovi_profile = 0;

        assert!(dovi_rpu_is_profile5(&rpu));
    }

    #[test]
    fn profile5_detection_accepts_profile0_partial_header_rpu() {
        let payload = profile5_rpu_payload();
        let mut rpu = DoviRpu::parse_rpu(&payload).unwrap();
        rpu.dovi_profile = 0;
        rpu.header.disable_residual_flag = false;
        rpu.header.vdr_seq_info_present_flag = false;

        assert!(dovi_rpu_is_profile5(&rpu));
    }

    #[test]
    fn profile5_detection_rejects_default_unparsed_rpu() {
        assert!(!dovi_rpu_is_profile5(&DoviRpu::default()));
    }

    #[test]
    fn malformed_rpu_nalu_returns_error() {
        let mut extractor = DoviRpuExtractor;
        let data = [0, 0, 1, 0x7c, 0x01, 0xaa, 0xbb];

        let error = extractor
            .extract_from_hevc_access_unit(&data, HevcStreamFormat::ByteStream)
            .unwrap_err();

        assert!(error.to_string().contains("UNSPEC62"));
    }

    #[test]
    fn rejects_invalid_length_size() {
        let mut extractor = DoviRpuExtractor;

        assert!(
            extractor
                .extract_from_hevc_access_unit(
                    &[],
                    HevcStreamFormat::LengthPrefixed { length_size: 0 }
                )
                .is_err()
        );
    }

    #[test]
    fn detects_hevc_nalu_type() {
        assert_eq!(nalu_type(&[0x7c, 0x01]), Some(62));
        assert_eq!(nalu_type(&[0x26, 0x01]), Some(19));
        assert_eq!(nalu_type(&[]), None);
    }

    #[test]
    fn strips_annex_b_dovi_rpu_nalus() {
        let data = [
            0, 0, 0, 1, 0x26, 0x01, 0xaa, 0, 0, 1, 0x7c, 0x01, 0xbb, 0, 0, 0, 1, 0x02, 0x01, 0xcc,
        ];

        let stripped = strip_dovi_rpu_nalus(&data).unwrap();

        assert_eq!(stripped.stream_format, HevcStreamFormat::ByteStream);
        assert_eq!(stripped.nal_count, 3);
        assert_eq!(stripped.stripped_nal_count, 1);
        assert_eq!(
            stripped.data,
            vec![0, 0, 0, 1, 0x26, 0x01, 0xaa, 0, 0, 0, 1, 0x02, 0x01, 0xcc]
        );
    }

    #[test]
    fn strips_length_prefixed_dovi_rpu_nalus() {
        let data = [
            0, 0, 0, 3, 0x26, 0x01, 0xaa, 0, 0, 0, 3, 0x7c, 0x01, 0xbb, 0, 0, 0, 3, 0x02, 0x01,
            0xcc,
        ];

        let stripped = strip_dovi_rpu_nalus(&data).unwrap();

        assert_eq!(
            stripped.stream_format,
            HevcStreamFormat::LengthPrefixed { length_size: 4 }
        );
        assert_eq!(stripped.nal_count, 3);
        assert_eq!(stripped.stripped_nal_count, 1);
        assert_eq!(
            stripped.data,
            vec![0, 0, 0, 3, 0x26, 0x01, 0xaa, 0, 0, 0, 3, 0x02, 0x01, 0xcc]
        );
    }

    #[test]
    fn stripping_length_prefixed_dovi_rpu_keeps_metadata() {
        let payload = profile5_rpu_payload();
        let rpu_nalu = DoviRpu::parse_rpu(&payload)
            .unwrap()
            .write_hevc_unspec62_nalu()
            .unwrap();
        let mut data = vec![0, 0, 0, 3, 0x26, 0x01, 0xaa];
        data.extend_from_slice(&(rpu_nalu.len() as u32).to_be_bytes());
        data.extend_from_slice(&rpu_nalu);

        let stripped = strip_dovi_rpu_nalus(&data).unwrap();
        let metadata = stripped.metadata.unwrap();

        assert_eq!(
            stripped.stream_format,
            HevcStreamFormat::LengthPrefixed { length_size: 4 }
        );
        assert_eq!(stripped.stripped_nal_count, 1);
        assert_eq!(metadata.profile, 5);
        assert!(metadata.is_profile5());
        assert_eq!(metadata.rpu_payload, payload);
    }

    #[test]
    fn strips_length_prefixed_dovi_rpu_when_payload_contains_start_code_bytes() {
        let data = [
            0, 0, 0, 7, 0x02, 0x01, 0, 0, 1, 0x26, 0x01, 0, 0, 0, 3, 0x7c, 0x01, 0xbb,
        ];

        let stripped = strip_dovi_rpu_nalus(&data).unwrap();

        assert_eq!(
            stripped.stream_format,
            HevcStreamFormat::LengthPrefixed { length_size: 4 }
        );
        assert_eq!(stripped.nal_count, 2);
        assert_eq!(stripped.stripped_nal_count, 1);
        assert_eq!(
            stripped.data,
            vec![0, 0, 0, 7, 0x02, 0x01, 0, 0, 1, 0x26, 0x01]
        );
    }

    #[test]
    fn strip_returns_none_without_dovi_rpu_nalus() {
        let annex_b = [0, 0, 1, 0x26, 0x01, 0xaa, 0, 0, 1, 0x02, 0x01, 0xbb];
        let length_prefixed = [0, 0, 0, 3, 0x26, 0x01, 0xaa];

        assert!(strip_dovi_rpu_nalus(&annex_b).is_none());
        assert!(strip_dovi_rpu_nalus(&length_prefixed).is_none());
    }

    #[test]
    fn finds_three_and_four_byte_start_codes() {
        let data = [9, 0, 0, 1, 1, 2, 0, 0, 0, 1, 3];

        assert_eq!(
            find_start_code(&data, 0),
            Some(StartCode {
                offset: 1,
                next_offset: 4,
            })
        );
        assert_eq!(
            find_start_code(&data, 4),
            Some(StartCode {
                offset: 6,
                next_offset: 10,
            })
        );
    }

    fn profile5_rpu_payload() -> Vec<u8> {
        use dolby_vision::rpu::generate::{GenerateConfig, GenerateProfile};

        let config = GenerateConfig {
            profile: GenerateProfile::Profile5,
            ..Default::default()
        };
        DoviRpu::profile5_config(&config)
            .unwrap()
            .write_rpu()
            .unwrap()
    }
}
