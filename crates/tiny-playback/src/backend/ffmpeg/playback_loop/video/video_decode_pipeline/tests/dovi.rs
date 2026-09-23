use super::*;

#[test]
fn rpu_only_packet_uses_original_decode_packet() {
    assert_eq!(
        hevc_dovi_decode_action_for_inspection(&dovi_inspection(0, None)),
        StrippedHevcDoviDecodeAction::PassthroughMetadataOnly
    );
    assert_eq!(
        hevc_dovi_decode_action_for_inspection(&dovi_inspection(0, Some(dovi_metadata()))),
        StrippedHevcDoviDecodeAction::PassthroughMetadataOnly
    );
}

#[test]
fn mixed_dovi_packet_keeps_decode_action() {
    assert_eq!(
        hevc_dovi_decode_action_for_inspection(&dovi_inspection(1, None)),
        StrippedHevcDoviDecodeAction::DecodeStripped
    );
}
