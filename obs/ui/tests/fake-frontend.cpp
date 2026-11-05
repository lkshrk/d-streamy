#include "fake-frontend.hpp"

#include "../frontend.hpp"

#include <QMainWindow>

namespace fake {
QMainWindow *window;
int watches;
int unwatches;
void (*exit_handler)();

void exit()
{
	if (exit_handler)
		exit_handler();
}
} // namespace fake

namespace dstreamy_ui {

QMainWindow *main_window()
{
	return fake::window;
}

void watch_exit(void (*on_exit)())
{
	fake::exit_handler = on_exit;
	fake::watches++;
}

void unwatch_exit()
{
	fake::exit_handler = nullptr;
	fake::unwatches++;
}

} // namespace dstreamy_ui
