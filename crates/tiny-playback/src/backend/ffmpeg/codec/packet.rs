use super::*;

impl AvPacket {
    pub(in super::super) fn new() -> std::result::Result<Self, String> {
        let ptr = unsafe { ffi::av_packet_alloc() };
        if ptr.is_null() {
            return Err("FFmpeg 分配 packet 失败".to_string());
        }
        Ok(Self {
            ptr,
            read_diagnostic: None,
            nal_format: None,
        })
    }

    pub(in super::super) fn ref_from(packet: &Self) -> std::result::Result<Self, String> {
        let mut clone = Self::new()?;
        let result = unsafe { ffi::av_packet_ref(clone.ptr, packet.ptr) };
        if result < 0 {
            return Err(format!("FFmpeg 复制 packet 失败：{}", ffmpeg_error(result)));
        }
        clone.read_diagnostic = packet.read_diagnostic.clone();
        clone.nal_format = packet.nal_format;
        Ok(clone)
    }

    pub(in super::super) fn props_from(packet: &Self) -> std::result::Result<Self, String> {
        let mut props = Self::new()?;
        let result = unsafe { ffi::av_packet_copy_props(props.ptr, packet.ptr) };
        if result < 0 {
            return Err(format!(
                "FFmpeg 复制 packet metadata 失败：{}",
                ffmpeg_error(result)
            ));
        }
        unsafe {
            (*props.ptr).stream_index = (*packet.ptr).stream_index;
        }
        props.read_diagnostic = packet.read_diagnostic.clone();
        props.nal_format = packet.nal_format;
        Ok(props)
    }

    pub(in super::super) fn from_data_and_props(
        data: &[u8],
        props: &Self,
    ) -> std::result::Result<Self, String> {
        let mut packet = Self::new()?;
        if !data.is_empty() {
            let size = c_int::try_from(data.len())
                .map_err(|_| "FFmpeg packet payload 过大".to_string())?;
            let result = unsafe { ffi::av_new_packet(packet.ptr, size) };
            if result < 0 {
                return Err(format!(
                    "FFmpeg 分配 packet payload 失败：{}",
                    ffmpeg_error(result)
                ));
            }
            let target = unsafe { (*packet.ptr).data };
            if target.is_null() {
                return Err("FFmpeg packet payload 为空".to_string());
            }
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), target, data.len()) };
        }
        let result = unsafe { ffi::av_packet_copy_props(packet.ptr, props.ptr) };
        if result < 0 {
            return Err(format!(
                "FFmpeg 恢复 packet metadata 失败：{}",
                ffmpeg_error(result)
            ));
        }
        unsafe {
            (*packet.ptr).stream_index = (*props.ptr).stream_index;
        }
        packet.read_diagnostic = props.read_diagnostic.clone();
        packet.nal_format = props.nal_format;
        Ok(packet)
    }

    pub(in super::super) fn as_ptr(&self) -> *const ffi::AVPacket {
        self.ptr
    }

    pub(in super::super) fn as_mut_ptr(&mut self) -> *mut ffi::AVPacket {
        self.ptr
    }

    pub(in super::super) fn stream_index(&self) -> c_int {
        unsafe { (*self.ptr).stream_index }
    }

    pub(in super::super) fn pts(&self) -> Option<i64> {
        let pts = unsafe { (*self.ptr).pts };
        (pts != ffi::AV_NOPTS_VALUE).then_some(pts)
    }

    pub(in super::super) fn dts(&self) -> Option<i64> {
        let dts = unsafe { (*self.ptr).dts };
        (dts != ffi::AV_NOPTS_VALUE).then_some(dts)
    }

    pub(in super::super) fn position(&self) -> Option<i64> {
        let position = unsafe { (*self.ptr).pos };
        (position >= 0).then_some(position)
    }

    pub(in super::super) fn best_timestamp(&self) -> Option<i64> {
        unsafe {
            if (*self.ptr).pts != ffi::AV_NOPTS_VALUE {
                Some((*self.ptr).pts)
            } else if (*self.ptr).dts != ffi::AV_NOPTS_VALUE {
                Some((*self.ptr).dts)
            } else {
                None
            }
        }
    }

    pub(in super::super) fn duration(&self) -> Option<i64> {
        let duration = unsafe { (*self.ptr).duration };
        (duration > 0).then_some(duration)
    }

    pub(in super::super) fn is_key(&self) -> bool {
        unsafe { (*self.ptr).flags & ffi::AV_PKT_FLAG_KEY != 0 }
    }

    pub(in super::super) fn flags(&self) -> c_int {
        unsafe { (*self.ptr).flags }
    }

    pub(in super::super) fn read_diagnostic(&self) -> Option<AvPacketReadDiagnostic> {
        self.read_diagnostic.as_deref().copied()
    }

    pub(in super::super) fn set_read_diagnostic(&mut self, diagnostic: AvPacketReadDiagnostic) {
        self.read_diagnostic = Some(Arc::new(diagnostic));
    }

    pub(in super::super) fn set_nal_format(&mut self, format: Option<NalPacketFormat>) {
        self.nal_format = format;
    }

    pub(in super::super) fn size(&self) -> Option<u64> {
        let size = unsafe { (*self.ptr).size };
        (size > 0).then_some(size as u64)
    }

    pub(in super::super) fn byte_len(&self) -> usize {
        self.size()
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(0)
    }

    pub(in super::super) fn properties_byte_len(&self) -> usize {
        // FFmpeg copies side data when referencing a packet or its properties.
        // It remains resident even after the compressed payload moves to disk.
        let packet = unsafe { &*self.ptr };
        let mut bytes = std::mem::size_of::<ffi::AVPacket>();
        if !packet.side_data.is_null() && packet.side_data_elems > 0 {
            let side_data =
                unsafe { slice::from_raw_parts(packet.side_data, packet.side_data_elems as usize) };
            for entry in side_data {
                bytes = bytes
                    .saturating_add(std::mem::size_of_val(entry))
                    .saturating_add(entry.size);
            }
        }
        bytes
    }

    pub(in super::super) fn data(&self) -> Option<&[u8]> {
        let (data, size) = unsafe { ((*self.ptr).data, (*self.ptr).size) };
        if data.is_null() || size <= 0 {
            return None;
        }
        Some(unsafe { slice::from_raw_parts(data, usize::try_from(size).ok()?) })
    }

    pub(in super::super) fn unref(&mut self) {
        unsafe { ffi::av_packet_unref(self.ptr) };
        self.read_diagnostic = None;
        self.nal_format = None;
    }
}
