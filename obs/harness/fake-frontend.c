#include <stdbool.h>
#include <stdlib.h>
#include <string.h>

#include <obs.h>
#include <util/base.h>

typedef void (*obs_frontend_cb)(void *private_data);
typedef void (*obs_frontend_event_cb)(int event, void *private_data);
typedef void (*obs_frontend_save_cb)(obs_data_t *save_data, bool saving, void *private_data);

// OBS_FRONTEND_EVENT_EXIT in obs-frontend-api.h.
#define FAKE_FRONTEND_EVENT_EXIT 17
#define FAKE_FRONTEND_EVENT_SCENE_COLLECTION_CLEANUP 25
#define FAKE_FRONTEND_EVENT_SCRIPTING_SHUTDOWN 36
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
static bool (*close_dialogs)(void);

struct save_callbacks {
	struct {
		obs_frontend_save_cb callback;
		void *private_data;
	} list[EVENT_CALLBACKS_MAX];
	size_t count;
};
static struct save_callbacks save_callbacks, preload_callbacks;

EXPORT void obs_frontend_add_tools_menu_item(const char *name, obs_frontend_cb callback, void *private_data)
{
	items++;
	free(item_name);
	item_name = name ? strdup(name) : NULL;
	item_callback = callback;
	item_data = private_data;
}

// The harness's model of OBSBasic::CloseDialogs and of closing the open properties dialog: false if it stays.
EXPORT void fake_frontend_set_close_dialogs(bool (*close)(void))
{
	close_dialogs = close;
}

#ifndef FAKE_FRONTEND_INCOMPLETE
// OBSBasic::CreatePropertiesWindow: close the open dialog or give up, then hold a reference; the build is queued.
EXPORT void obs_frontend_open_source_properties(obs_source_t *source)
{
	if (close_dialogs && !close_dialogs())
		return;
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

// OBSBasic::closeWindow: saveAll, scripting shutdown, ClearSceneData (CloseDialogs, cleanup), exit, API gone.
EXPORT size_t fake_frontend_exit(void)
{
	obs_data_t *modules = obs_data_create();
	for (size_t i = save_callbacks.count; i > 0; i--)
		save_callbacks.list[i - 1].callback(modules, true, save_callbacks.list[i - 1].private_data);
	obs_data_release(modules);
	for (size_t i = event_callback_count; i > 0; i--)
		event_callbacks[i - 1].callback(FAKE_FRONTEND_EVENT_SCRIPTING_SHUTDOWN,
						event_callbacks[i - 1].private_data);
	if (close_dialogs)
		close_dialogs();
	size_t count = event_callback_count;
	for (size_t i = event_callback_count; i > 0; i--)
		event_callbacks[i - 1].callback(FAKE_FRONTEND_EVENT_SCENE_COLLECTION_CLEANUP,
						event_callbacks[i - 1].private_data);
	for (size_t i = event_callback_count; i > 0; i--)
		event_callbacks[i - 1].callback(FAKE_FRONTEND_EVENT_EXIT, event_callbacks[i - 1].private_data);
	event_callback_count = 0;
	save_callbacks.count = 0;
	preload_callbacks.count = 0;
	exited = true;
	return count;
}

static void add_save_callback(struct save_callbacks *to, obs_frontend_save_cb callback, void *private_data)
{
	if (!callbacks_valid("obs_frontend_add_save_callback"))
		return;
	if (to->count < EVENT_CALLBACKS_MAX) {
		to->list[to->count].callback = callback;
		to->list[to->count].private_data = private_data;
		to->count++;
	}
}

static void remove_save_callback(struct save_callbacks *from, obs_frontend_save_cb callback, void *private_data)
{
	if (!callbacks_valid("obs_frontend_remove_save_callback"))
		return;
	for (size_t i = 0; i < from->count; i++) {
		if (from->list[i].callback != callback || from->list[i].private_data != private_data)
			continue;
		memmove(from->list + i, from->list + i + 1, (from->count - i - 1) * sizeof(from->list[0]));
		from->count--;
		return;
	}
}

EXPORT void obs_frontend_add_save_callback(obs_frontend_save_cb callback, void *private_data)
{
	add_save_callback(&save_callbacks, callback, private_data);
}

EXPORT void obs_frontend_remove_save_callback(obs_frontend_save_cb callback, void *private_data)
{
	remove_save_callback(&save_callbacks, callback, private_data);
}

EXPORT void obs_frontend_add_preload_callback(obs_frontend_save_cb callback, void *private_data)
{
	add_save_callback(&preload_callbacks, callback, private_data);
}

EXPORT void obs_frontend_remove_preload_callback(obs_frontend_save_cb callback, void *private_data)
{
	remove_save_callback(&preload_callbacks, callback, private_data);
}

// OBSStudioAPI::on_save, on_preload and on_load: newest callback first.
static void run_save_callbacks(struct save_callbacks *callbacks, obs_data_t *data, bool saving)
{
	for (size_t i = callbacks->count; i > 0; i--)
		callbacks->list[i - 1].callback(data, saving, callbacks->list[i - 1].private_data);
}

EXPORT void fake_frontend_save(obs_data_t *modules)
{
	run_save_callbacks(&save_callbacks, modules, true);
}

EXPORT void fake_frontend_preload(obs_data_t *modules)
{
	run_save_callbacks(&preload_callbacks, modules, false);
}

EXPORT void fake_frontend_load(obs_data_t *modules)
{
	run_save_callbacks(&save_callbacks, modules, false);
}

// OBSStudioAPI::on_event, without OBSBasic's disableSaving filter.
EXPORT void fake_frontend_event(int event)
{
	for (size_t i = event_callback_count; i > 0; i--)
		event_callbacks[i - 1].callback(event, event_callbacks[i - 1].private_data);
}
