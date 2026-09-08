/* amf_layout_gen.c — emits the Rust body of
 * src/windows_utils/streamer/test/amd_layout.rs from the real AMD AMF headers.
 *
 * Compiled in C mode (`cl /TC`) on purpose: the AMF headers carry two APIs behind
 * `#ifdef __cplusplus`, and the `#else` C branch — plain vtable structs — is the one
 * `amd_sys` binds. `offsetof(SomeVtbl, member) / sizeof(void *)` IS the vtable slot
 * index, because a C-API vtable struct is exactly an array of function pointers.
 *
 * The Rust field names in `amd_sys::vtbl` are the C member names verbatim (the module
 * carries `#![allow(non_snake_case)]`), so the output below is a literal paste, not a
 * translation.
 *
 * Build and run — see tools/README.md:
 *   call "C:\...\VC\Auxiliary\Build\vcvars64.bat"
 *   cl /nologo /TC /W3 /I ..\resources\amf-headers amf_layout_gen.c /Fe:amf_layout_gen.exe
 *   amf_layout_gen.exe > generated.txt
 */
#include <stddef.h>
#include <stdio.h>

#include "core/Factory.h"
#include "core/Trace.h"
#include "core/PropertyStorageEx.h"
#include "components/Component.h"
#include "components/ComponentCaps.h"
#include "components/VideoEncoderVCE.h"

#define SZ(T)      printf("    assert_eq!(size_of::<%s>(), %zu, \"%s\");\n", #T, sizeof(T), #T)
#define AL(T)      printf("    assert_eq!(align_of::<%s>(), %zu, \"%s\");\n", #T, _Alignof(T), #T)
#define OFF(T, f)  printf("    assert_eq!(offset_of!(%s, %s), %zu, \"%s.%s\");\n", #T, #f, offsetof(T, f), #T, #f)
#define SLOT(V, f) printf("    assert_eq!(slot!(%s, %s), %zu, \"%s::%s\");\n", #V, #f, \
                          offsetof(V, f) / sizeof(void *), #V, #f)
#define SLOTS(V)   printf("    assert_eq!(size_of::<%s>() / PTR, %zu, \"%s total slots\");\n", \
                          #V, sizeof(V) / sizeof(void *), #V)
#define ENUMV(E)   printf("    assert_eq!(%s as i64, %lld, \"%s\");\n", #E, (long long)(E), #E)
/* %ls prints a wchar_t* as the current locale's narrow string; every AMF property
 * name is plain ASCII, so this round-trips exactly. */
#define PROP(C)    printf("    assert_wide(props::%s, \"%ls\", \"%s\");\n", #C, C, #C)

int main(void)
{
    puts("// ---- sizes / alignment ----");
    SZ(AMFGuid);            AL(AMFGuid);
    SZ(AMFVariantStruct);   AL(AMFVariantStruct);
    SZ(AMFSize);            SZ(AMFRate);   SZ(AMFRatio);
    SZ(AMFRect);            SZ(AMFPoint);  SZ(AMFColor);
    printf("    assert_eq!(PTR, %zu, \"pointer\");\n", sizeof(void *));

    puts("");
    puts("// ---- field offsets ----");
    /* `type` is a Rust keyword, hence the raw identifier on the Rust side. */
    printf("    assert_eq!(offset_of!(AMFVariantStruct, r#type), %zu, \"AMFVariantStruct.type\");\n",
           offsetof(AMFVariantStruct, type));
    printf("    assert_eq!(offset_of!(AMFVariantStruct, value), %zu, \"AMFVariantStruct.<union>\");\n",
           offsetof(AMFVariantStruct, int64Value));
    OFF(AMFGuid, data1); OFF(AMFGuid, data2); OFF(AMFGuid, data3); OFF(AMFGuid, data41);
    OFF(AMFGuid, data48);
    OFF(AMFSize, width); OFF(AMFSize, height);
    OFF(AMFRate, num);   OFF(AMFRate, den);

    /* AMFPropertyInfo is deliberately NOT modelled in Rust: its C layout (112 bytes,
     * name @0) differs from its C++ layout (120 bytes, name @8 — it has a virtual
     * destructor), and amfrt64.dll is built from the C++ headers. amd_sys probes
     * optional properties with HasProperty instead of GetPropertyInfo, so the trap
     * cannot be hit. tools/amf_layout_cpp_check.cpp re-measures the divergence. */
    printf("    // AMFPropertyInfo: C sizeof=%zu name@%zu — not modelled in Rust, see amf_layout_cpp_check.cpp\n",
           sizeof(AMFPropertyInfo), offsetof(AMFPropertyInfo, name));

    puts("");
    puts("// ---- vtable slot indices (THE ABI: a wrong index is memory corruption) ----");
    SLOT(AMFFactoryVtbl, CreateContext);
    SLOT(AMFFactoryVtbl, CreateComponent);
    SLOT(AMFFactoryVtbl, SetCacheFolder);
    SLOT(AMFFactoryVtbl, GetCacheFolder);
    SLOT(AMFFactoryVtbl, GetDebug);
    SLOT(AMFFactoryVtbl, GetTrace);
    SLOT(AMFFactoryVtbl, GetPrograms);
    SLOTS(AMFFactoryVtbl);

    puts("");
    SLOT(AMFInterfaceVtbl, Acquire);
    SLOT(AMFInterfaceVtbl, Release);
    SLOT(AMFInterfaceVtbl, QueryInterface);
    SLOTS(AMFInterfaceVtbl);

    puts("");
    SLOT(AMFPropertyStorageVtbl, Acquire);
    SLOT(AMFPropertyStorageVtbl, Release);
    SLOT(AMFPropertyStorageVtbl, QueryInterface);
    SLOT(AMFPropertyStorageVtbl, SetProperty);
    SLOT(AMFPropertyStorageVtbl, GetProperty);
    SLOT(AMFPropertyStorageVtbl, HasProperty);
    SLOT(AMFPropertyStorageVtbl, GetPropertyCount);
    SLOT(AMFPropertyStorageVtbl, GetPropertyAt);
    SLOT(AMFPropertyStorageVtbl, Clear);
    SLOT(AMFPropertyStorageVtbl, AddTo);
    SLOT(AMFPropertyStorageVtbl, CopyTo);
    SLOT(AMFPropertyStorageVtbl, AddObserver);
    SLOT(AMFPropertyStorageVtbl, RemoveObserver);
    SLOTS(AMFPropertyStorageVtbl);

    puts("");
    SLOT(AMFPropertyStorageExVtbl, GetPropertiesInfoCount);
    SLOT(AMFPropertyStorageExVtbl, GetPropertyInfoAt);
    SLOT(AMFPropertyStorageExVtbl, GetPropertyInfo);
    SLOT(AMFPropertyStorageExVtbl, ValidateProperty);
    SLOTS(AMFPropertyStorageExVtbl);

    puts("");
    SLOT(AMFContextVtbl, Terminate);
    SLOT(AMFContextVtbl, InitDX9);
    SLOT(AMFContextVtbl, InitDX11);
    SLOT(AMFContextVtbl, GetDX11Device);
    SLOT(AMFContextVtbl, LockDX11);
    SLOT(AMFContextVtbl, UnlockDX11);
    SLOT(AMFContextVtbl, InitOpenCL);
    SLOT(AMFContextVtbl, InitOpenGL);
    SLOT(AMFContextVtbl, AllocBuffer);
    SLOT(AMFContextVtbl, AllocSurface);
    SLOT(AMFContextVtbl, CreateBufferFromHostNative);
    SLOT(AMFContextVtbl, CreateSurfaceFromHostNative);
    SLOT(AMFContextVtbl, CreateSurfaceFromDX9Native);
    SLOT(AMFContextVtbl, CreateSurfaceFromDX11Native);
    SLOT(AMFContextVtbl, GetCompute);
    SLOTS(AMFContextVtbl);

    puts("");
    SLOT(AMFContext1Vtbl, InitDX11);
    SLOT(AMFContext1Vtbl, CreateSurfaceFromDX11Native);
    SLOT(AMFContext1Vtbl, CreateBufferFromDX11Native);
    SLOT(AMFContext1Vtbl, AllocSurfaceEx);
    SLOT(AMFContext1Vtbl, InitVulkan);
    SLOTS(AMFContext1Vtbl);

    puts("");
    SLOT(AMFComponentVtbl, SetProperty);
    SLOT(AMFComponentVtbl, GetProperty);
    SLOT(AMFComponentVtbl, HasProperty);
    SLOT(AMFComponentVtbl, Init);
    SLOT(AMFComponentVtbl, ReInit);
    SLOT(AMFComponentVtbl, Terminate);
    SLOT(AMFComponentVtbl, Drain);
    SLOT(AMFComponentVtbl, Flush);
    SLOT(AMFComponentVtbl, SubmitInput);
    SLOT(AMFComponentVtbl, QueryOutput);
    SLOT(AMFComponentVtbl, GetContext);
    SLOT(AMFComponentVtbl, SetOutputDataAllocatorCB);
    SLOT(AMFComponentVtbl, GetCaps);
    SLOT(AMFComponentVtbl, Optimize);
    SLOTS(AMFComponentVtbl);

    puts("");
    SLOT(AMFDataVtbl, GetMemoryType);
    SLOT(AMFDataVtbl, Duplicate);
    SLOT(AMFDataVtbl, Convert);
    SLOT(AMFDataVtbl, Interop);
    SLOT(AMFDataVtbl, GetDataType);
    SLOT(AMFDataVtbl, IsReusable);
    SLOT(AMFDataVtbl, SetPts);
    SLOT(AMFDataVtbl, GetPts);
    SLOT(AMFDataVtbl, SetDuration);
    SLOT(AMFDataVtbl, GetDuration);
    SLOTS(AMFDataVtbl);

    puts("");
    SLOT(AMFSurfaceVtbl, GetFormat);
    SLOT(AMFSurfaceVtbl, GetPlanesCount);
    SLOT(AMFSurfaceVtbl, GetPlaneAt);
    SLOT(AMFSurfaceVtbl, GetPlane);
    SLOT(AMFSurfaceVtbl, GetFrameType);
    SLOT(AMFSurfaceVtbl, SetFrameType);
    SLOT(AMFSurfaceVtbl, SetCrop);
    SLOT(AMFSurfaceVtbl, CopySurfaceRegion);
    SLOT(AMFSurfaceVtbl, AddObserver_Surface);
    SLOT(AMFSurfaceVtbl, RemoveObserver_Surface);
    SLOTS(AMFSurfaceVtbl);

    puts("");
    SLOT(AMFBufferVtbl, SetSize);
    SLOT(AMFBufferVtbl, GetSize);
    SLOT(AMFBufferVtbl, GetNative);
    SLOT(AMFBufferVtbl, AddObserver_Buffer);
    SLOT(AMFBufferVtbl, RemoveObserver_Buffer);
    SLOTS(AMFBufferVtbl);

    puts("");
    SLOT(AMFPlaneVtbl, GetType);
    SLOT(AMFPlaneVtbl, GetNative);
    SLOT(AMFPlaneVtbl, GetPixelSizeInBytes);
    SLOT(AMFPlaneVtbl, GetOffsetX);
    SLOT(AMFPlaneVtbl, GetOffsetY);
    SLOT(AMFPlaneVtbl, GetWidth);
    SLOT(AMFPlaneVtbl, GetHeight);
    SLOT(AMFPlaneVtbl, GetHPitch);
    SLOT(AMFPlaneVtbl, GetVPitch);
    SLOT(AMFPlaneVtbl, IsTiled);
    SLOTS(AMFPlaneVtbl);

    puts("");
    SLOT(AMFCapsVtbl, GetAccelerationType);
    SLOT(AMFCapsVtbl, GetInputCaps);
    SLOT(AMFCapsVtbl, GetOutputCaps);
    SLOTS(AMFCapsVtbl);

    puts("");
    SLOT(AMFIOCapsVtbl, GetWidthRange);
    SLOT(AMFIOCapsVtbl, GetHeightRange);
    SLOT(AMFIOCapsVtbl, GetVertAlign);
    SLOT(AMFIOCapsVtbl, GetNumOfFormats);
    SLOT(AMFIOCapsVtbl, GetFormatAt);
    SLOT(AMFIOCapsVtbl, GetNumOfMemoryTypes);
    SLOT(AMFIOCapsVtbl, GetMemoryTypeAt);
    SLOT(AMFIOCapsVtbl, IsInterlacedSupported);
    SLOTS(AMFIOCapsVtbl);

    puts("");
    /* AMFTrace is NOT an AMFInterface: it starts at TraceW, there is no
     * Acquire/Release/QueryInterface prefix, and slots 0/1 are variadic. */
    SLOT(AMFTraceVtbl, SetGlobalLevel);
    SLOT(AMFTraceVtbl, GetGlobalLevel);
    SLOT(AMFTraceVtbl, EnableWriter);
    SLOT(AMFTraceVtbl, SetWriterLevel);
    SLOT(AMFTraceVtbl, GetResultText);
    SLOTS(AMFTraceVtbl);

    puts("");
    puts("// ---- enum values ----");
    ENUMV(AMF_OK); ENUMV(AMF_FAIL); ENUMV(AMF_UNEXPECTED); ENUMV(AMF_ACCESS_DENIED);
    ENUMV(AMF_INVALID_ARG); ENUMV(AMF_OUT_OF_MEMORY); ENUMV(AMF_INVALID_POINTER);
    ENUMV(AMF_NO_INTERFACE); ENUMV(AMF_NOT_IMPLEMENTED); ENUMV(AMF_NOT_SUPPORTED);
    ENUMV(AMF_NOT_FOUND); ENUMV(AMF_ALREADY_INITIALIZED);
    ENUMV(AMF_NOT_INITIALIZED); ENUMV(AMF_INVALID_FORMAT); ENUMV(AMF_WRONG_STATE);
    ENUMV(AMF_NO_DEVICE); ENUMV(AMF_DIRECTX_FAILED);
    ENUMV(AMF_EOF); ENUMV(AMF_REPEAT); ENUMV(AMF_INPUT_FULL);
    ENUMV(AMF_RESOLUTION_CHANGED); ENUMV(AMF_RESOLUTION_UPDATED);
    ENUMV(AMF_INVALID_DATA_TYPE); ENUMV(AMF_INVALID_RESOLUTION);
    ENUMV(AMF_CODEC_NOT_SUPPORTED);
    ENUMV(AMF_SURFACE_FORMAT_NOT_SUPPORTED); ENUMV(AMF_SURFACE_MUST_BE_SHARED);
    ENUMV(AMF_ENCODER_NOT_PRESENT); ENUMV(AMF_NEED_MORE_INPUT); ENUMV(AMF_VULKAN_FAILED);
    puts("");
    ENUMV(AMF_SURFACE_UNKNOWN); ENUMV(AMF_SURFACE_NV12); ENUMV(AMF_SURFACE_YV12);
    ENUMV(AMF_SURFACE_BGRA); ENUMV(AMF_SURFACE_ARGB);
    ENUMV(AMF_SURFACE_RGBA); ENUMV(AMF_SURFACE_P010); ENUMV(AMF_SURFACE_R10G10B10A2);
    puts("");
    ENUMV(AMF_MEMORY_HOST); ENUMV(AMF_MEMORY_DX9); ENUMV(AMF_MEMORY_DX11);
    ENUMV(AMF_MEMORY_OPENCL); ENUMV(AMF_MEMORY_VULKAN); ENUMV(AMF_MEMORY_DX12);
    ENUMV(AMF_DX11_0); ENUMV(AMF_DX11_1); ENUMV(AMF_DX12);
    puts("");
    ENUMV(AMF_DATA_BUFFER); ENUMV(AMF_DATA_SURFACE);
    ENUMV(AMF_VARIANT_EMPTY); ENUMV(AMF_VARIANT_BOOL); ENUMV(AMF_VARIANT_INT64);
    ENUMV(AMF_VARIANT_DOUBLE); ENUMV(AMF_VARIANT_RECT); ENUMV(AMF_VARIANT_SIZE);
    ENUMV(AMF_VARIANT_POINT); ENUMV(AMF_VARIANT_RATE);
    ENUMV(AMF_VARIANT_RATIO); ENUMV(AMF_VARIANT_COLOR); ENUMV(AMF_VARIANT_STRING);
    ENUMV(AMF_VARIANT_WSTRING); ENUMV(AMF_VARIANT_INTERFACE);
    ENUMV(AMF_FRAME_PROGRESSIVE); ENUMV(AMF_FRAME_UNKNOWN);
    ENUMV(AMF_ACCEL_NOT_SUPPORTED); ENUMV(AMF_ACCEL_HARDWARE); ENUMV(AMF_ACCEL_GPU);
    ENUMV(AMF_ACCEL_SOFTWARE);
    puts("");
    ENUMV(AMF_VIDEO_ENCODER_USAGE_TRANSCODING);
    ENUMV(AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY);
    ENUMV(AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY);
    ENUMV(AMF_VIDEO_ENCODER_USAGE_WEBCAM);
    ENUMV(AMF_VIDEO_ENCODER_USAGE_HIGH_QUALITY);
    ENUMV(AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY_HIGH_QUALITY);
    ENUMV(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP);
    ENUMV(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR);
    ENUMV(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR);
    ENUMV(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_LATENCY_CONSTRAINED_VBR);
    ENUMV(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_QUALITY_VBR);
    ENUMV(AMF_VIDEO_ENCODER_QUALITY_PRESET_BALANCED);
    ENUMV(AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED);
    ENUMV(AMF_VIDEO_ENCODER_QUALITY_PRESET_QUALITY);
    ENUMV(AMF_VIDEO_ENCODER_PROFILE_BASELINE);
    ENUMV(AMF_VIDEO_ENCODER_PROFILE_MAIN);
    ENUMV(AMF_VIDEO_ENCODER_PROFILE_HIGH);
    ENUMV(AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_BASELINE);
    ENUMV(AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_HIGH);
    ENUMV(AMF_H264_LEVEL__3_1); ENUMV(AMF_H264_LEVEL__4);
    ENUMV(AMF_H264_LEVEL__4_1); ENUMV(AMF_H264_LEVEL__4_2);
    ENUMV(AMF_H264_LEVEL__5); ENUMV(AMF_H264_LEVEL__5_1); ENUMV(AMF_H264_LEVEL__5_2);
    ENUMV(AMF_H264_LEVEL__6); ENUMV(AMF_H264_LEVEL__6_1); ENUMV(AMF_H264_LEVEL__6_2);
    ENUMV(AMF_VIDEO_ENCODER_PICTURE_TYPE_NONE);
    ENUMV(AMF_VIDEO_ENCODER_PICTURE_TYPE_SKIP);
    ENUMV(AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR);
    ENUMV(AMF_VIDEO_ENCODER_PICTURE_TYPE_I);
    ENUMV(AMF_VIDEO_ENCODER_PICTURE_TYPE_P);
    ENUMV(AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_IDR);
    ENUMV(AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_I);
    ENUMV(AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_P);
    ENUMV(AMF_VIDEO_ENCODER_PREENCODE_DISABLED);
    ENUMV(AMF_VIDEO_ENCODER_PREENCODE_ENABLED);
    ENUMV(AMF_VIDEO_ENCODER_UNDEFINED);
    ENUMV(AMF_VIDEO_ENCODER_CABAC);
    ENUMV(AMF_VIDEO_ENCODER_CALV);
    ENUMV(AMF_VIDEO_ENCODER_OUTPUT_MODE_FRAME);
    ENUMV(AMF_VIDEO_ENCODER_OUTPUT_MODE_SLICE);
    ENUMV(AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_FRAME);
    ENUMV(AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_SLICE);
    ENUMV(AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_SLICE_LAST);

    puts("");
    puts("// ---- property-name wide strings (a typo here fails silently at runtime) ----");
    PROP(AMF_VIDEO_ENCODER_USAGE);
    PROP(AMF_VIDEO_ENCODER_QUALITY_PRESET);
    PROP(AMF_VIDEO_ENCODER_FRAMESIZE);
    PROP(AMF_VIDEO_ENCODER_FRAMERATE);
    PROP(AMF_VIDEO_ENCODER_PROFILE);
    PROP(AMF_VIDEO_ENCODER_PROFILE_LEVEL);
    PROP(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD);
    PROP(AMF_VIDEO_ENCODER_TARGET_BITRATE);
    PROP(AMF_VIDEO_ENCODER_PEAK_BITRATE);
    PROP(AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE);
    PROP(AMF_VIDEO_ENCODER_INITIAL_VBV_BUFFER_FULLNESS);
    PROP(AMF_VIDEO_ENCODER_ENFORCE_HRD);
    PROP(AMF_VIDEO_ENCODER_FILLER_DATA_ENABLE);
    PROP(AMF_VIDEO_ENCODER_B_PIC_PATTERN);
    PROP(AMF_VIDEO_ENCODER_MAX_CONSECUTIVE_BPICTURES);
    PROP(AMF_VIDEO_ENCODER_LOWLATENCY_MODE);
    PROP(AMF_VIDEO_ENCODER_PREENCODE_ENABLE);
    PROP(AMF_VIDEO_ENCODER_PRE_ANALYSIS_ENABLE);
    PROP(AMF_VIDEO_ENCODER_ENABLE_VBAQ);
    PROP(AMF_VIDEO_ENCODER_HIGH_MOTION_QUALITY_BOOST_ENABLE);
    PROP(AMF_VIDEO_ENCODER_IDR_PERIOD);
    PROP(AMF_VIDEO_ENCODER_INTRA_PERIOD);
    PROP(AMF_VIDEO_ENCODER_HEADER_INSERTION_SPACING);
    PROP(AMF_VIDEO_ENCODER_DE_BLOCKING_FILTER);
    PROP(AMF_VIDEO_ENCODER_SLICES_PER_FRAME);
    PROP(AMF_VIDEO_ENCODER_CABAC_ENABLE);
    PROP(AMF_VIDEO_ENCODER_INPUT_FULL_RANGE_COLOR);
    PROP(AMF_VIDEO_ENCODER_OUTPUT_FULL_RANGE_COLOR);
    PROP(AMF_VIDEO_ENCODER_OUTPUT_COLOR_PROFILE);
    PROP(AMF_VIDEO_ENCODER_OUTPUT_TRANSFER_CHARACTERISTIC);
    PROP(AMF_VIDEO_ENCODER_OUTPUT_COLOR_PRIMARIES);
    PROP(AMF_VIDEO_ENCODER_QUERY_TIMEOUT);
    PROP(AMF_VIDEO_ENCODER_INPUT_QUEUE_SIZE);
    PROP(AMF_VIDEO_ENCODER_ENABLE_SMART_ACCESS_VIDEO);
    PROP(AMF_VIDEO_ENCODER_MAX_NUM_REFRAMES);
    PROP(AMF_VIDEO_ENCODER_INTRA_REFRESH_NUM_MBS_PER_SLOT);
    PROP(AMF_VIDEO_ENCODER_MAX_LTR_FRAMES);
    PROP(AMF_VIDEO_ENCODER_LTR_MODE);
    PROP(AMF_VIDEO_ENCODER_OUTPUT_MODE);
    PROP(AMF_VIDEO_ENCODER_EXTRADATA);
    PROP(AMF_VIDEO_ENCODER_FORCE_PICTURE_TYPE);
    PROP(AMF_VIDEO_ENCODER_INSERT_SPS);
    PROP(AMF_VIDEO_ENCODER_INSERT_PPS);
    PROP(AMF_VIDEO_ENCODER_INSERT_AUD);
    PROP(AMF_VIDEO_ENCODER_MARK_CURRENT_WITH_LTR_INDEX);
    PROP(AMF_VIDEO_ENCODER_FORCE_LTR_REFERENCE_BITFIELD);
    PROP(AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE);
    PROP(AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE);
    PROP(AMF_VIDEO_ENCODER_OUTPUT_MARKED_LTR_INDEX);
    PROP(AMF_VIDEO_ENCODER_OUTPUT_REFERENCED_LTR_INDEX_BITFIELD);
    PROP(AMF_VIDEO_ENCODER_CAP_MAX_BITRATE);
    PROP(AMF_VIDEO_ENCODER_CAP_NUM_OF_STREAMS);
    PROP(AMF_VIDEO_ENCODER_CAP_MAX_PROFILE);
    PROP(AMF_VIDEO_ENCODER_CAP_MAX_LEVEL);
    PROP(AMF_VIDEO_ENCODER_CAP_BFRAMES);
    PROP(AMF_VIDEO_ENCODER_CAP_MAX_REFERENCE_FRAMES);
    PROP(AMF_VIDEO_ENCODER_CAP_NUM_OF_HW_INSTANCES);
    PROP(AMF_VIDEO_ENCODER_CAP_COLOR_CONVERSION);
    PROP(AMF_VIDEO_ENCODER_CAP_PRE_ANALYSIS);
    PROP(AMF_VIDEO_ENCODER_CAP_MAX_THROUGHPUT);
    PROP(AMF_VIDEO_ENCODER_CAP_REQUESTED_THROUGHPUT);
    PROP(AMF_VIDEO_ENCODER_CAP_QUERY_TIMEOUT_SUPPORT);
    PROP(AMF_VIDEO_ENCODER_CAP_SUPPORT_SLICE_OUTPUT);
    printf("    assert_wide(codec::VIDEO_ENCODER_VCE_AVC, \"%ls\", \"AMFVideoEncoderVCE_AVC\");\n",
           AMFVideoEncoderVCE_AVC);
    printf("    assert_wide(trace_writer::CONSOLE, \"%ls\", \"AMF_TRACE_WRITER_CONSOLE\");\n",
           AMF_TRACE_WRITER_CONSOLE);
    printf("    assert_wide(trace_writer::DEBUG_OUTPUT, \"%ls\", \"AMF_TRACE_WRITER_DEBUG_OUTPUT\");\n",
           AMF_TRACE_WRITER_DEBUG_OUTPUT);

    puts("");
    printf("    // headers built against AMF_FULL_VERSION %llu (%llu.%llu.%llu.%llu)\n",
           (unsigned long long)AMF_FULL_VERSION,
           (unsigned long long)AMF_GET_MAJOR_VERSION(AMF_FULL_VERSION),
           (unsigned long long)AMF_GET_MINOR_VERSION(AMF_FULL_VERSION),
           (unsigned long long)AMF_GET_SUBMINOR_VERSION(AMF_FULL_VERSION),
           (unsigned long long)AMF_GET_BUILD_VERSION(AMF_FULL_VERSION));
    printf("    assert_eq!(HEADERS_FULL_VERSION, %llu, \"AMF_FULL_VERSION of the vendored headers\");\n",
           (unsigned long long)AMF_FULL_VERSION);
    printf("    assert_eq!(make_full_version(1, 4, 30, 0), %llu, \"AMF_MAKE_FULL_VERSION(1,4,30,0)\");\n",
           (unsigned long long)AMF_MAKE_FULL_VERSION(1, 4, 30, 0));
    return 0;
}
