#pragma once

class QMainWindow;

namespace fake {
extern QMainWindow *window;
extern int watches;
extern int unwatches;
extern int warnings;
void exit();
} // namespace fake
