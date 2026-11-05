#pragma once

class QMainWindow;

namespace fake {
extern QMainWindow *window;
extern int watches;
extern int unwatches;
void exit();
} // namespace fake
