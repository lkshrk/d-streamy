#if defined(_MSC_VER) && defined(_M_X64) && (_MSC_VER >= 1920)
/* libclang's intrin.h lacks this MSVC intrinsic that util/util_uint64.h calls under the same condition. */
unsigned __int64 _udiv128(unsigned __int64, unsigned __int64, unsigned __int64, unsigned __int64 *);
#endif

#include <obs-module.h>
#include <util/base.h>
#include <util/bmem.h>
