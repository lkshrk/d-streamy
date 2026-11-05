#pragma once

#include <stdbool.h>
#include <stdint.h>

#ifdef _WIN32
#ifdef DSTREAMY_UI_BUILDING
#define DSTREAMY_UI_EXPORT __declspec(dllexport)
#else
#define DSTREAMY_UI_EXPORT __declspec(dllimport)
#endif
#else
#define DSTREAMY_UI_EXPORT __attribute__((visibility("default")))
#endif

#ifdef __cplusplus
extern "C" {
#endif

#define DSTREAMY_UI_ABI_VERSION 5u

/* Callbacks run on the Qt UI thread; strings are UTF-8, valid only for the call. */
struct dstreamy_ui_host {
	uint32_t abi_version;
	void *ctx;
	/* The panel view as JSON (see panel.rs), or NULL; release it with free_string. */
	char *(*view_json)(void *ctx);
	void (*free_string)(char *s);
	void (*start)(void *ctx);
	void (*stop)(void *ctx);
	/* Loads the server and channel lists; the view shows them loading. */
	void (*load_lists)(void *ctx);
	/* An empty id is the list's "Pick a …" entry. */
	void (*select_guild)(void *ctx, const char *id);
	void (*select_channel)(void *ctx, const char *id);
	void (*set_track)(void *ctx, int track);
	void (*request_take_over)(void *ctx);
	void (*confirm_take_over)(void *ctx, bool confirm);
	void (*set_source)(void *ctx, const char *uuid, bool on);
	void (*panel_closed)(void *ctx);
	/* Stops managing a track Discord doesn't get; no mixer flag changes. */
	void (*release_track)(void *ctx, int track);
	/* Asks the service to open its login window; the view shows the wait. */
	void (*login)(void *ctx);
};

/* DSTREAMY_UI_ABI_VERSION of this library; the plugin checks it before dstreamy_ui_load. */
DSTREAMY_UI_EXPORT uint32_t dstreamy_ui_abi_version(void);

/* Call on the UI thread once the OBS main window exists; the host is copied. */
DSTREAMY_UI_EXPORT bool dstreamy_ui_load(const struct dstreamy_ui_host *host);

/* Opens or raises the panel on the UI thread; true when it is now shown. */
DSTREAMY_UI_EXPORT bool dstreamy_ui_open_panel(void);

/* Never calls the host afterwards. Also happens on its own when OBS exits. */
DSTREAMY_UI_EXPORT void dstreamy_ui_unload(void);

#ifdef __cplusplus
}
#endif
