use serde::Serialize;

const LOCAL_DEVICE_BITRATE: u32 = 40_000_000;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceProfileConfig {
    device_profile: DeviceProfile,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct DeviceProfile {
    codec_profiles: Vec<CodecProfile>,
    max_streaming_bitrate: u32,
    subtitle_profiles: Vec<SubtitleProfile>,
    response_profiles: Vec<ResponseProfile>,
    music_streaming_transcoding_bitrate: u32,
    max_static_bitrate: u32,
    transcoding_profiles: Vec<TranscodingProfile>,
    direct_play_profiles: Vec<DirectPlayProfile>,
    container_profiles: Vec<ContainerProfile>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct CodecProfile {
    r#type: &'static str,
    codec: &'static str,
    apply_conditions: Vec<ProfileCondition>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct ProfileCondition {
    is_required: bool,
    property: &'static str,
    value: &'static str,
    condition: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct SubtitleProfile {
    format: &'static str,
    method: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct ResponseProfile {
    container: &'static str,
    r#type: &'static str,
    mime_type: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct TranscodingProfile {
    audio_codec: &'static str,
    min_segments: u8,
    break_on_non_key_frames: bool,
    protocol: &'static str,
    video_codec: &'static str,
    r#type: &'static str,
    max_audio_channels: &'static str,
    container: &'static str,
    context: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct DirectPlayProfile {
    container: &'static str,
    video_codec: &'static str,
    r#type: &'static str,
    audio_codec: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct ContainerProfile {}

pub fn device_profile() -> DeviceProfileConfig {
    DeviceProfileConfig {
        device_profile: DeviceProfile {
            codec_profiles: vec![
                CodecProfile {
                    r#type: "Video",
                    codec: "h264",
                    apply_conditions: vec![
                        ProfileCondition {
                            is_required: false,
                            property: "IsAnamorphic",
                            value: "true",
                            condition: "NotEquals",
                        },
                        ProfileCondition {
                            is_required: false,
                            property: "VideoProfile",
                            value: "high|main|baseline|constrained baseline",
                            condition: "EqualsAny",
                        },
                        ProfileCondition {
                            is_required: false,
                            property: "VideoLevel",
                            value: "80",
                            condition: "LessThanEqual",
                        },
                        ProfileCondition {
                            is_required: false,
                            property: "IsInterlaced",
                            value: "true",
                            condition: "NotEquals",
                        },
                    ],
                },
                CodecProfile {
                    r#type: "Video",
                    codec: "hevc",
                    apply_conditions: vec![
                        ProfileCondition {
                            is_required: false,
                            property: "IsAnamorphic",
                            value: "true",
                            condition: "NotEquals",
                        },
                        ProfileCondition {
                            is_required: false,
                            property: "VideoProfile",
                            value: "high|main|main 10",
                            condition: "EqualsAny",
                        },
                        ProfileCondition {
                            is_required: false,
                            property: "VideoLevel",
                            value: "175",
                            condition: "LessThanEqual",
                        },
                        ProfileCondition {
                            is_required: false,
                            property: "IsInterlaced",
                            value: "true",
                            condition: "NotEquals",
                        },
                    ],
                },
            ],
            max_streaming_bitrate: LOCAL_DEVICE_BITRATE,
            subtitle_profiles: vec![
                SubtitleProfile {
                    format: "ass",
                    method: "Embed",
                },
                SubtitleProfile {
                    format: "ssa",
                    method: "Embed",
                },
                SubtitleProfile {
                    format: "subrip",
                    method: "Embed",
                },
                SubtitleProfile {
                    format: "sub",
                    method: "Embed",
                },
                SubtitleProfile {
                    format: "pgssub",
                    method: "Embed",
                },
                SubtitleProfile {
                    format: "subrip",
                    method: "External",
                },
                SubtitleProfile {
                    format: "sub",
                    method: "External",
                },
                SubtitleProfile {
                    format: "ass",
                    method: "External",
                },
                SubtitleProfile {
                    format: "ssa",
                    method: "External",
                },
                SubtitleProfile {
                    format: "vtt",
                    method: "External",
                },
                SubtitleProfile {
                    format: "ass",
                    method: "External",
                },
                SubtitleProfile {
                    format: "ssa",
                    method: "External",
                },
            ],
            response_profiles: vec![ResponseProfile {
                container: "m4v",
                r#type: "Video",
                mime_type: "video/mp4",
            }],
            music_streaming_transcoding_bitrate: LOCAL_DEVICE_BITRATE,
            max_static_bitrate: LOCAL_DEVICE_BITRATE,
            transcoding_profiles: vec![TranscodingProfile {
                audio_codec: "aac,mp3,wav,ac3,eac3,flac,opus",
                min_segments: 2,
                break_on_non_key_frames: true,
                protocol: "hls",
                video_codec: "hevc,h264,mpeg4",
                r#type: "Video",
                max_audio_channels: "6",
                container: "ts",
                context: "Streaming",
            }],
            direct_play_profiles: vec![DirectPlayProfile {
                container: "mov,mp4,mkv,webm",
                video_codec: "h264,hevc,dvhe,dvh1,h264,hevc,hev1,mpeg4,vp9",
                r#type: "Video",
                audio_codec: "aac,mp3,wav,ac3,eac3,flac,truehd,dts,dca,opus",
            }],
            container_profiles: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn serializes_local_emby_device_profile() {
        let value = serde_json::to_value(device_profile()).unwrap();

        assert_eq!(
            value,
            json!({
                "DeviceProfile": {
                    "CodecProfiles": [
                        {
                            "Type": "Video",
                            "Codec": "h264",
                            "ApplyConditions": [
                                {
                                    "IsRequired": false,
                                    "Property": "IsAnamorphic",
                                    "Value": "true",
                                    "Condition": "NotEquals"
                                },
                                {
                                    "IsRequired": false,
                                    "Property": "VideoProfile",
                                    "Value": "high|main|baseline|constrained baseline",
                                    "Condition": "EqualsAny"
                                },
                                {
                                    "IsRequired": false,
                                    "Property": "VideoLevel",
                                    "Value": "80",
                                    "Condition": "LessThanEqual"
                                },
                                {
                                    "IsRequired": false,
                                    "Property": "IsInterlaced",
                                    "Value": "true",
                                    "Condition": "NotEquals"
                                }
                            ]
                        },
                        {
                            "Type": "Video",
                            "Codec": "hevc",
                            "ApplyConditions": [
                                {
                                    "IsRequired": false,
                                    "Property": "IsAnamorphic",
                                    "Value": "true",
                                    "Condition": "NotEquals"
                                },
                                {
                                    "IsRequired": false,
                                    "Property": "VideoProfile",
                                    "Value": "high|main|main 10",
                                    "Condition": "EqualsAny"
                                },
                                {
                                    "IsRequired": false,
                                    "Property": "VideoLevel",
                                    "Value": "175",
                                    "Condition": "LessThanEqual"
                                },
                                {
                                    "IsRequired": false,
                                    "Property": "IsInterlaced",
                                    "Value": "true",
                                    "Condition": "NotEquals"
                                }
                            ]
                        }
                    ],
                    "MaxStreamingBitrate": 40000000,
                    "SubtitleProfiles": [
                        {
                            "Format": "ass",
                            "Method": "Embed"
                        },
                        {
                            "Format": "ssa",
                            "Method": "Embed"
                        },
                        {
                            "Format": "subrip",
                            "Method": "Embed"
                        },
                        {
                            "Format": "sub",
                            "Method": "Embed"
                        },
                        {
                            "Format": "pgssub",
                            "Method": "Embed"
                        },
                        {
                            "Format": "subrip",
                            "Method": "External"
                        },
                        {
                            "Format": "sub",
                            "Method": "External"
                        },
                        {
                            "Format": "ass",
                            "Method": "External"
                        },
                        {
                            "Format": "ssa",
                            "Method": "External"
                        },
                        {
                            "Format": "vtt",
                            "Method": "External"
                        },
                        {
                            "Format": "ass",
                            "Method": "External"
                        },
                        {
                            "Format": "ssa",
                            "Method": "External"
                        }
                    ],
                    "ResponseProfiles": [
                        {
                            "Container": "m4v",
                            "Type": "Video",
                            "MimeType": "video/mp4"
                        }
                    ],
                    "MusicStreamingTranscodingBitrate": 40000000,
                    "MaxStaticBitrate": 40000000,
                    "TranscodingProfiles": [
                        {
                            "AudioCodec": "aac,mp3,wav,ac3,eac3,flac,opus",
                            "MinSegments": 2,
                            "BreakOnNonKeyFrames": true,
                            "Protocol": "hls",
                            "VideoCodec": "hevc,h264,mpeg4",
                            "Type": "Video",
                            "MaxAudioChannels": "6",
                            "Container": "ts",
                            "Context": "Streaming"
                        }
                    ],
                    "DirectPlayProfiles": [
                        {
                            "Container": "mov,mp4,mkv,webm",
                            "VideoCodec": "h264,hevc,dvhe,dvh1,h264,hevc,hev1,mpeg4,vp9",
                            "Type": "Video",
                            "AudioCodec": "aac,mp3,wav,ac3,eac3,flac,truehd,dts,dca,opus"
                        }
                    ],
                    "ContainerProfiles": []
                }
            }),
        );
    }
}
