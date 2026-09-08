#include <cstddef>
#include <cstdio>
#include "core/PropertyStorageEx.h"
#include "core/Variant.h"
int main() {
    printf("C++ sizeof(amf::AMFPropertyInfo)      = %zu\n", sizeof(amf::AMFPropertyInfo));
    printf("C++ offsetof(AMFPropertyInfo, name)   = %zu\n", offsetof(amf::AMFPropertyInfo, name));
    printf("C++ offsetof(AMFPropertyInfo, type)   = %zu\n", offsetof(amf::AMFPropertyInfo, type));
    printf("C++ offsetof(AMFPropertyInfo, access) = %zu\n", offsetof(amf::AMFPropertyInfo, accessType));
    printf("C++ sizeof(AMFVariantStruct)          = %zu\n", sizeof(amf::AMFVariantStruct));
    printf("C++ offsetof(AMFVariantStruct, type)  = %zu\n", offsetof(amf::AMFVariantStruct, type));
    printf("C++ sizeof(amf::AMFVariant)           = %zu\n", sizeof(amf::AMFVariant));
    printf("C++ sizeof(AMFGuid)                   = %zu\n", sizeof(amf::AMFGuid));
    printf("C++ sizeof(AMFSize)/(AMFRate)         = %zu / %zu\n", sizeof(AMFSize), sizeof(AMFRate));
    return 0;
}
