#if defined(_MSC_VER) && defined(_M_X64) && (_MSC_VER >= 1920)
/* libclang's intrin.h lacks this MSVC intrinsic that util/util_uint64.h calls under the same condition. */
unsigned __int64 _udiv128(unsigned __int64, unsigned __int64, unsigned __int64, unsigned __int64 *);
#endif

#include <obs-module.h>
#include <util/base.h>
#include <util/bmem.h>

/* blog() takes int, but enum constants are unsigned int under the gcc/clang ABI and int under MSVC. */
static const int DSTREAMY_LOG_ERROR = LOG_ERROR;
static const int DSTREAMY_LOG_WARNING = LOG_WARNING;
static const int DSTREAMY_LOG_INFO = LOG_INFO;
static const int DSTREAMY_LOG_DEBUG = LOG_DEBUG;

/* obs-frontend-api is resolved at runtime, never linked: a libobs-only host lacks it. */
typedef void (*dstreamy_frontend_cb)(void *private_data);
typedef void (*dstreamy_frontend_add_tools_menu_item)(const char *name, dstreamy_frontend_cb callback,
						      void *private_data);
typedef void (*dstreamy_frontend_open_source_properties)(obs_source_t *source);
/* The event is enum obs_frontend_event, int-sized under every OBS ABI. */
typedef void (*dstreamy_frontend_event_cb)(int event, void *private_data);
typedef void (*dstreamy_frontend_event_callback)(dstreamy_frontend_event_cb callback, void *private_data);
#define DSTREAMY_FRONTEND_EVENT_EXIT 17
#define DSTREAMY_FRONTEND_EVENT_SCENE_COLLECTION_CLEANUP 25
typedef void (*dstreamy_frontend_save_cb)(obs_data_t *save_data, bool saving, void *private_data);
typedef void (*dstreamy_frontend_save_callback)(dstreamy_frontend_save_cb callback, void *private_data);

#if __has_include(<obs-frontend-api.h>)
#include <obs-frontend-api.h>
_Static_assert(__builtin_types_compatible_p(__typeof__(&obs_frontend_add_tools_menu_item),
					    dstreamy_frontend_add_tools_menu_item),
	       "obs_frontend_add_tools_menu_item changed");
_Static_assert(__builtin_types_compatible_p(__typeof__(&obs_frontend_open_source_properties),
					    dstreamy_frontend_open_source_properties),
	       "obs_frontend_open_source_properties changed");
_Static_assert(__builtin_types_compatible_p(obs_frontend_event_cb, void (*)(enum obs_frontend_event, void *)),
	       "obs_frontend_event_cb changed");
_Static_assert(sizeof(enum obs_frontend_event) == sizeof(int), "enum obs_frontend_event is not int-sized");
_Static_assert(OBS_FRONTEND_EVENT_EXIT == DSTREAMY_FRONTEND_EVENT_EXIT, "OBS_FRONTEND_EVENT_EXIT changed");
_Static_assert(__builtin_types_compatible_p(__typeof__(&obs_frontend_add_event_callback),
					    void (*)(obs_frontend_event_cb, void *)),
	       "obs_frontend_add_event_callback changed");
_Static_assert(__builtin_types_compatible_p(__typeof__(&obs_frontend_remove_event_callback),
					    void (*)(obs_frontend_event_cb, void *)),
	       "obs_frontend_remove_event_callback changed");
_Static_assert(OBS_FRONTEND_EVENT_SCENE_COLLECTION_CLEANUP == DSTREAMY_FRONTEND_EVENT_SCENE_COLLECTION_CLEANUP,
	       "OBS_FRONTEND_EVENT_SCENE_COLLECTION_CLEANUP changed");
_Static_assert(__builtin_types_compatible_p(obs_frontend_save_cb, dstreamy_frontend_save_cb),
	       "obs_frontend_save_cb changed");
_Static_assert(__builtin_types_compatible_p(__typeof__(&obs_frontend_add_save_callback),
					    dstreamy_frontend_save_callback),
	       "obs_frontend_add_save_callback changed");
_Static_assert(__builtin_types_compatible_p(__typeof__(&obs_frontend_remove_save_callback),
					    dstreamy_frontend_save_callback),
	       "obs_frontend_remove_save_callback changed");
_Static_assert(__builtin_types_compatible_p(__typeof__(&obs_frontend_add_preload_callback),
					    dstreamy_frontend_save_callback),
	       "obs_frontend_add_preload_callback changed");
_Static_assert(__builtin_types_compatible_p(__typeof__(&obs_frontend_remove_preload_callback),
					    dstreamy_frontend_save_callback),
	       "obs_frontend_remove_preload_callback changed");
#elif defined(DSTREAMY_FRONTEND_ABI_CHECK)
#error "DSTREAMY_FRONTEND_ABI_CHECK is set but obs-frontend-api.h is not on the include path, so the frontend types above go unchecked"
#endif
