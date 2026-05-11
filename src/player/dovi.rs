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
    pub rpu_nalu: Vec<u8>,
    pub rpu_payload: Vec<u8>,
}

impl DoviFrameMetadata {
    pub fn from_rpu_payload(rpu_payload: &[u8]) -> Result<Self> {
        let rpu = DoviRpu::parse_rpu(rpu_payload).context("解析 Dolby Vision RPU payload 失败")?;
        Ok(Self {
            profile: rpu.dovi_profile,
            rpu_nalu: Vec::new(),
            rpu_payload: rpu_payload.to_vec(),
        })
    }

    pub fn from_unspec62_nalu(nalu: &[u8]) -> Result<Self> {
        let rpu =
            DoviRpu::parse_unspec62_nalu(nalu).context("解析 Dolby Vision UNSPEC62 NALU 失败")?;
        let rpu_payload = rpu
            .write_rpu()
            .context("写出 Dolby Vision RPU payload 失败")?;

        Ok(Self {
            profile: rpu.dovi_profile,
            rpu_nalu: nalu.to_vec(),
            rpu_payload,
        })
    }

    pub fn parse_rpu(&self) -> Result<DoviRpu> {
        DoviRpu::parse_rpu(&self.rpu_payload).context("解析 Dolby Vision RPU 失败")
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

        if let Some(metadata) = metadata_from_nalu(&data[nalu_start..nalu_end])? {
            return Ok(Some(metadata));
        }
        offset = nalu_end;
    }

    Ok(None)
}

fn metadata_from_nalu(nalu: &[u8]) -> Result<Option<DoviFrameMetadata>> {
    if nalu_type(nalu) != Some(62) {
        return Ok(None);
    }

    DoviFrameMetadata::from_unspec62_nalu(nalu).map(Some)
}

fn nalu_type(nalu: &[u8]) -> Option<u8> {
    nalu.first().map(|header| (header >> 1) & 0x3f)
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
}
