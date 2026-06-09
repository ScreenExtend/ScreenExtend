#![allow(non_snake_case, non_camel_case_types, dead_code)]

use crate::windows_utils::streamer::nvidia::nvenc_sys::nvEncodeAPI::*;

#[test]
fn bindgen_test_layout__GUID() {
    const UNINIT: ::core::mem::MaybeUninit<_GUID> = ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_GUID>(),
        16usize,
        concat!("Size of: ", stringify!(_GUID))
    );
    assert_eq!(
        ::core::mem::align_of::<_GUID>(),
        4usize,
        concat!("Alignment of ", stringify!(_GUID))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).Data1) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_GUID),
            "::",
            stringify!(Data1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).Data2) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_GUID),
            "::",
            stringify!(Data2)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).Data3) as usize - ptr as usize },
        6usize,
        concat!(
            "Offset of field: ",
            stringify!(_GUID),
            "::",
            stringify!(Data3)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).Data4) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_GUID),
            "::",
            stringify!(Data4)
        )
    );
}
#[test]
fn bindgen_test_layout_tagRECT() {
    const UNINIT: ::core::mem::MaybeUninit<tagRECT> = ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<tagRECT>(),
        16usize,
        concat!("Size of: ", stringify!(tagRECT))
    );
    assert_eq!(
        ::core::mem::align_of::<tagRECT>(),
        4usize,
        concat!("Alignment of ", stringify!(tagRECT))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).left) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(tagRECT),
            "::",
            stringify!(left)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).top) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(tagRECT),
            "::",
            stringify!(top)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).right) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(tagRECT),
            "::",
            stringify!(right)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bottom) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(tagRECT),
            "::",
            stringify!(bottom)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CAPS_PARAM() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CAPS_PARAM> = ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CAPS_PARAM>(),
        256usize,
        concat!("Size of: ", stringify!(_NV_ENC_CAPS_PARAM))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CAPS_PARAM>(),
        4usize,
        concat!("Alignment of ", stringify!(_NV_ENC_CAPS_PARAM))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CAPS_PARAM),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).capsToQuery) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CAPS_PARAM),
            "::",
            stringify!(capsToQuery)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CAPS_PARAM),
            "::",
            stringify!(reserved)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_RESTORE_ENCODER_STATE_PARAMS() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_RESTORE_ENCODER_STATE_PARAMS> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_RESTORE_ENCODER_STATE_PARAMS>(),
        800usize,
        concat!(
            "Size of: ",
            stringify!(_NV_ENC_RESTORE_ENCODER_STATE_PARAMS)
        )
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_RESTORE_ENCODER_STATE_PARAMS>(),
        8usize,
        concat!(
            "Alignment of ",
            stringify!(_NV_ENC_RESTORE_ENCODER_STATE_PARAMS)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RESTORE_ENCODER_STATE_PARAMS),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bufferIdx) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RESTORE_ENCODER_STATE_PARAMS),
            "::",
            stringify!(bufferIdx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).state) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RESTORE_ENCODER_STATE_PARAMS),
            "::",
            stringify!(state)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).outputBitstream) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RESTORE_ENCODER_STATE_PARAMS),
            "::",
            stringify!(outputBitstream)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).completionEvent) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RESTORE_ENCODER_STATE_PARAMS),
            "::",
            stringify!(completionEvent)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RESTORE_ENCODER_STATE_PARAMS),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        288usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RESTORE_ENCODER_STATE_PARAMS),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_OUTPUT_STATS_BLOCK() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_OUTPUT_STATS_BLOCK> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_OUTPUT_STATS_BLOCK>(),
        64usize,
        concat!("Size of: ", stringify!(_NV_ENC_OUTPUT_STATS_BLOCK))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_OUTPUT_STATS_BLOCK>(),
        4usize,
        concat!("Alignment of ", stringify!(_NV_ENC_OUTPUT_STATS_BLOCK))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_STATS_BLOCK),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).QP) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_STATS_BLOCK),
            "::",
            stringify!(QP)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        5usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_STATS_BLOCK),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bitcount) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_STATS_BLOCK),
            "::",
            stringify!(bitcount)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_STATS_BLOCK),
            "::",
            stringify!(reserved1)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_OUTPUT_STATS_ROW() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_OUTPUT_STATS_ROW> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_OUTPUT_STATS_ROW>(),
        64usize,
        concat!("Size of: ", stringify!(_NV_ENC_OUTPUT_STATS_ROW))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_OUTPUT_STATS_ROW>(),
        4usize,
        concat!("Alignment of ", stringify!(_NV_ENC_OUTPUT_STATS_ROW))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_STATS_ROW),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).QP) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_STATS_ROW),
            "::",
            stringify!(QP)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        5usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_STATS_ROW),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bitcount) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_STATS_ROW),
            "::",
            stringify!(bitcount)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_STATS_ROW),
            "::",
            stringify!(reserved1)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_ENCODE_OUT_PARAMS() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_ENCODE_OUT_PARAMS> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_ENCODE_OUT_PARAMS>(),
        256usize,
        concat!("Size of: ", stringify!(_NV_ENC_ENCODE_OUT_PARAMS))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_ENCODE_OUT_PARAMS>(),
        4usize,
        concat!("Alignment of ", stringify!(_NV_ENC_ENCODE_OUT_PARAMS))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_ENCODE_OUT_PARAMS),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bitstreamSizeInBytes) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_ENCODE_OUT_PARAMS),
            "::",
            stringify!(bitstreamSizeInBytes)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_ENCODE_OUT_PARAMS),
            "::",
            stringify!(reserved)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_LOOKAHEAD_PIC_PARAMS() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_LOOKAHEAD_PIC_PARAMS> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_LOOKAHEAD_PIC_PARAMS>(),
        792usize,
        concat!("Size of: ", stringify!(_NV_ENC_LOOKAHEAD_PIC_PARAMS))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_LOOKAHEAD_PIC_PARAMS>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_LOOKAHEAD_PIC_PARAMS))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOOKAHEAD_PIC_PARAMS),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputBuffer) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOOKAHEAD_PIC_PARAMS),
            "::",
            stringify!(inputBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pictureType) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOOKAHEAD_PIC_PARAMS),
            "::",
            stringify!(pictureType)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOOKAHEAD_PIC_PARAMS),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        280usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOOKAHEAD_PIC_PARAMS),
            "::",
            stringify!(reserved1)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CREATE_INPUT_BUFFER() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CREATE_INPUT_BUFFER> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CREATE_INPUT_BUFFER>(),
        776usize,
        concat!("Size of: ", stringify!(_NV_ENC_CREATE_INPUT_BUFFER))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CREATE_INPUT_BUFFER>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_CREATE_INPUT_BUFFER))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_INPUT_BUFFER),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).width) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_INPUT_BUFFER),
            "::",
            stringify!(width)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).height) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_INPUT_BUFFER),
            "::",
            stringify!(height)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).memoryHeap) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_INPUT_BUFFER),
            "::",
            stringify!(memoryHeap)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bufferFmt) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_INPUT_BUFFER),
            "::",
            stringify!(bufferFmt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_INPUT_BUFFER),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputBuffer) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_INPUT_BUFFER),
            "::",
            stringify!(inputBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pSysMemBuffer) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_INPUT_BUFFER),
            "::",
            stringify!(pSysMemBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_INPUT_BUFFER),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        272usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_INPUT_BUFFER),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CREATE_BITSTREAM_BUFFER() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CREATE_BITSTREAM_BUFFER> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CREATE_BITSTREAM_BUFFER>(),
        776usize,
        concat!("Size of: ", stringify!(_NV_ENC_CREATE_BITSTREAM_BUFFER))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CREATE_BITSTREAM_BUFFER>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_CREATE_BITSTREAM_BUFFER))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_BITSTREAM_BUFFER),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).size) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_BITSTREAM_BUFFER),
            "::",
            stringify!(size)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).memoryHeap) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_BITSTREAM_BUFFER),
            "::",
            stringify!(memoryHeap)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_BITSTREAM_BUFFER),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bitstreamBuffer) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_BITSTREAM_BUFFER),
            "::",
            stringify!(bitstreamBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bitstreamBufferPtr) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_BITSTREAM_BUFFER),
            "::",
            stringify!(bitstreamBufferPtr)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_BITSTREAM_BUFFER),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        264usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_BITSTREAM_BUFFER),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_MVECTOR() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_MVECTOR> = ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_MVECTOR>(),
        4usize,
        concat!("Size of: ", stringify!(_NV_ENC_MVECTOR))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_MVECTOR>(),
        2usize,
        concat!("Alignment of ", stringify!(_NV_ENC_MVECTOR))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).mvx) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MVECTOR),
            "::",
            stringify!(mvx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).mvy) as usize - ptr as usize },
        2usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MVECTOR),
            "::",
            stringify!(mvy)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_H264_MV_DATA() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_H264_MV_DATA> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_H264_MV_DATA>(),
        24usize,
        concat!("Size of: ", stringify!(_NV_ENC_H264_MV_DATA))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_H264_MV_DATA>(),
        4usize,
        concat!("Alignment of ", stringify!(_NV_ENC_H264_MV_DATA))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).mv) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_H264_MV_DATA),
            "::",
            stringify!(mv)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).mbType) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_H264_MV_DATA),
            "::",
            stringify!(mbType)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).partitionType) as usize - ptr as usize },
        17usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_H264_MV_DATA),
            "::",
            stringify!(partitionType)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        18usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_H264_MV_DATA),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).mbCost) as usize - ptr as usize },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_H264_MV_DATA),
            "::",
            stringify!(mbCost)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_HEVC_MV_DATA() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_HEVC_MV_DATA> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_HEVC_MV_DATA>(),
        20usize,
        concat!("Size of: ", stringify!(_NV_ENC_HEVC_MV_DATA))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_HEVC_MV_DATA>(),
        2usize,
        concat!("Alignment of ", stringify!(_NV_ENC_HEVC_MV_DATA))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).mv) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_HEVC_MV_DATA),
            "::",
            stringify!(mv)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).cuType) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_HEVC_MV_DATA),
            "::",
            stringify!(cuType)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).cuSize) as usize - ptr as usize },
        17usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_HEVC_MV_DATA),
            "::",
            stringify!(cuSize)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).partitionMode) as usize - ptr as usize },
        18usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_HEVC_MV_DATA),
            "::",
            stringify!(partitionMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).lastCUInCTB) as usize - ptr as usize },
        19usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_HEVC_MV_DATA),
            "::",
            stringify!(lastCUInCTB)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CREATE_MV_BUFFER() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CREATE_MV_BUFFER> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CREATE_MV_BUFFER>(),
        1544usize,
        concat!("Size of: ", stringify!(_NV_ENC_CREATE_MV_BUFFER))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CREATE_MV_BUFFER>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_CREATE_MV_BUFFER))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_MV_BUFFER),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).mvBuffer) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_MV_BUFFER),
            "::",
            stringify!(mvBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_MV_BUFFER),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1040usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CREATE_MV_BUFFER),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_QP() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_QP> = ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_QP>(),
        12usize,
        concat!("Size of: ", stringify!(_NV_ENC_QP))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_QP>(),
        4usize,
        concat!("Alignment of ", stringify!(_NV_ENC_QP))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).qpInterP) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_QP),
            "::",
            stringify!(qpInterP)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).qpInterB) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_QP),
            "::",
            stringify!(qpInterB)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).qpIntra) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_QP),
            "::",
            stringify!(qpIntra)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_RC_PARAMS() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_RC_PARAMS> = ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_RC_PARAMS>(),
        128usize,
        concat!("Size of: ", stringify!(_NV_ENC_RC_PARAMS))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_RC_PARAMS>(),
        4usize,
        concat!("Alignment of ", stringify!(_NV_ENC_RC_PARAMS))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).rateControlMode) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(rateControlMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).constQP) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(constQP)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).averageBitRate) as usize - ptr as usize },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(averageBitRate)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).maxBitRate) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(maxBitRate)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).vbvBufferSize) as usize - ptr as usize },
        28usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(vbvBufferSize)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).vbvInitialDelay) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(vbvInitialDelay)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).minQP) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(minQP)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).maxQP) as usize - ptr as usize },
        52usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(maxQP)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).initialRCQP) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(initialRCQP)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).temporallayerIdxMask) as usize - ptr as usize },
        76usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(temporallayerIdxMask)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).temporalLayerQP) as usize - ptr as usize },
        80usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(temporalLayerQP)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).targetQuality) as usize - ptr as usize },
        88usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(targetQuality)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).targetQualityLSB) as usize - ptr as usize },
        89usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(targetQualityLSB)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).lookaheadDepth) as usize - ptr as usize },
        90usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(lookaheadDepth)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).lowDelayKeyFrameScale) as usize - ptr as usize },
        92usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(lowDelayKeyFrameScale)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).yDcQPIndexOffset) as usize - ptr as usize },
        93usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(yDcQPIndexOffset)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).uDcQPIndexOffset) as usize - ptr as usize },
        94usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(uDcQPIndexOffset)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).vDcQPIndexOffset) as usize - ptr as usize },
        95usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(vDcQPIndexOffset)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).qpMapMode) as usize - ptr as usize },
        96usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(qpMapMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).multiPass) as usize - ptr as usize },
        100usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(multiPass)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).alphaLayerBitrateRatio) as usize - ptr as usize },
        104usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(alphaLayerBitrateRatio)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).cbQPIndexOffset) as usize - ptr as usize },
        108usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(cbQPIndexOffset)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).crQPIndexOffset) as usize - ptr as usize },
        109usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(crQPIndexOffset)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        110usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(reserved2)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        112usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RC_PARAMS),
            "::",
            stringify!(reserved)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CLOCK_TIMESTAMP_SET() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CLOCK_TIMESTAMP_SET> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CLOCK_TIMESTAMP_SET>(),
        8usize,
        concat!("Size of: ", stringify!(_NV_ENC_CLOCK_TIMESTAMP_SET))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CLOCK_TIMESTAMP_SET>(),
        4usize,
        concat!("Alignment of ", stringify!(_NV_ENC_CLOCK_TIMESTAMP_SET))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).timeOffset) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CLOCK_TIMESTAMP_SET),
            "::",
            stringify!(timeOffset)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_TIME_CODE() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_TIME_CODE> = ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_TIME_CODE>(),
        28usize,
        concat!("Size of: ", stringify!(_NV_ENC_TIME_CODE))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_TIME_CODE>(),
        4usize,
        concat!("Alignment of ", stringify!(_NV_ENC_TIME_CODE))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).displayPicStruct) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_TIME_CODE),
            "::",
            stringify!(displayPicStruct)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).clockTimestamp) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_TIME_CODE),
            "::",
            stringify!(clockTimestamp)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CONFIG_H264_VUI_PARAMETERS() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CONFIG_H264_VUI_PARAMETERS> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CONFIG_H264_VUI_PARAMETERS>(),
        112usize,
        concat!("Size of: ", stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CONFIG_H264_VUI_PARAMETERS>(),
        4usize,
        concat!(
            "Alignment of ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).overscanInfoPresentFlag) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(overscanInfoPresentFlag)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).overscanInfo) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(overscanInfo)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).videoSignalTypePresentFlag) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(videoSignalTypePresentFlag)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).videoFormat) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(videoFormat)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).videoFullRangeFlag) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(videoFullRangeFlag)
        )
    );
    assert_eq!(
        unsafe {
            ::core::ptr::addr_of!((*ptr).colourDescriptionPresentFlag) as usize - ptr as usize
        },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(colourDescriptionPresentFlag)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).colourPrimaries) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(colourPrimaries)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).transferCharacteristics) as usize - ptr as usize },
        28usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(transferCharacteristics)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).colourMatrix) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(colourMatrix)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).chromaSampleLocationFlag) as usize - ptr as usize },
        36usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(chromaSampleLocationFlag)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).chromaSampleLocationTop) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(chromaSampleLocationTop)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).chromaSampleLocationBot) as usize - ptr as usize },
        44usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(chromaSampleLocationBot)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bitstreamRestrictionFlag) as usize - ptr as usize },
        48usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(bitstreamRestrictionFlag)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).timingInfoPresentFlag) as usize - ptr as usize },
        52usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(timingInfoPresentFlag)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numUnitInTicks) as usize - ptr as usize },
        56usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(numUnitInTicks)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).timeScale) as usize - ptr as usize },
        60usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(timeScale)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_VUI_PARAMETERS),
            "::",
            stringify!(reserved)
        )
    );
}
#[test]
fn bindgen_test_layout__NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE() {
    const UNINIT: ::core::mem::MaybeUninit<_NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE>(),
        16usize,
        concat!(
            "Size of: ",
            stringify!(_NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE)
        )
    );
    assert_eq!(
        ::core::mem::align_of::<_NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE>(),
        4usize,
        concat!(
            "Alignment of ",
            stringify!(_NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE),
            "::",
            stringify!(reserved1)
        )
    );
}
#[test]
fn bindgen_test_layout__NVENC_EXTERNAL_ME_HINT() {
    assert_eq!(
        ::core::mem::size_of::<_NVENC_EXTERNAL_ME_HINT>(),
        4usize,
        concat!("Size of: ", stringify!(_NVENC_EXTERNAL_ME_HINT))
    );
    assert_eq!(
        ::core::mem::align_of::<_NVENC_EXTERNAL_ME_HINT>(),
        4usize,
        concat!("Alignment of ", stringify!(_NVENC_EXTERNAL_ME_HINT))
    );
}
#[test]
fn bindgen_test_layout__NVENC_EXTERNAL_ME_SB_HINT() {
    assert_eq!(
        ::core::mem::size_of::<_NVENC_EXTERNAL_ME_SB_HINT>(),
        6usize,
        concat!("Size of: ", stringify!(_NVENC_EXTERNAL_ME_SB_HINT))
    );
    assert_eq!(
        ::core::mem::align_of::<_NVENC_EXTERNAL_ME_SB_HINT>(),
        2usize,
        concat!("Alignment of ", stringify!(_NVENC_EXTERNAL_ME_SB_HINT))
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CONFIG_H264() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CONFIG_H264> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CONFIG_H264>(),
        1792usize,
        concat!("Size of: ", stringify!(_NV_ENC_CONFIG_H264))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CONFIG_H264>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_CONFIG_H264))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).level) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(level)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).idrPeriod) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(idrPeriod)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).separateColourPlaneFlag) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(separateColourPlaneFlag)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).disableDeblockingFilterIDC) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(disableDeblockingFilterIDC)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numTemporalLayers) as usize - ptr as usize },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(numTemporalLayers)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).spsId) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(spsId)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ppsId) as usize - ptr as usize },
        28usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(ppsId)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).adaptiveTransformMode) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(adaptiveTransformMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).fmoMode) as usize - ptr as usize },
        36usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(fmoMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bdirectMode) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(bdirectMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).entropyCodingMode) as usize - ptr as usize },
        44usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(entropyCodingMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).stereoMode) as usize - ptr as usize },
        48usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(stereoMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).intraRefreshPeriod) as usize - ptr as usize },
        52usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(intraRefreshPeriod)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).intraRefreshCnt) as usize - ptr as usize },
        56usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(intraRefreshCnt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).maxNumRefFrames) as usize - ptr as usize },
        60usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(maxNumRefFrames)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceMode) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(sliceMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceModeData) as usize - ptr as usize },
        68usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(sliceModeData)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).h264VUIParameters) as usize - ptr as usize },
        72usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(h264VUIParameters)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ltrNumFrames) as usize - ptr as usize },
        184usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(ltrNumFrames)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ltrTrustMode) as usize - ptr as usize },
        188usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(ltrTrustMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).chromaFormatIDC) as usize - ptr as usize },
        192usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(chromaFormatIDC)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).maxTemporalLayers) as usize - ptr as usize },
        196usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(maxTemporalLayers)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).useBFramesAsRef) as usize - ptr as usize },
        200usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(useBFramesAsRef)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numRefL0) as usize - ptr as usize },
        204usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(numRefL0)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numRefL1) as usize - ptr as usize },
        208usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(numRefL1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        212usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1280usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CONFIG_HEVC() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CONFIG_HEVC> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CONFIG_HEVC>(),
        1560usize,
        concat!("Size of: ", stringify!(_NV_ENC_CONFIG_HEVC))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CONFIG_HEVC>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_CONFIG_HEVC))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).level) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(level)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).tier) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(tier)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).minCUSize) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(minCUSize)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).maxCUSize) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(maxCUSize)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).idrPeriod) as usize - ptr as usize },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(idrPeriod)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).intraRefreshPeriod) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(intraRefreshPeriod)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).intraRefreshCnt) as usize - ptr as usize },
        28usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(intraRefreshCnt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).maxNumRefFramesInDPB) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(maxNumRefFramesInDPB)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ltrNumFrames) as usize - ptr as usize },
        36usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(ltrNumFrames)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).vpsId) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(vpsId)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).spsId) as usize - ptr as usize },
        44usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(spsId)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ppsId) as usize - ptr as usize },
        48usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(ppsId)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceMode) as usize - ptr as usize },
        52usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(sliceMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceModeData) as usize - ptr as usize },
        56usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(sliceModeData)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).maxTemporalLayersMinus1) as usize - ptr as usize },
        60usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(maxTemporalLayersMinus1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).hevcVUIParameters) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(hevcVUIParameters)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ltrTrustMode) as usize - ptr as usize },
        176usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(ltrTrustMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).useBFramesAsRef) as usize - ptr as usize },
        180usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(useBFramesAsRef)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numRefL0) as usize - ptr as usize },
        184usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(numRefL0)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numRefL1) as usize - ptr as usize },
        188usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(numRefL1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        192usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1048usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_FILM_GRAIN_PARAMS_AV1() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_FILM_GRAIN_PARAMS_AV1> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_FILM_GRAIN_PARAMS_AV1>(),
        156usize,
        concat!("Size of: ", stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_FILM_GRAIN_PARAMS_AV1>(),
        4usize,
        concat!("Alignment of ", stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pointYValue) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(pointYValue)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pointYScaling) as usize - ptr as usize },
        18usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(pointYScaling)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pointCbValue) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(pointCbValue)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pointCbScaling) as usize - ptr as usize },
        42usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(pointCbScaling)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pointCrValue) as usize - ptr as usize },
        52usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(pointCrValue)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pointCrScaling) as usize - ptr as usize },
        62usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(pointCrScaling)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).arCoeffsYPlus128) as usize - ptr as usize },
        72usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(arCoeffsYPlus128)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).arCoeffsCbPlus128) as usize - ptr as usize },
        96usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(arCoeffsCbPlus128)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).arCoeffsCrPlus128) as usize - ptr as usize },
        121usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(arCoeffsCrPlus128)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        146usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(reserved2)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).cbMult) as usize - ptr as usize },
        148usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(cbMult)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).cbLumaMult) as usize - ptr as usize },
        149usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(cbLumaMult)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).cbOffset) as usize - ptr as usize },
        150usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(cbOffset)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).crMult) as usize - ptr as usize },
        152usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(crMult)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).crLumaMult) as usize - ptr as usize },
        153usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(crLumaMult)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).crOffset) as usize - ptr as usize },
        154usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FILM_GRAIN_PARAMS_AV1),
            "::",
            stringify!(crOffset)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CONFIG_AV1() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CONFIG_AV1> = ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CONFIG_AV1>(),
        1552usize,
        concat!("Size of: ", stringify!(_NV_ENC_CONFIG_AV1))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CONFIG_AV1>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_CONFIG_AV1))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).level) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(level)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).tier) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(tier)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).minPartSize) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(minPartSize)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).maxPartSize) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(maxPartSize)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).idrPeriod) as usize - ptr as usize },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(idrPeriod)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).intraRefreshPeriod) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(intraRefreshPeriod)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).intraRefreshCnt) as usize - ptr as usize },
        28usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(intraRefreshCnt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).maxNumRefFramesInDPB) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(maxNumRefFramesInDPB)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numTileColumns) as usize - ptr as usize },
        36usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(numTileColumns)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numTileRows) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(numTileRows)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).tileWidths) as usize - ptr as usize },
        48usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(tileWidths)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).tileHeights) as usize - ptr as usize },
        56usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(tileHeights)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).maxTemporalLayersMinus1) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(maxTemporalLayersMinus1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).colorPrimaries) as usize - ptr as usize },
        68usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(colorPrimaries)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).transferCharacteristics) as usize - ptr as usize },
        72usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(transferCharacteristics)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).matrixCoefficients) as usize - ptr as usize },
        76usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(matrixCoefficients)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).colorRange) as usize - ptr as usize },
        80usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(colorRange)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).chromaSamplePosition) as usize - ptr as usize },
        84usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(chromaSamplePosition)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).useBFramesAsRef) as usize - ptr as usize },
        88usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(useBFramesAsRef)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).filmGrainParams) as usize - ptr as usize },
        96usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(filmGrainParams)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numFwdRefs) as usize - ptr as usize },
        104usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(numFwdRefs)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numBwdRefs) as usize - ptr as usize },
        108usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(numBwdRefs)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        112usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1056usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_AV1),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CONFIG_H264_MEONLY() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CONFIG_H264_MEONLY> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CONFIG_H264_MEONLY>(),
        1536usize,
        concat!("Size of: ", stringify!(_NV_ENC_CONFIG_H264_MEONLY))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CONFIG_H264_MEONLY>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_CONFIG_H264_MEONLY))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_MEONLY),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1024usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_H264_MEONLY),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CONFIG_HEVC_MEONLY() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CONFIG_HEVC_MEONLY> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CONFIG_HEVC_MEONLY>(),
        1536usize,
        concat!("Size of: ", stringify!(_NV_ENC_CONFIG_HEVC_MEONLY))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CONFIG_HEVC_MEONLY>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_CONFIG_HEVC_MEONLY))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC_MEONLY),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        1024usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG_HEVC_MEONLY),
            "::",
            stringify!(reserved1)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CODEC_CONFIG() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CODEC_CONFIG> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CODEC_CONFIG>(),
        1792usize,
        concat!("Size of: ", stringify!(_NV_ENC_CODEC_CONFIG))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CODEC_CONFIG>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_CODEC_CONFIG))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).h264Config) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CODEC_CONFIG),
            "::",
            stringify!(h264Config)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).hevcConfig) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CODEC_CONFIG),
            "::",
            stringify!(hevcConfig)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).av1Config) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CODEC_CONFIG),
            "::",
            stringify!(av1Config)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).h264MeOnlyConfig) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CODEC_CONFIG),
            "::",
            stringify!(h264MeOnlyConfig)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).hevcMeOnlyConfig) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CODEC_CONFIG),
            "::",
            stringify!(hevcMeOnlyConfig)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CODEC_CONFIG),
            "::",
            stringify!(reserved)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CONFIG() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CONFIG> = ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CONFIG>(),
        3584usize,
        concat!("Size of: ", stringify!(_NV_ENC_CONFIG))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CONFIG>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_CONFIG))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).profileGUID) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG),
            "::",
            stringify!(profileGUID)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).gopLength) as usize - ptr as usize },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG),
            "::",
            stringify!(gopLength)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).frameIntervalP) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG),
            "::",
            stringify!(frameIntervalP)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).monoChromeEncoding) as usize - ptr as usize },
        28usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG),
            "::",
            stringify!(monoChromeEncoding)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).frameFieldMode) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG),
            "::",
            stringify!(frameFieldMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).mvPrecision) as usize - ptr as usize },
        36usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG),
            "::",
            stringify!(mvPrecision)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).rcParams) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG),
            "::",
            stringify!(rcParams)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).encodeCodecConfig) as usize - ptr as usize },
        168usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG),
            "::",
            stringify!(encodeCodecConfig)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        1960usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        3072usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CONFIG),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_INITIALIZE_PARAMS() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_INITIALIZE_PARAMS> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_INITIALIZE_PARAMS>(),
        1808usize,
        concat!("Size of: ", stringify!(_NV_ENC_INITIALIZE_PARAMS))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_INITIALIZE_PARAMS>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_INITIALIZE_PARAMS))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).encodeGUID) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(encodeGUID)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).presetGUID) as usize - ptr as usize },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(presetGUID)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).encodeWidth) as usize - ptr as usize },
        36usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(encodeWidth)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).encodeHeight) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(encodeHeight)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).darWidth) as usize - ptr as usize },
        44usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(darWidth)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).darHeight) as usize - ptr as usize },
        48usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(darHeight)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).frameRateNum) as usize - ptr as usize },
        52usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(frameRateNum)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).frameRateDen) as usize - ptr as usize },
        56usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(frameRateDen)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).enableEncodeAsync) as usize - ptr as usize },
        60usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(enableEncodeAsync)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).enablePTD) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(enablePTD)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).privDataSize) as usize - ptr as usize },
        72usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(privDataSize)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).privData) as usize - ptr as usize },
        80usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(privData)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).encodeConfig) as usize - ptr as usize },
        88usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(encodeConfig)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).maxEncodeWidth) as usize - ptr as usize },
        96usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(maxEncodeWidth)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).maxEncodeHeight) as usize - ptr as usize },
        100usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(maxEncodeHeight)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).maxMEHintCountsPerBlock) as usize - ptr as usize },
        104usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(maxMEHintCountsPerBlock)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).tuningInfo) as usize - ptr as usize },
        136usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(tuningInfo)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bufferFormat) as usize - ptr as usize },
        140usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(bufferFormat)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numStateBuffers) as usize - ptr as usize },
        144usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(numStateBuffers)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).outputStatsLevel) as usize - ptr as usize },
        148usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(outputStatsLevel)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        152usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1296usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INITIALIZE_PARAMS),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_RECONFIGURE_PARAMS() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_RECONFIGURE_PARAMS> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_RECONFIGURE_PARAMS>(),
        1824usize,
        concat!("Size of: ", stringify!(_NV_ENC_RECONFIGURE_PARAMS))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_RECONFIGURE_PARAMS>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_RECONFIGURE_PARAMS))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RECONFIGURE_PARAMS),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reInitEncodeParams) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_RECONFIGURE_PARAMS),
            "::",
            stringify!(reInitEncodeParams)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_PRESET_CONFIG() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_PRESET_CONFIG> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_PRESET_CONFIG>(),
        5128usize,
        concat!("Size of: ", stringify!(_NV_ENC_PRESET_CONFIG))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_PRESET_CONFIG>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_PRESET_CONFIG))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PRESET_CONFIG),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).presetCfg) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PRESET_CONFIG),
            "::",
            stringify!(presetCfg)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        3592usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PRESET_CONFIG),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        4616usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PRESET_CONFIG),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_PIC_PARAMS_MVC() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_PIC_PARAMS_MVC> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_PIC_PARAMS_MVC>(),
        128usize,
        concat!("Size of: ", stringify!(_NV_ENC_PIC_PARAMS_MVC))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_PIC_PARAMS_MVC>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_PIC_PARAMS_MVC))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_MVC),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).viewID) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_MVC),
            "::",
            stringify!(viewID)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).temporalID) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_MVC),
            "::",
            stringify!(temporalID)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).priorityID) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_MVC),
            "::",
            stringify!(priorityID)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_MVC),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_MVC),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_PIC_PARAMS_H264_EXT() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_PIC_PARAMS_H264_EXT> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_PIC_PARAMS_H264_EXT>(),
        128usize,
        concat!("Size of: ", stringify!(_NV_ENC_PIC_PARAMS_H264_EXT))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_PIC_PARAMS_H264_EXT>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_PIC_PARAMS_H264_EXT))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).mvcPicParams) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264_EXT),
            "::",
            stringify!(mvcPicParams)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264_EXT),
            "::",
            stringify!(reserved1)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_SEI_PAYLOAD() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_SEI_PAYLOAD> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_SEI_PAYLOAD>(),
        16usize,
        concat!("Size of: ", stringify!(_NV_ENC_SEI_PAYLOAD))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_SEI_PAYLOAD>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_SEI_PAYLOAD))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).payloadSize) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_SEI_PAYLOAD),
            "::",
            stringify!(payloadSize)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).payloadType) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_SEI_PAYLOAD),
            "::",
            stringify!(payloadType)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).payload) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_SEI_PAYLOAD),
            "::",
            stringify!(payload)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_PIC_PARAMS_H264() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_PIC_PARAMS_H264> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_PIC_PARAMS_H264>(),
        1536usize,
        concat!("Size of: ", stringify!(_NV_ENC_PIC_PARAMS_H264))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_PIC_PARAMS_H264>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_PIC_PARAMS_H264))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).displayPOCSyntax) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(displayPOCSyntax)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved3) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(reserved3)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).refPicFlag) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(refPicFlag)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).colourPlaneId) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(colourPlaneId)
        )
    );
    assert_eq!(
        unsafe {
            ::core::ptr::addr_of!((*ptr).forceIntraRefreshWithFrameCnt) as usize - ptr as usize
        },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(forceIntraRefreshWithFrameCnt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceTypeData) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(sliceTypeData)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceTypeArrayCnt) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(sliceTypeArrayCnt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).seiPayloadArrayCnt) as usize - ptr as usize },
        36usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(seiPayloadArrayCnt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).seiPayloadArray) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(seiPayloadArray)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceMode) as usize - ptr as usize },
        48usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(sliceMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceModeData) as usize - ptr as usize },
        52usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(sliceModeData)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ltrMarkFrameIdx) as usize - ptr as usize },
        56usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(ltrMarkFrameIdx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ltrUseFrameBitmap) as usize - ptr as usize },
        60usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(ltrUseFrameBitmap)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ltrUsageMode) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(ltrUsageMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).forceIntraSliceCount) as usize - ptr as usize },
        68usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(forceIntraSliceCount)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).forceIntraSliceIdx) as usize - ptr as usize },
        72usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(forceIntraSliceIdx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).h264ExtPicParams) as usize - ptr as usize },
        80usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(h264ExtPicParams)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).timeCode) as usize - ptr as usize },
        208usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(timeCode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        236usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1048usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_H264),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_PIC_PARAMS_HEVC() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_PIC_PARAMS_HEVC> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_PIC_PARAMS_HEVC>(),
        1536usize,
        concat!("Size of: ", stringify!(_NV_ENC_PIC_PARAMS_HEVC))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_PIC_PARAMS_HEVC>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_PIC_PARAMS_HEVC))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).displayPOCSyntax) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(displayPOCSyntax)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).refPicFlag) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(refPicFlag)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).temporalId) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(temporalId)
        )
    );
    assert_eq!(
        unsafe {
            ::core::ptr::addr_of!((*ptr).forceIntraRefreshWithFrameCnt) as usize - ptr as usize
        },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(forceIntraRefreshWithFrameCnt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceTypeData) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(sliceTypeData)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceTypeArrayCnt) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(sliceTypeArrayCnt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceMode) as usize - ptr as usize },
        36usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(sliceMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceModeData) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(sliceModeData)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ltrMarkFrameIdx) as usize - ptr as usize },
        44usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(ltrMarkFrameIdx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ltrUseFrameBitmap) as usize - ptr as usize },
        48usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(ltrUseFrameBitmap)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ltrUsageMode) as usize - ptr as usize },
        52usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(ltrUsageMode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).seiPayloadArrayCnt) as usize - ptr as usize },
        56usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(seiPayloadArrayCnt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        60usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).seiPayloadArray) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(seiPayloadArray)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).timeCode) as usize - ptr as usize },
        72usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(timeCode)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        100usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(reserved2)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved3) as usize - ptr as usize },
        1048usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_HEVC),
            "::",
            stringify!(reserved3)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_PIC_PARAMS_AV1() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_PIC_PARAMS_AV1> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_PIC_PARAMS_AV1>(),
        1552usize,
        concat!("Size of: ", stringify!(_NV_ENC_PIC_PARAMS_AV1))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_PIC_PARAMS_AV1>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_PIC_PARAMS_AV1))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).displayPOCSyntax) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(displayPOCSyntax)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).refPicFlag) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(refPicFlag)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).temporalId) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(temporalId)
        )
    );
    assert_eq!(
        unsafe {
            ::core::ptr::addr_of!((*ptr).forceIntraRefreshWithFrameCnt) as usize - ptr as usize
        },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(forceIntraRefreshWithFrameCnt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numTileColumns) as usize - ptr as usize },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(numTileColumns)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numTileRows) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(numTileRows)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).tileWidths) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(tileWidths)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).tileHeights) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(tileHeights)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).obuPayloadArrayCnt) as usize - ptr as usize },
        48usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(obuPayloadArrayCnt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        52usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).obuPayloadArray) as usize - ptr as usize },
        56usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(obuPayloadArray)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).filmGrainParams) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(filmGrainParams)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        72usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(reserved2)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved3) as usize - ptr as usize },
        1064usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS_AV1),
            "::",
            stringify!(reserved3)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_CODEC_PIC_PARAMS() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_CODEC_PIC_PARAMS> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_CODEC_PIC_PARAMS>(),
        1552usize,
        concat!("Size of: ", stringify!(_NV_ENC_CODEC_PIC_PARAMS))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_CODEC_PIC_PARAMS>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_CODEC_PIC_PARAMS))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).h264PicParams) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CODEC_PIC_PARAMS),
            "::",
            stringify!(h264PicParams)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).hevcPicParams) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CODEC_PIC_PARAMS),
            "::",
            stringify!(hevcPicParams)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).av1PicParams) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CODEC_PIC_PARAMS),
            "::",
            stringify!(av1PicParams)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_CODEC_PIC_PARAMS),
            "::",
            stringify!(reserved)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_PIC_PARAMS() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_PIC_PARAMS> = ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_PIC_PARAMS>(),
        3360usize,
        concat!("Size of: ", stringify!(_NV_ENC_PIC_PARAMS))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_PIC_PARAMS>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_PIC_PARAMS))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputWidth) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(inputWidth)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputHeight) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(inputHeight)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputPitch) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(inputPitch)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).encodePicFlags) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(encodePicFlags)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).frameIdx) as usize - ptr as usize },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(frameIdx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputTimeStamp) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(inputTimeStamp)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputDuration) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(inputDuration)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputBuffer) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(inputBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).outputBitstream) as usize - ptr as usize },
        48usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(outputBitstream)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).completionEvent) as usize - ptr as usize },
        56usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(completionEvent)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bufferFmt) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(bufferFmt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pictureStruct) as usize - ptr as usize },
        68usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(pictureStruct)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pictureType) as usize - ptr as usize },
        72usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(pictureType)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).codecPicParams) as usize - ptr as usize },
        80usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(codecPicParams)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).meHintCountsPerBlock) as usize - ptr as usize },
        1632usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(meHintCountsPerBlock)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).meExternalHints) as usize - ptr as usize },
        1664usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(meExternalHints)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        1672usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1696usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(reserved2)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).qpDeltaMap) as usize - ptr as usize },
        1712usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(qpDeltaMap)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).qpDeltaMapSize) as usize - ptr as usize },
        1720usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(qpDeltaMapSize)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reservedBitFields) as usize - ptr as usize },
        1724usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(reservedBitFields)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).meHintRefPicDist) as usize - ptr as usize },
        1728usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(meHintRefPicDist)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).alphaBuffer) as usize - ptr as usize },
        1736usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(alphaBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).meExternalSbHints) as usize - ptr as usize },
        1744usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(meExternalSbHints)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).meSbHintsCount) as usize - ptr as usize },
        1752usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(meSbHintsCount)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).stateBufferIdx) as usize - ptr as usize },
        1756usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(stateBufferIdx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).outputReconBuffer) as usize - ptr as usize },
        1760usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(outputReconBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved3) as usize - ptr as usize },
        1768usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(reserved3)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved4) as usize - ptr as usize },
        2904usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_PIC_PARAMS),
            "::",
            stringify!(reserved4)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_MEONLY_PARAMS() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_MEONLY_PARAMS> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_MEONLY_PARAMS>(),
        1552usize,
        concat!("Size of: ", stringify!(_NV_ENC_MEONLY_PARAMS))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_MEONLY_PARAMS>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_MEONLY_PARAMS))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MEONLY_PARAMS),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputWidth) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MEONLY_PARAMS),
            "::",
            stringify!(inputWidth)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputHeight) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MEONLY_PARAMS),
            "::",
            stringify!(inputHeight)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputBuffer) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MEONLY_PARAMS),
            "::",
            stringify!(inputBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).referenceFrame) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MEONLY_PARAMS),
            "::",
            stringify!(referenceFrame)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).mvBuffer) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MEONLY_PARAMS),
            "::",
            stringify!(mvBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bufferFmt) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MEONLY_PARAMS),
            "::",
            stringify!(bufferFmt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).completionEvent) as usize - ptr as usize },
        48usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MEONLY_PARAMS),
            "::",
            stringify!(completionEvent)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).viewID) as usize - ptr as usize },
        56usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MEONLY_PARAMS),
            "::",
            stringify!(viewID)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).meHintCountsPerBlock) as usize - ptr as usize },
        60usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MEONLY_PARAMS),
            "::",
            stringify!(meHintCountsPerBlock)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).meExternalHints) as usize - ptr as usize },
        96usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MEONLY_PARAMS),
            "::",
            stringify!(meExternalHints)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        104usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MEONLY_PARAMS),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1080usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MEONLY_PARAMS),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_LOCK_BITSTREAM() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_LOCK_BITSTREAM> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_LOCK_BITSTREAM>(),
        1552usize,
        concat!("Size of: ", stringify!(_NV_ENC_LOCK_BITSTREAM))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_LOCK_BITSTREAM>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_LOCK_BITSTREAM))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).outputBitstream) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(outputBitstream)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceOffsets) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(sliceOffsets)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).frameIdx) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(frameIdx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).hwEncodeStatus) as usize - ptr as usize },
        28usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(hwEncodeStatus)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).numSlices) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(numSlices)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bitstreamSizeInBytes) as usize - ptr as usize },
        36usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(bitstreamSizeInBytes)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).outputTimeStamp) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(outputTimeStamp)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).outputDuration) as usize - ptr as usize },
        48usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(outputDuration)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bitstreamBufferPtr) as usize - ptr as usize },
        56usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(bitstreamBufferPtr)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pictureType) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(pictureType)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pictureStruct) as usize - ptr as usize },
        68usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(pictureStruct)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).frameAvgQP) as usize - ptr as usize },
        72usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(frameAvgQP)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).frameSatd) as usize - ptr as usize },
        76usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(frameSatd)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ltrFrameIdx) as usize - ptr as usize },
        80usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(ltrFrameIdx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ltrFrameBitmap) as usize - ptr as usize },
        84usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(ltrFrameBitmap)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).temporalId) as usize - ptr as usize },
        88usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(temporalId)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).intraMBCount) as usize - ptr as usize },
        92usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(intraMBCount)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).interMBCount) as usize - ptr as usize },
        96usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(interMBCount)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).averageMVX) as usize - ptr as usize },
        100usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(averageMVX)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).averageMVY) as usize - ptr as usize },
        104usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(averageMVY)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).alphaLayerSizeInBytes) as usize - ptr as usize },
        108usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(alphaLayerSizeInBytes)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).outputStatsPtrSize) as usize - ptr as usize },
        112usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(outputStatsPtrSize)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).outputStatsPtr) as usize - ptr as usize },
        120usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(outputStatsPtr)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).frameIdxDisplay) as usize - ptr as usize },
        128usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(frameIdxDisplay)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        132usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1016usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(reserved2)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reservedInternal) as usize - ptr as usize },
        1520usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_BITSTREAM),
            "::",
            stringify!(reservedInternal)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_LOCK_INPUT_BUFFER() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_LOCK_INPUT_BUFFER> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_LOCK_INPUT_BUFFER>(),
        1544usize,
        concat!("Size of: ", stringify!(_NV_ENC_LOCK_INPUT_BUFFER))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_LOCK_INPUT_BUFFER>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_LOCK_INPUT_BUFFER))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_INPUT_BUFFER),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputBuffer) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_INPUT_BUFFER),
            "::",
            stringify!(inputBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bufferDataPtr) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_INPUT_BUFFER),
            "::",
            stringify!(bufferDataPtr)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pitch) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_INPUT_BUFFER),
            "::",
            stringify!(pitch)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        28usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_INPUT_BUFFER),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1032usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_LOCK_INPUT_BUFFER),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_MAP_INPUT_RESOURCE() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_MAP_INPUT_RESOURCE> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_MAP_INPUT_RESOURCE>(),
        1544usize,
        concat!("Size of: ", stringify!(_NV_ENC_MAP_INPUT_RESOURCE))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_MAP_INPUT_RESOURCE>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_MAP_INPUT_RESOURCE))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MAP_INPUT_RESOURCE),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).subResourceIndex) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MAP_INPUT_RESOURCE),
            "::",
            stringify!(subResourceIndex)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputResource) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MAP_INPUT_RESOURCE),
            "::",
            stringify!(inputResource)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).registeredResource) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MAP_INPUT_RESOURCE),
            "::",
            stringify!(registeredResource)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).mappedResource) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MAP_INPUT_RESOURCE),
            "::",
            stringify!(mappedResource)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).mappedBufferFmt) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MAP_INPUT_RESOURCE),
            "::",
            stringify!(mappedBufferFmt)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        36usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MAP_INPUT_RESOURCE),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1040usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_MAP_INPUT_RESOURCE),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_INPUT_RESOURCE_OPENGL_TEX() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_INPUT_RESOURCE_OPENGL_TEX> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_INPUT_RESOURCE_OPENGL_TEX>(),
        8usize,
        concat!("Size of: ", stringify!(_NV_ENC_INPUT_RESOURCE_OPENGL_TEX))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_INPUT_RESOURCE_OPENGL_TEX>(),
        4usize,
        concat!(
            "Alignment of ",
            stringify!(_NV_ENC_INPUT_RESOURCE_OPENGL_TEX)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).texture) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INPUT_RESOURCE_OPENGL_TEX),
            "::",
            stringify!(texture)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).target) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INPUT_RESOURCE_OPENGL_TEX),
            "::",
            stringify!(target)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_FENCE_POINT_D3D12() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_FENCE_POINT_D3D12> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_FENCE_POINT_D3D12>(),
        64usize,
        concat!("Size of: ", stringify!(_NV_ENC_FENCE_POINT_D3D12))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_FENCE_POINT_D3D12>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_FENCE_POINT_D3D12))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FENCE_POINT_D3D12),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FENCE_POINT_D3D12),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pFence) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FENCE_POINT_D3D12),
            "::",
            stringify!(pFence)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).waitValue) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FENCE_POINT_D3D12),
            "::",
            stringify!(waitValue)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).signalValue) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FENCE_POINT_D3D12),
            "::",
            stringify!(signalValue)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        36usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_FENCE_POINT_D3D12),
            "::",
            stringify!(reserved1)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_INPUT_RESOURCE_D3D12() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_INPUT_RESOURCE_D3D12> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_INPUT_RESOURCE_D3D12>(),
        272usize,
        concat!("Size of: ", stringify!(_NV_ENC_INPUT_RESOURCE_D3D12))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_INPUT_RESOURCE_D3D12>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_INPUT_RESOURCE_D3D12))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INPUT_RESOURCE_D3D12),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INPUT_RESOURCE_D3D12),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pInputBuffer) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INPUT_RESOURCE_D3D12),
            "::",
            stringify!(pInputBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inputFencePoint) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INPUT_RESOURCE_D3D12),
            "::",
            stringify!(inputFencePoint)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        80usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INPUT_RESOURCE_D3D12),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        144usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_INPUT_RESOURCE_D3D12),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_OUTPUT_RESOURCE_D3D12() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_OUTPUT_RESOURCE_D3D12> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_OUTPUT_RESOURCE_D3D12>(),
        272usize,
        concat!("Size of: ", stringify!(_NV_ENC_OUTPUT_RESOURCE_D3D12))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_OUTPUT_RESOURCE_D3D12>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_OUTPUT_RESOURCE_D3D12))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_RESOURCE_D3D12),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_RESOURCE_D3D12),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pOutputBuffer) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_RESOURCE_D3D12),
            "::",
            stringify!(pOutputBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).outputFencePoint) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_RESOURCE_D3D12),
            "::",
            stringify!(outputFencePoint)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        80usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_RESOURCE_D3D12),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        144usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OUTPUT_RESOURCE_D3D12),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_REGISTER_RESOURCE() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_REGISTER_RESOURCE> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_REGISTER_RESOURCE>(),
        1536usize,
        concat!("Size of: ", stringify!(_NV_ENC_REGISTER_RESOURCE))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_REGISTER_RESOURCE>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_REGISTER_RESOURCE))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).resourceType) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(resourceType)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).width) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(width)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).height) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(height)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pitch) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(pitch)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).subResourceIndex) as usize - ptr as usize },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(subResourceIndex)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).resourceToRegister) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(resourceToRegister)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).registeredResource) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(registeredResource)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bufferFormat) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(bufferFormat)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bufferUsage) as usize - ptr as usize },
        44usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(bufferUsage)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).pInputFencePoint) as usize - ptr as usize },
        48usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(pInputFencePoint)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).chromaOffset) as usize - ptr as usize },
        56usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(chromaOffset)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1048usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_REGISTER_RESOURCE),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_STAT() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_STAT> = ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_STAT>(),
        1544usize,
        concat!("Size of: ", stringify!(_NV_ENC_STAT))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_STAT>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_STAT))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).outputBitStream) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(outputBitStream)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).bitStreamSize) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(bitStreamSize)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).picType) as usize - ptr as usize },
        20usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(picType)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).lastValidByteOffset) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(lastValidByteOffset)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).sliceOffsets) as usize - ptr as usize },
        28usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(sliceOffsets)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).picIdx) as usize - ptr as usize },
        92usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(picIdx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).frameAvgQP) as usize - ptr as usize },
        96usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(frameAvgQP)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ltrFrameIdx) as usize - ptr as usize },
        104usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(ltrFrameIdx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).intraMBCount) as usize - ptr as usize },
        108usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(intraMBCount)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).interMBCount) as usize - ptr as usize },
        112usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(interMBCount)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).averageMVX) as usize - ptr as usize },
        116usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(averageMVX)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).averageMVY) as usize - ptr as usize },
        120usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(averageMVY)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        124usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1032usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_STAT),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_SEQUENCE_PARAM_PAYLOAD() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_SEQUENCE_PARAM_PAYLOAD> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_SEQUENCE_PARAM_PAYLOAD>(),
        1544usize,
        concat!("Size of: ", stringify!(_NV_ENC_SEQUENCE_PARAM_PAYLOAD))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_SEQUENCE_PARAM_PAYLOAD>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_SEQUENCE_PARAM_PAYLOAD))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_SEQUENCE_PARAM_PAYLOAD),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).inBufferSize) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_SEQUENCE_PARAM_PAYLOAD),
            "::",
            stringify!(inBufferSize)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).spsId) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_SEQUENCE_PARAM_PAYLOAD),
            "::",
            stringify!(spsId)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).ppsId) as usize - ptr as usize },
        12usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_SEQUENCE_PARAM_PAYLOAD),
            "::",
            stringify!(ppsId)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).spsppsBuffer) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_SEQUENCE_PARAM_PAYLOAD),
            "::",
            stringify!(spsppsBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).outSPSPPSPayloadSize) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_SEQUENCE_PARAM_PAYLOAD),
            "::",
            stringify!(outSPSPPSPayloadSize)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_SEQUENCE_PARAM_PAYLOAD),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1032usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_SEQUENCE_PARAM_PAYLOAD),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_EVENT_PARAMS() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_EVENT_PARAMS> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_EVENT_PARAMS>(),
        1544usize,
        concat!("Size of: ", stringify!(_NV_ENC_EVENT_PARAMS))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_EVENT_PARAMS>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENC_EVENT_PARAMS))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_EVENT_PARAMS),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_EVENT_PARAMS),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).completionEvent) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_EVENT_PARAMS),
            "::",
            stringify!(completionEvent)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_EVENT_PARAMS),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1032usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_EVENT_PARAMS),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENC_OPEN_ENCODE_SESSIONEX_PARAMS() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENC_OPEN_ENCODE_SESSIONEX_PARAMS> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENC_OPEN_ENCODE_SESSIONEX_PARAMS>(),
        1552usize,
        concat!(
            "Size of: ",
            stringify!(_NV_ENC_OPEN_ENCODE_SESSIONEX_PARAMS)
        )
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENC_OPEN_ENCODE_SESSIONEX_PARAMS>(),
        8usize,
        concat!(
            "Alignment of ",
            stringify!(_NV_ENC_OPEN_ENCODE_SESSIONEX_PARAMS)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OPEN_ENCODE_SESSIONEX_PARAMS),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).deviceType) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OPEN_ENCODE_SESSIONEX_PARAMS),
            "::",
            stringify!(deviceType)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).device) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OPEN_ENCODE_SESSIONEX_PARAMS),
            "::",
            stringify!(device)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OPEN_ENCODE_SESSIONEX_PARAMS),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).apiVersion) as usize - ptr as usize },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OPEN_ENCODE_SESSIONEX_PARAMS),
            "::",
            stringify!(apiVersion)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        28usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OPEN_ENCODE_SESSIONEX_PARAMS),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        1040usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENC_OPEN_ENCODE_SESSIONEX_PARAMS),
            "::",
            stringify!(reserved2)
        )
    );
}
#[test]
fn bindgen_test_layout__NV_ENCODE_API_FUNCTION_LIST() {
    const UNINIT: ::core::mem::MaybeUninit<_NV_ENCODE_API_FUNCTION_LIST> =
        ::core::mem::MaybeUninit::uninit();
    let ptr = UNINIT.as_ptr();
    assert_eq!(
        ::core::mem::size_of::<_NV_ENCODE_API_FUNCTION_LIST>(),
        2552usize,
        concat!("Size of: ", stringify!(_NV_ENCODE_API_FUNCTION_LIST))
    );
    assert_eq!(
        ::core::mem::align_of::<_NV_ENCODE_API_FUNCTION_LIST>(),
        8usize,
        concat!("Alignment of ", stringify!(_NV_ENCODE_API_FUNCTION_LIST))
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).version) as usize - ptr as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(version)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved) as usize - ptr as usize },
        4usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(reserved)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncOpenEncodeSession) as usize - ptr as usize },
        8usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncOpenEncodeSession)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncGetEncodeGUIDCount) as usize - ptr as usize },
        16usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetEncodeGUIDCount)
        )
    );
    assert_eq!(
        unsafe {
            ::core::ptr::addr_of!((*ptr).nvEncGetEncodeProfileGUIDCount) as usize - ptr as usize
        },
        24usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetEncodeProfileGUIDCount)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncGetEncodeProfileGUIDs) as usize - ptr as usize },
        32usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetEncodeProfileGUIDs)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncGetEncodeGUIDs) as usize - ptr as usize },
        40usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetEncodeGUIDs)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncGetInputFormatCount) as usize - ptr as usize },
        48usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetInputFormatCount)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncGetInputFormats) as usize - ptr as usize },
        56usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetInputFormats)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncGetEncodeCaps) as usize - ptr as usize },
        64usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetEncodeCaps)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncGetEncodePresetCount) as usize - ptr as usize },
        72usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetEncodePresetCount)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncGetEncodePresetGUIDs) as usize - ptr as usize },
        80usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetEncodePresetGUIDs)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncGetEncodePresetConfig) as usize - ptr as usize },
        88usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetEncodePresetConfig)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncInitializeEncoder) as usize - ptr as usize },
        96usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncInitializeEncoder)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncCreateInputBuffer) as usize - ptr as usize },
        104usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncCreateInputBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncDestroyInputBuffer) as usize - ptr as usize },
        112usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncDestroyInputBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncCreateBitstreamBuffer) as usize - ptr as usize },
        120usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncCreateBitstreamBuffer)
        )
    );
    assert_eq!(
        unsafe {
            ::core::ptr::addr_of!((*ptr).nvEncDestroyBitstreamBuffer) as usize - ptr as usize
        },
        128usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncDestroyBitstreamBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncEncodePicture) as usize - ptr as usize },
        136usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncEncodePicture)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncLockBitstream) as usize - ptr as usize },
        144usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncLockBitstream)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncUnlockBitstream) as usize - ptr as usize },
        152usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncUnlockBitstream)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncLockInputBuffer) as usize - ptr as usize },
        160usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncLockInputBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncUnlockInputBuffer) as usize - ptr as usize },
        168usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncUnlockInputBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncGetEncodeStats) as usize - ptr as usize },
        176usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetEncodeStats)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncGetSequenceParams) as usize - ptr as usize },
        184usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetSequenceParams)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncRegisterAsyncEvent) as usize - ptr as usize },
        192usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncRegisterAsyncEvent)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncUnregisterAsyncEvent) as usize - ptr as usize },
        200usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncUnregisterAsyncEvent)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncMapInputResource) as usize - ptr as usize },
        208usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncMapInputResource)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncUnmapInputResource) as usize - ptr as usize },
        216usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncUnmapInputResource)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncDestroyEncoder) as usize - ptr as usize },
        224usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncDestroyEncoder)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncInvalidateRefFrames) as usize - ptr as usize },
        232usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncInvalidateRefFrames)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncOpenEncodeSessionEx) as usize - ptr as usize },
        240usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncOpenEncodeSessionEx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncRegisterResource) as usize - ptr as usize },
        248usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncRegisterResource)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncUnregisterResource) as usize - ptr as usize },
        256usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncUnregisterResource)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncReconfigureEncoder) as usize - ptr as usize },
        264usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncReconfigureEncoder)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved1) as usize - ptr as usize },
        272usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(reserved1)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncCreateMVBuffer) as usize - ptr as usize },
        280usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncCreateMVBuffer)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncDestroyMVBuffer) as usize - ptr as usize },
        288usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncDestroyMVBuffer)
        )
    );
    assert_eq!(
        unsafe {
            ::core::ptr::addr_of!((*ptr).nvEncRunMotionEstimationOnly) as usize - ptr as usize
        },
        296usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncRunMotionEstimationOnly)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncGetLastErrorString) as usize - ptr as usize },
        304usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetLastErrorString)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncSetIOCudaStreams) as usize - ptr as usize },
        312usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncSetIOCudaStreams)
        )
    );
    assert_eq!(
        unsafe {
            ::core::ptr::addr_of!((*ptr).nvEncGetEncodePresetConfigEx) as usize - ptr as usize
        },
        320usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetEncodePresetConfigEx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncGetSequenceParamEx) as usize - ptr as usize },
        328usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncGetSequenceParamEx)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncRestoreEncoderState) as usize - ptr as usize },
        336usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncRestoreEncoderState)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).nvEncLookaheadPicture) as usize - ptr as usize },
        344usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(nvEncLookaheadPicture)
        )
    );
    assert_eq!(
        unsafe { ::core::ptr::addr_of!((*ptr).reserved2) as usize - ptr as usize },
        352usize,
        concat!(
            "Offset of field: ",
            stringify!(_NV_ENCODE_API_FUNCTION_LIST),
            "::",
            stringify!(reserved2)
        )
    );
}
