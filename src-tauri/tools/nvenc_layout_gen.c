/* nvenc_layout_gen.c — emits the Rust body of
 * src/windows_utils/streamer/test/nvenc_layout.rs from the real NVENC header.
 *
 * The problem this exists to solve: the layout test used to assert the Rust structs'
 * `size_of`/`offset_of` against constants emitted by the same bindgen run that produced
 * the structs. That is self-referential — it detects a hand edit to the Rust and nothing
 * else, and in particular it cannot detect bindings generated against one SDK while
 * `version.rs`, which is hand-written, encodes the struct revisions of another. A wrong
 * revision word is not a compile error and not a crash: `nvEncInitializeEncoder` returns
 * NV_ENC_ERR_INVALID_VERSION, `build_backend` catches it, and the whole NVIDIA path
 * silently degrades to Quick Sync or x264.
 *
 * Every number below comes from a C compiler reading
 * ../resources/nvenc-headers/nvEncodeAPI.h, so a header bump moves the numbers and the
 * diff is the review item.
 *
 * Build and run — see tools/README.md:
 *   call "C:\...\VC\Auxiliary\Build\vcvars64.bat"
 *   cl /nologo /TC /W3 /I ..\resources\nvenc-headers nvenc_layout_gen.c /Fe:nvenc_layout_gen.exe
 *   nvenc_layout_gen.exe > nvenc_layout_generated.txt
 *
 * MSVC, not MinGW: nvEncodeAPI64.dll is MSVC-built, and the bitfield packing rules the
 * two compilers use for the `unsigned int x : 1` runs in NV_ENC_CONFIG_H264 and
 * NV_ENC_PIC_PARAMS_H264 are not the same.
 */
#include <stddef.h>
#include <stdio.h>

#include "nvEncodeAPI.h"

#define SZ(T)     printf("    assert_eq!(size_of::<%s>(), %zu, \"%s\");\n", #T, sizeof(T), #T)
#define AL(T)     printf("    assert_eq!(align_of::<%s>(), %zu, \"%s\");\n", #T, _Alignof(T), #T)
#define OFF(T, f) printf("    assert_eq!(offset_of!(%s, %s), %zu, \"%s.%s\");\n", \
                         #T, #f, offsetof(T, f), #T, #f)
#define U32(C)    printf("    assert_eq!(%s, 0x%08X, \"%s\");\n", #C, (unsigned)(C), #C)
#define ENUMV(E)  printf("    assert_eq!(%s as i64, %lld, \"%s\");\n", #E, (long long)(E), #E)
/* A GUID is 16 bytes whose interpretation the driver fixes; comparing the four fields is
 * the same check the driver does and reads better in a failure than a byte array. */
#define GUIDV(G)  printf("    assert_guid(%s, 0x%08lX, 0x%04X, 0x%04X, [%u, %u, %u, %u, %u, %u, %u, %u], \"%s\");\n", \
                         #G, (unsigned long)(G).Data1, (G).Data2, (G).Data3, \
                         (G).Data4[0], (G).Data4[1], (G).Data4[2], (G).Data4[3], \
                         (G).Data4[4], (G).Data4[5], (G).Data4[6], (G).Data4[7], #G)

int main(void)
{
    puts("// ---- API version ----");
    printf("    assert_eq!(NVENCAPI_MAJOR_VERSION, %u, \"NVENCAPI_MAJOR_VERSION\");\n",
           (unsigned)NVENCAPI_MAJOR_VERSION);
    printf("    assert_eq!(NVENCAPI_MINOR_VERSION, %u, \"NVENCAPI_MINOR_VERSION\");\n",
           (unsigned)NVENCAPI_MINOR_VERSION);
    U32(NVENCAPI_VERSION);

    puts("");
    puts("// ---- struct version words ----");
    puts("// These are what the driver validates. A wrong revision here is not a crash: the");
    puts("// call returns NV_ENC_ERR_INVALID_VERSION and the NVIDIA path silently degrades.");
    U32(NV_ENCODE_API_FUNCTION_LIST_VER);
    U32(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER);
    U32(NV_ENC_INITIALIZE_PARAMS_VER);
    U32(NV_ENC_CONFIG_VER);
    U32(NV_ENC_RC_PARAMS_VER);
    U32(NV_ENC_PRESET_CONFIG_VER);
    U32(NV_ENC_RECONFIGURE_PARAMS_VER);
    U32(NV_ENC_PIC_PARAMS_VER);
    U32(NV_ENC_LOCK_BITSTREAM_VER);
    U32(NV_ENC_MAP_INPUT_RESOURCE_VER);
    U32(NV_ENC_REGISTER_RESOURCE_VER);
    U32(NV_ENC_CREATE_BITSTREAM_BUFFER_VER);
    U32(NV_ENC_EVENT_PARAMS_VER);
    U32(NV_ENC_CAPS_PARAM_VER);
    U32(NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER);

    puts("");
    puts("// ---- sizes and alignment ----");
    SZ(NV_ENCODE_API_FUNCTION_LIST);            AL(NV_ENCODE_API_FUNCTION_LIST);
    SZ(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS);   AL(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS);
    SZ(NV_ENC_INITIALIZE_PARAMS);               AL(NV_ENC_INITIALIZE_PARAMS);
    SZ(NV_ENC_CONFIG);                          AL(NV_ENC_CONFIG);
    SZ(NV_ENC_RC_PARAMS);                       AL(NV_ENC_RC_PARAMS);
    SZ(NV_ENC_QP);                              AL(NV_ENC_QP);
    SZ(NV_ENC_CODEC_CONFIG);                    AL(NV_ENC_CODEC_CONFIG);
    SZ(NV_ENC_CONFIG_H264);                     AL(NV_ENC_CONFIG_H264);
    SZ(NV_ENC_CONFIG_H264_VUI_PARAMETERS);      AL(NV_ENC_CONFIG_H264_VUI_PARAMETERS);
    SZ(NV_ENC_PRESET_CONFIG);                   AL(NV_ENC_PRESET_CONFIG);
    SZ(NV_ENC_RECONFIGURE_PARAMS);              AL(NV_ENC_RECONFIGURE_PARAMS);
    SZ(NV_ENC_PIC_PARAMS);                      AL(NV_ENC_PIC_PARAMS);
    SZ(NV_ENC_CODEC_PIC_PARAMS);                AL(NV_ENC_CODEC_PIC_PARAMS);
    SZ(NV_ENC_PIC_PARAMS_H264);                 AL(NV_ENC_PIC_PARAMS_H264);
    SZ(NV_ENC_LOCK_BITSTREAM);                  AL(NV_ENC_LOCK_BITSTREAM);
    SZ(NV_ENC_MAP_INPUT_RESOURCE);              AL(NV_ENC_MAP_INPUT_RESOURCE);
    SZ(NV_ENC_REGISTER_RESOURCE);               AL(NV_ENC_REGISTER_RESOURCE);
    SZ(NV_ENC_CREATE_BITSTREAM_BUFFER);         AL(NV_ENC_CREATE_BITSTREAM_BUFFER);
    SZ(NV_ENC_EVENT_PARAMS);                    AL(NV_ENC_EVENT_PARAMS);
    SZ(NV_ENC_CAPS_PARAM);                      AL(NV_ENC_CAPS_PARAM);
    SZ(NV_ENC_SEQUENCE_PARAM_PAYLOAD);          AL(NV_ENC_SEQUENCE_PARAM_PAYLOAD);

    puts("");
    puts("// ---- NV_ENCODE_API_FUNCTION_LIST: the driver fills this, so an offset that");
    puts("// disagrees is a call through the wrong function pointer, not a type error ----");
    OFF(NV_ENCODE_API_FUNCTION_LIST, version);
    OFF(NV_ENCODE_API_FUNCTION_LIST, reserved);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncOpenEncodeSession);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeGUIDCount);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeGUIDs);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetInputFormatCount);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetInputFormats);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeCaps);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetCount);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetGUIDs);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetConfig);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncInitializeEncoder);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncCreateInputBuffer);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyInputBuffer);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncCreateBitstreamBuffer);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyBitstreamBuffer);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncEncodePicture);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncLockBitstream);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncUnlockBitstream);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncLockInputBuffer);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncUnlockInputBuffer);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeStats);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetSequenceParams);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncRegisterAsyncEvent);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncUnregisterAsyncEvent);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncMapInputResource);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncUnmapInputResource);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyEncoder);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncInvalidateRefFrames);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncOpenEncodeSessionEx);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncRegisterResource);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncUnregisterResource);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncReconfigureEncoder);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncCreateMVBuffer);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyMVBuffer);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncRunMotionEstimationOnly);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetLastErrorString);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncSetIOCudaStreams);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetConfigEx);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetSequenceParamEx);

    puts("");
    puts("// ---- field offsets, for every field the encoder writes or reads ----");
    OFF(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, version);
    OFF(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, deviceType);
    OFF(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, device);
    OFF(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, apiVersion);

    OFF(NV_ENC_INITIALIZE_PARAMS, version);
    OFF(NV_ENC_INITIALIZE_PARAMS, encodeGUID);
    OFF(NV_ENC_INITIALIZE_PARAMS, presetGUID);
    OFF(NV_ENC_INITIALIZE_PARAMS, encodeWidth);
    OFF(NV_ENC_INITIALIZE_PARAMS, encodeHeight);
    OFF(NV_ENC_INITIALIZE_PARAMS, darWidth);
    OFF(NV_ENC_INITIALIZE_PARAMS, darHeight);
    OFF(NV_ENC_INITIALIZE_PARAMS, frameRateNum);
    OFF(NV_ENC_INITIALIZE_PARAMS, frameRateDen);
    OFF(NV_ENC_INITIALIZE_PARAMS, enableEncodeAsync);
    OFF(NV_ENC_INITIALIZE_PARAMS, enablePTD);
    OFF(NV_ENC_INITIALIZE_PARAMS, encodeConfig);
    OFF(NV_ENC_INITIALIZE_PARAMS, maxEncodeWidth);
    OFF(NV_ENC_INITIALIZE_PARAMS, maxEncodeHeight);
    OFF(NV_ENC_INITIALIZE_PARAMS, tuningInfo);
    OFF(NV_ENC_INITIALIZE_PARAMS, bufferFormat);

    OFF(NV_ENC_CONFIG, version);
    OFF(NV_ENC_CONFIG, profileGUID);
    OFF(NV_ENC_CONFIG, gopLength);
    OFF(NV_ENC_CONFIG, frameIntervalP);
    OFF(NV_ENC_CONFIG, rcParams);
    OFF(NV_ENC_CONFIG, encodeCodecConfig);

    OFF(NV_ENC_RC_PARAMS, version);
    OFF(NV_ENC_RC_PARAMS, rateControlMode);
    OFF(NV_ENC_RC_PARAMS, constQP);
    OFF(NV_ENC_RC_PARAMS, averageBitRate);
    OFF(NV_ENC_RC_PARAMS, maxBitRate);
    OFF(NV_ENC_RC_PARAMS, vbvBufferSize);
    OFF(NV_ENC_RC_PARAMS, vbvInitialDelay);
    OFF(NV_ENC_RC_PARAMS, minQP);
    OFF(NV_ENC_RC_PARAMS, maxQP);
    OFF(NV_ENC_RC_PARAMS, targetQuality);
    OFF(NV_ENC_RC_PARAMS, lookaheadDepth);
    OFF(NV_ENC_RC_PARAMS, lowDelayKeyFrameScale);
    OFF(NV_ENC_RC_PARAMS, multiPass);

    OFF(NV_ENC_QP, qpInterP);
    OFF(NV_ENC_QP, qpInterB);
    OFF(NV_ENC_QP, qpIntra);

    OFF(NV_ENC_CONFIG_H264, idrPeriod);
    OFF(NV_ENC_CONFIG_H264, intraRefreshPeriod);
    OFF(NV_ENC_CONFIG_H264, intraRefreshCnt);
    OFF(NV_ENC_CONFIG_H264, maxNumRefFrames);
    OFF(NV_ENC_CONFIG_H264, sliceMode);
    OFF(NV_ENC_CONFIG_H264, sliceModeData);
    OFF(NV_ENC_CONFIG_H264, h264VUIParameters);
    OFF(NV_ENC_CONFIG_H264, ltrNumFrames);
    OFF(NV_ENC_CONFIG_H264, ltrTrustMode);
    OFF(NV_ENC_CONFIG_H264, entropyCodingMode);
    OFF(NV_ENC_CONFIG_H264, adaptiveTransformMode);
    OFF(NV_ENC_CONFIG_H264, numRefL0);
    OFF(NV_ENC_CONFIG_H264, numRefL1);

    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoSignalTypePresentFlag);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoFormat);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoFullRangeFlag);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourDescriptionPresentFlag);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourPrimaries);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, transferCharacteristics);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourMatrix);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, bitstreamRestrictionFlag);

    OFF(NV_ENC_PIC_PARAMS, version);
    OFF(NV_ENC_PIC_PARAMS, inputWidth);
    OFF(NV_ENC_PIC_PARAMS, inputHeight);
    OFF(NV_ENC_PIC_PARAMS, inputPitch);
    OFF(NV_ENC_PIC_PARAMS, encodePicFlags);
    OFF(NV_ENC_PIC_PARAMS, inputBuffer);
    OFF(NV_ENC_PIC_PARAMS, outputBitstream);
    OFF(NV_ENC_PIC_PARAMS, completionEvent);
    OFF(NV_ENC_PIC_PARAMS, bufferFmt);
    OFF(NV_ENC_PIC_PARAMS, pictureStruct);
    OFF(NV_ENC_PIC_PARAMS, inputTimeStamp);
    OFF(NV_ENC_PIC_PARAMS, frameIdx);
    OFF(NV_ENC_PIC_PARAMS, codecPicParams);

    OFF(NV_ENC_PIC_PARAMS_H264, ltrMarkFrameIdx);
    OFF(NV_ENC_PIC_PARAMS_H264, ltrUseFrameBitmap);
    OFF(NV_ENC_PIC_PARAMS_H264, ltrUsageMode);
    OFF(NV_ENC_PIC_PARAMS_H264, sliceMode);
    OFF(NV_ENC_PIC_PARAMS_H264, sliceModeData);

    OFF(NV_ENC_LOCK_BITSTREAM, version);
    OFF(NV_ENC_LOCK_BITSTREAM, outputBitstream);
    OFF(NV_ENC_LOCK_BITSTREAM, bitstreamBufferPtr);
    OFF(NV_ENC_LOCK_BITSTREAM, bitstreamSizeInBytes);
    OFF(NV_ENC_LOCK_BITSTREAM, outputTimeStamp);
    OFF(NV_ENC_LOCK_BITSTREAM, pictureType);

    OFF(NV_ENC_MAP_INPUT_RESOURCE, version);
    OFF(NV_ENC_MAP_INPUT_RESOURCE, registeredResource);
    OFF(NV_ENC_MAP_INPUT_RESOURCE, mappedResource);
    OFF(NV_ENC_MAP_INPUT_RESOURCE, mappedBufferFmt);

    OFF(NV_ENC_REGISTER_RESOURCE, version);
    OFF(NV_ENC_REGISTER_RESOURCE, resourceType);
    OFF(NV_ENC_REGISTER_RESOURCE, width);
    OFF(NV_ENC_REGISTER_RESOURCE, height);
    OFF(NV_ENC_REGISTER_RESOURCE, pitch);
    OFF(NV_ENC_REGISTER_RESOURCE, resourceToRegister);
    OFF(NV_ENC_REGISTER_RESOURCE, registeredResource);
    OFF(NV_ENC_REGISTER_RESOURCE, bufferFormat);
    OFF(NV_ENC_REGISTER_RESOURCE, bufferUsage);

    OFF(NV_ENC_CREATE_BITSTREAM_BUFFER, version);
    OFF(NV_ENC_CREATE_BITSTREAM_BUFFER, bitstreamBuffer);

    OFF(NV_ENC_EVENT_PARAMS, version);
    OFF(NV_ENC_EVENT_PARAMS, completionEvent);

    OFF(NV_ENC_CAPS_PARAM, version);
    OFF(NV_ENC_CAPS_PARAM, capsToQuery);

    OFF(NV_ENC_SEQUENCE_PARAM_PAYLOAD, version);
    OFF(NV_ENC_SEQUENCE_PARAM_PAYLOAD, inBufferSize);
    OFF(NV_ENC_SEQUENCE_PARAM_PAYLOAD, spsppsBuffer);
    OFF(NV_ENC_SEQUENCE_PARAM_PAYLOAD, outSPSPPSPayloadSize);

    OFF(NV_ENC_RECONFIGURE_PARAMS, version);
    OFF(NV_ENC_RECONFIGURE_PARAMS, reInitEncodeParams);

    OFF(NV_ENC_PRESET_CONFIG, version);
    OFF(NV_ENC_PRESET_CONFIG, presetCfg);

    puts("");
    puts("// ---- enum values the encoder passes to the driver ----");
    ENUMV(NV_ENC_DEVICE_TYPE_DIRECTX);
    ENUMV(NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX);
    ENUMV(NV_ENC_INPUT_IMAGE);
    ENUMV(NV_ENC_BUFFER_FORMAT_ARGB);
    ENUMV(NV_ENC_BUFFER_FORMAT_ABGR);
    ENUMV(NV_ENC_BUFFER_FORMAT_NV12);
    ENUMV(NV_ENC_PIC_STRUCT_FRAME);
    ENUMV(NV_ENC_PIC_FLAG_FORCEINTRA);
    ENUMV(NV_ENC_PIC_FLAG_FORCEIDR);
    ENUMV(NV_ENC_PIC_FLAG_OUTPUT_SPSPPS);
    ENUMV(NV_ENC_PARAMS_RC_CONSTQP);
    ENUMV(NV_ENC_PARAMS_RC_VBR);
    ENUMV(NV_ENC_PARAMS_RC_CBR);
    ENUMV(NV_ENC_MULTI_PASS_DISABLED);
    ENUMV(NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY);
    ENUMV(NV_ENC_H264_ENTROPY_CODING_MODE_CABAC);
    ENUMV(NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC);
    ENUMV(NV_ENC_H264_ADAPTIVE_TRANSFORM_ENABLE);
    ENUMV(NV_ENC_H264_ADAPTIVE_TRANSFORM_DISABLE);
    ENUMV(NV_ENC_NUM_REF_FRAMES_1);
    ENUMV(NV_ENC_NUM_REF_FRAMES_4);
    ENUMV(NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED);
    ENUMV(NV_ENC_VUI_COLOR_PRIMARIES_BT709);
    ENUMV(NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709);
    ENUMV(NV_ENC_VUI_MATRIX_COEFFS_BT709);

    puts("");
    puts("// ---- the capability ids EncoderCaps::query asks for ----");
    ENUMV(NV_ENC_CAPS_NUM_MAX_BFRAMES);
    ENUMV(NV_ENC_CAPS_SUPPORTED_RATECONTROL_MODES);
    ENUMV(NV_ENC_CAPS_SUPPORT_FIELD_ENCODING);
    ENUMV(NV_ENC_CAPS_SUPPORT_MONOCHROME);
    ENUMV(NV_ENC_CAPS_SUPPORT_INTRA_REFRESH);
    ENUMV(NV_ENC_CAPS_SUPPORT_CONSTRAINED_ENCODING);
    ENUMV(NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE);
    ENUMV(NV_ENC_CAPS_SUPPORT_DYNAMIC_SLICE_MODE);
    ENUMV(NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION);
    ENUMV(NV_ENC_CAPS_WIDTH_MIN);
    ENUMV(NV_ENC_CAPS_WIDTH_MAX);
    ENUMV(NV_ENC_CAPS_HEIGHT_MIN);
    ENUMV(NV_ENC_CAPS_HEIGHT_MAX);
    ENUMV(NV_ENC_CAPS_LEVEL_MAX);
    ENUMV(NV_ENC_CAPS_SUPPORT_CABAC);
    ENUMV(NV_ENC_CAPS_SUPPORT_ADAPTIVE_TRANSFORM);
    ENUMV(NV_ENC_CAPS_NUM_ENCODER_ENGINES);
    ENUMV(NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK);
    ENUMV(NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT);
    ENUMV(NV_ENC_CAPS_NUM_MAX_LTR_FRAMES);
    ENUMV(NV_ENC_CAPS_SUPPORT_MULTIPLE_REF_FRAMES);
    ENUMV(NV_ENC_CAPS_SINGLE_SLICE_INTRA_REFRESH);
    ENUMV(NV_ENC_CAPS_EXPOSED_COUNT);

    puts("");
    puts("// ---- status codes the encoder branches on ----");
    ENUMV(NV_ENC_SUCCESS);
    ENUMV(NV_ENC_ERR_OUT_OF_MEMORY);
    ENUMV(NV_ENC_ERR_INVALID_VERSION);
    ENUMV(NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY);
    ENUMV(NV_ENC_ERR_ENCODER_BUSY);
    ENUMV(NV_ENC_ERR_NEED_MORE_INPUT);

    puts("");
    puts("// ---- GUIDs: a wrong one selects a different codec, preset or profile, and the");
    puts("// driver reports it as an unsupported parameter set rather than a bad GUID ----");
    GUIDV(NV_ENC_CODEC_H264_GUID);
    GUIDV(NV_ENC_H264_PROFILE_BASELINE_GUID);
    GUIDV(NV_ENC_H264_PROFILE_MAIN_GUID);
    GUIDV(NV_ENC_H264_PROFILE_HIGH_GUID);
    GUIDV(NV_ENC_PRESET_P1_GUID);
    GUIDV(NV_ENC_PRESET_P4_GUID);
    GUIDV(NV_ENC_PRESET_P7_GUID);

    return 0;
}
