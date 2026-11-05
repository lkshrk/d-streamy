#include <stdbool.h>
#include <stdlib.h>
#include <string.h>

#include <obs.h>
#include <util/base.h>

typedef void (*obs_frontend_cb)(void *private_data);
typedef void (*obs_frontend_event_cb)(int event, void *private_data);

// OBS_FRONTEND_EVENT_EXIT in obs-frontend-api.h.
#define FAKE_FRONTEND_EVENT_EXIT 17
#define EVENT_CALLBACKS_MAX 8

static char *item_name;
static obs_frontend_cb item_callback;
static void *item_data;
static int items;
static obs_source_t *opened;
static struct {
	obs_frontend_event_cb callback;
	void *private_data;
} event_callbacks[EVENT_CALLBACKS_MAX];
static size_t event_callback_count;
static bool exited;

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

// Like obs-frontend-api.cpp once OBSBasic cleared the callbacks after the exit event.
static bool callbacks_valid(const char *name)
{
	if (exited)
		blog(LOG_ERROR, "Tried to call %s with no callbacks!", name);
	return !exited;
}

EXPORT void obs_frontend_add_event_callback(obs_frontend_event_cb callback, void *private_data)
{
	if (!callbacks_valid(__func__))
		return;
	for (size_t i = 0; i < event_callback_count; i++)
		if (event_callbacks[i].callback == callback && event_callbacks[i].private_data == private_data)
			return;
	if (event_callback_count < EVENT_CALLBACKS_MAX) {
		event_callbacks[event_callback_count].callback = callback;
		event_callbacks[event_callback_count].private_data = private_data;
		event_callback_count++;
	}
}

EXPORT void obs_frontend_remove_event_callback(obs_frontend_event_cb callback, void *private_data)
{
	if (!callbacks_valid(__func__))
		return;
	for (size_t i = 0; i < event_callback_count; i++) {
		if (event_callbacks[i].callback != callback || event_callbacks[i].private_data != private_data)
			continue;
		memmove(event_callbacks + i, event_callbacks + i + 1,
			(event_callback_count - i - 1) * sizeof(event_callbacks[0]));
		event_callback_count--;
		return;
	}
}

// OBSBasic::closeWindow: the exit event, then the frontend API is gone.
EXPORT size_t fake_frontend_exit(void)
{
	size_t count = event_callback_count;
	for (size_t i = event_callback_count; i > 0; i--)
		event_callbacks[i - 1].callback(FAKE_FRONTEND_EVENT_EXIT, event_callbacks[i - 1].private_data);
	event_callback_count = 0;
	exited = true;
	return count;
}
