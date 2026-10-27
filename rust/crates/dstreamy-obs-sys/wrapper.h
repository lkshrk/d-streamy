#if defined(_MSC_VER) && defined(_M_X64) && (_MSC_VER >= 1920)
/* libclang's intrin.h lacks this MSVC intrinsic that util/util_uint64.h calls under the same condition. */
unsigned __int64 _udiv128(unsigned __int64, unsigned __int64, unsigned __int64, unsigned __int64 *);
#endif

#include <obs-module.h>
#include <util/base.h>
#include <util/bmem.h>
#include <util/platform.h>

/* blog() takes int, but enum constants are unsigned int under the gcc/clang ABI and int under MSVC. */
static const int DSTREAMY_LOG_ERROR = LOG_ERROR;
static const int DSTREAMY_LOG_WARNING = LOG_WARNING;
static const int DSTREAMY_LOG_INFO = LOG_INFO;
static const int DSTREAMY_LOG_DEBUG = LOG_DEBUG;
