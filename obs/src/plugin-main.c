#include <obs-module.h>
#include <plugin-support.h>

OBS_DECLARE_MODULE()

bool obs_module_load(void)
{
	obs_log(LOG_INFO, "loaded (version %s)", PLUGIN_VERSION);
	return true;
}
