use std::mem::size_of;

use crate::windows_utils::streamer::intel::intel_sys::*;

#[test]
fn struct_sizes_match_onevpl_abi() {
    assert_eq!(size_of::<mfxFrameId>(), 8, "mfxFrameId");
    assert_eq!(size_of::<mfxFrameInfo>(), 68, "mfxFrameInfo");
    assert_eq!(size_of::<mfxInfoMFX>(), 136, "mfxInfoMFX");
    assert_eq!(size_of::<mfxInfoVPP>(), 168, "mfxInfoVPP");
    assert_eq!(size_of::<mfxInfoUnion>(), 168, "mfxInfoUnion == sizeof(mfxInfoVPP)");
    assert_eq!(size_of::<mfxVideoParam>(), 208, "mfxVideoParam");
    assert_eq!(size_of::<mfxBitstream>(), 72, "mfxBitstream");
    assert_eq!(size_of::<mfxEncodeCtrl>(), 48, "mfxEncodeCtrl");
    assert_eq!(size_of::<mfxVariant>(), 16, "mfxVariant");
    assert_eq!(size_of::<mfxExtCodingOption>(), 64, "mfxExtCodingOption");
    assert_eq!(size_of::<mfxExtCodingOption2>(), 68, "mfxExtCodingOption2");
    assert_eq!(size_of::<mfxExtCodingOption3>(), 512, "mfxExtCodingOption3");
    assert_eq!(size_of::<mfxExtBuffer>(), 8, "mfxExtBuffer");
}

#[test]
fn key_field_offsets_match_onevpl_abi() {
    use std::mem::offset_of;

    assert_eq!(offset_of!(mfxFrameInfo, FourCC), 32, "mfxFrameInfo.FourCC");
    assert_eq!(offset_of!(mfxFrameInfo, Width), 36, "mfxFrameInfo.Width");
    assert_eq!(offset_of!(mfxFrameInfo, FrameRateExtN), 48, "mfxFrameInfo.FrameRateExtN");

    assert_eq!(offset_of!(mfxInfoMFX, FrameInfo), 32, "mfxInfoMFX.FrameInfo");
    assert_eq!(offset_of!(mfxInfoMFX, InitialDelayInKB), 122, "mfxInfoMFX.InitialDelayInKB (==QPI)");
    assert_eq!(offset_of!(mfxInfoMFX, TargetKbps), 126, "mfxInfoMFX.TargetKbps (==QPP)");
    assert_eq!(offset_of!(mfxInfoMFX, MaxKbps), 128, "mfxInfoMFX.MaxKbps (==QPB)");

    assert_eq!(offset_of!(mfxVideoParam, AsyncDepth), 14, "mfxVideoParam.AsyncDepth");
    assert_eq!(offset_of!(mfxVideoParam, IOPattern), 186, "mfxVideoParam.IOPattern");
    assert_eq!(offset_of!(mfxVideoParam, ExtParam), 192, "mfxVideoParam.ExtParam");

    assert_eq!(offset_of!(mfxBitstream, Data), 40, "mfxBitstream.Data");
    assert_eq!(offset_of!(mfxBitstream, DataLength), 52, "mfxBitstream.DataLength");

    assert_eq!(offset_of!(mfxExtCodingOption2, IntRefType), 8, "CO2.IntRefType");
    assert_eq!(offset_of!(mfxExtCodingOption3, WinBRCSize), 16, "CO3.WinBRCSize");
    assert_eq!(offset_of!(mfxExtCodingOption3, ScenarioInfo), 54, "CO3.ScenarioInfo");
    assert_eq!(offset_of!(mfxExtCodingOption3, LowDelayBRC), 166, "CO3.LowDelayBRC");
}
