#include "frontend.hpp"

#include <obs-frontend-api.h>
#include <util/base.h>

#include <QMainWindow>
#include <QWidget>

namespace dstreamy_ui {

namespace {
void (*exit_handler)();

void on_event(enum obs_frontend_event event, void *)
{
	if (event == OBS_FRONTEND_EVENT_EXIT && exit_handler)
		exit_handler();
}
} // namespace

QMainWindow *main_window()
{
	return qobject_cast<QMainWindow *>(static_cast<QWidget *>(obs_frontend_get_main_window()));
}

void watch_exit(void (*on_exit)())
{
	exit_handler = on_exit;
	obs_frontend_add_event_callback(on_event, nullptr);
}

void unwatch_exit()
{
	obs_frontend_remove_event_callback(on_event, nullptr);
	exit_handler = nullptr;
}

void warn(const char *call) noexcept
{
	blog(LOG_WARNING,
	     "[obs-dstreamy] %s threw: the status-bar panel is gone; Tools > D-Streamy opens the settings dialog",
	     call);
}

} // namespace dstreamy_ui
