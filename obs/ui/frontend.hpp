#pragma once

class QMainWindow;

namespace dstreamy_ui {

QMainWindow *main_window();
void watch_exit(void (*on_exit)());
void unwatch_exit();
// Logs that `call` threw and the status-bar panel is gone.
void warn(const char *call) noexcept;

} // namespace dstreamy_ui
