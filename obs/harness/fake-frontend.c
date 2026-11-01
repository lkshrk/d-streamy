#include <stdbool.h>
#include <stdlib.h>
#include <string.h>

#include <obs.h>

typedef void (*obs_frontend_cb)(void *private_data);

static char *item_name;
static obs_frontend_cb item_callback;
static void *item_data;
static int items;
static obs_source_t *opened;

EXPORT void obs_frontend_add_tools_menu_item(const char *name, obs_frontend_cb callback, void *private_data)
{
	items++;
	free(item_name);
	item_name = name ? strdup(name) : NULL;
	item_callback = callback;
	item_data = private_data;
}

#ifndef FAKE_FRONTEND_INCOMPLETE
// Holds a reference like OBSBasicProperties' OBSSource member does.
EXPORT void obs_frontend_open_source_properties(obs_source_t *source)
{
	obs_source_release(opened);
	opened = obs_source_get_ref(source);
}
#endif

EXPORT int fake_frontend_tools_items(const char **name)
{
	*name = item_name;
	return items;
}

EXPORT bool fake_frontend_click(void)
{
	if (!item_callback)
		return false;
	item_callback(item_data);
	return true;
}

EXPORT obs_source_t *fake_frontend_take_opened(void)
{
	obs_source_t *source = opened;
	opened = NULL;
	return source;
}
