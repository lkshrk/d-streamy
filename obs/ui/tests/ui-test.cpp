#include "../dstreamy-ui.h"
#include "fake-frontend.hpp"

#include <QApplication>
#include <QCheckBox>
#include <QComboBox>
#include <QEventLoop>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QKeyEvent>
#include <QMouseEvent>
#include <QScreen>
#include <QScrollArea>
#include <QLabel>
#include <QMainWindow>
#include <QPalette>
#include <QPushButton>
#include <QStatusBar>
#include <QStyleFactory>
#include <QTimer>
#include <QToolButton>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <tuple>
#include <utility>
#include <vector>

namespace {

int failures;

void check(bool ok, const char *what, int line)
{
	if (!ok) {
		std::fprintf(stderr, "FAIL line %d: %s\n", line, what);
		failures++;
	}
}

#define CHECK(x) check((x), #x, __LINE__)

struct Host {
	std::string json;
	bool null_view = false;
	bool unloaded = false;
	int views = 0;
	int starts = 0;
	int stops = 0;
	int loads = 0;
	int take_overs = 0;
	int closed = 0;
	std::vector<bool> confirms;
	std::vector<int> releases;
	std::string guild = "-";
	std::string channel = "-";
	int track = 0;
	std::vector<std::pair<std::string, bool>> ticks;
	int after_unload = 0;
} host;

void called()
{
	if (host.unloaded)
		host.after_unload++;
}

char *view_json(void *)
{
	called();
	host.views++;
	if (host.null_view)
		return nullptr;
	char *s = static_cast<char *>(std::malloc(host.json.size() + 1));
	std::memcpy(s, host.json.c_str(), host.json.size() + 1);
	return s;
}

void free_string(char *s)
{
	std::free(s);
}

dstreamy_ui_host make_host()
{
	dstreamy_ui_host h{};
	h.abi_version = DSTREAMY_UI_ABI_VERSION;
	h.view_json = view_json;
	h.free_string = free_string;
	h.start = [](void *) {
		called();
		host.starts++;
	};
	h.stop = [](void *) {
		called();
		host.stops++;
	};
	h.load_lists = [](void *) {
		called();
		host.loads++;
	};
	h.select_guild = [](void *, const char *id) {
		called();
		host.guild = id;
	};
	h.select_channel = [](void *, const char *id) {
		called();
		host.channel = id;
	};
	h.set_track = [](void *, int track) {
		called();
		host.track = track;
	};
	h.request_take_over = [](void *) {
		called();
		host.take_overs++;
	};
	h.confirm_take_over = [](void *, bool yes) {
		called();
		host.confirms.push_back(yes);
	};
	h.set_source = [](void *, const char *uuid, bool on) {
		called();
		host.ticks.emplace_back(uuid, on);
	};
	h.panel_closed = [](void *) {
		called();
		host.closed++;
	};
	h.release_track = [](void *, int track) {
		called();
		host.releases.push_back(track);
	};
	return h;
}

using Items = std::vector<std::tuple<const char *, const char *, bool>>;

QJsonObject choices(const char *placeholder, const Items &items, bool loading = false, bool enabled = true)
{
	QJsonArray list;
	for (auto [id, name, selected] : items)
		list.append(QJsonObject{{"id", QString::fromUtf8(id)},
					{"name", QString::fromUtf8(name)},
					{"selected", selected}});
	return QJsonObject{{"placeholder", QString::fromUtf8(placeholder)},
			   {"items", list},
			   {"loading", loading},
			   {"enabled", enabled}};
}

const std::vector<std::string> five = {"Alerts", "Mic", "Music", "Music-Out", "Stream"};

QJsonObject audio(bool managed, bool confirming, int track = 6, const std::vector<std::string> &names = five)
{
	QJsonArray tracks;
	for (int t = 1; t <= 6; t++)
		tracks.append(QJsonObject{{"track", t},
					  {"name", QStringLiteral("Track %1").arg(t)},
					  {"selected", t == track}});
	QJsonArray sources;
	if (managed)
		for (size_t i = 0; i < names.size(); i++)
			sources.append(QJsonObject{{"uuid", QStringLiteral("u%1").arg(i)},
						   {"name", QString::fromStdString(names[i])},
						   {"on", names[i] == "Music"}});
	QJsonObject a{{"title", "Discord audio"},
		      {"track", track},
		      {"tracks", tracks},
		      {"managed", managed},
		      {"line",
		       managed ? QString()
			       : QStringLiteral("Discord hears every source on Track %1. Let D-Streamy manage Track %1 "
						"to pick sources here.")
					 .arg(track)},
		      {"manage", managed ? QString() : QStringLiteral("Manage Track %1").arg(track)},
		      {"sources", sources},
		      {"empty", QJsonValue()}};
	a["confirm"] = confirming
			       ? QJsonValue(QJsonObject{
					 {"text", QStringLiteral("Turns Track %1 off on every audio source, also for "
								 "recordings and streams that use it.")
							  .arg(track)},
					 {"ok", "Manage"},
					 {"cancel", "Not now"}})
			       : QJsonValue();
	return a;
}

struct State {
	const char *tone;
	const char *mark;
	const char *status_bar;
	const char *line;
	const char *fix;
	const char *tooltip;
	const char *button;
	const char *action;
	bool enabled;
};

const State idle{"idle",
		 "●",
		 "D-Streamy idle ▴",
		 "Idle · Kids Lounge 🧃",
		 "",
		 "Idle · Kids Lounge 🧃\nTrack 6",
		 "Start D-Streamy",
		 "start",
		 true};
const State live{"live",
		 "●",
		 "● D-Streamy live ▴",
		 "Live · 6.0 Mbit/s · 60 fps · 12:34",
		 "",
		 "Live · Kids Lounge 🧃\n6.0 Mbit/s · 60 fps · 12:34 · 0 dropped\nTrack 6",
		 "Stop D-Streamy",
		 "stop",
		 true};
const State attention{
	"attention",
	"⚠",
	"⚠ D-Streamy ▴",
	"Service not running",
	"Run `dstreamy service install`",
	"The D-Streamy service is not running (no service is listening at /run/user/1000/dstreamy/serve.sock; "
	"start it with `dstreamy service install`, or run `dstreamy serve`)",
	"Start D-Streamy",
	"start",
	true};

std::string view(const State &s, const QJsonObject &a, bool loading = false)
{
	bool lists = std::strcmp(s.tone, "live") != 0;
	QJsonObject v{
		{"tone", s.tone},
		{"mark", QString::fromUtf8(s.mark)},
		{"status_bar", QString::fromUtf8(s.status_bar)},
		{"status_tooltip", QString::fromUtf8(s.tooltip)},
		{"state_line", QString::fromUtf8(s.line)},
		{"state_fix", QString::fromUtf8(s.fix)},
		{"state_tooltip", QString::fromUtf8(s.tooltip)},
		{"guilds", choices(loading ? "Loading…" : "Pick a server",
				   loading ? Items{} : Items{{"g", "Beste Chatgruppe", true}, {"h", "Other", false}},
				   loading, lists)},
		{"channels", choices("Pick a voice channel", loading ? Items{} : Items{{"c", "Kids Lounge 🧃", true}},
				     loading, lists)},
		{"audio", a},
		{"button",
		 QJsonObject{{"label", QString::fromUtf8(s.button)}, {"action", s.action}, {"enabled", s.enabled}}}};
	return QJsonDocument(v).toJson(QJsonDocument::Compact).toStdString();
}

void spin(int ms)
{
	QEventLoop loop;
	QTimer::singleShot(ms, &loop, &QEventLoop::quit);
	loop.exec();
}

template<typename T> T *find(const char *name)
{
	for (QWidget *top : QApplication::topLevelWidgets())
		if (auto *w = top->findChild<T *>(QString::fromLatin1(name)))
			return w;
	return nullptr;
}

QWidget *panel()
{
	for (QWidget *top : QApplication::topLevelWidgets())
		if (top->objectName() == QLatin1String("dstreamy-panel"))
			return top;
	return nullptr;
}

QString text(const char *name)
{
	auto *l = find<QLabel>(name);
	return l ? l->text() : QStringLiteral("<missing>");
}

bool visible(const char *name)
{
	auto *w = find<QWidget>(name);
	return w && w->isVisible();
}

void show(const std::string &json)
{
	host.json = json;
	spin(650);
}

// With DSTREAMY_UI_SHOTS=DIR, saves the open panel as DIR/panel-NAME.png at its natural size.
void shoot(const char *name)
{
	QByteArray dir = qgetenv("DSTREAMY_UI_SHOTS");
	QWidget *p = panel();
	if (dir.isEmpty() || !p)
		return;
	QString path = QString::fromLocal8Bit(dir) + QStringLiteral("/panel-%1.png").arg(QLatin1String(name));
	if (!p->grab().save(path))
		std::fprintf(stderr, "could not save %s\n", qPrintable(path));
}

QList<QCheckBox *> sources()
{
	QWidget *box = find<QWidget>("dstreamy-sources");
	return box ? box->findChildren<QCheckBox *>() : QList<QCheckBox *>();
}

} // namespace

int main(int argc, char **argv)
{
	QApplication app(argc, argv);
	if (!qgetenv("DSTREAMY_UI_SHOTS").isEmpty()) {
		app.setStyle(QStyleFactory::create(QStringLiteral("Fusion")));
		QPalette dark;
		dark.setColor(QPalette::Window, QColor(0x1d, 0x1f, 0x26));
		dark.setColor(QPalette::WindowText, QColor(0xe4, 0xe4, 0xe7));
		dark.setColor(QPalette::Button, QColor(0x2b, 0x2e, 0x38));
		dark.setColor(QPalette::ButtonText, QColor(0xe4, 0xe4, 0xe7));
		dark.setColor(QPalette::Base, QColor(0x16, 0x18, 0x1d));
		dark.setColor(QPalette::Text, QColor(0xe4, 0xe4, 0xe7));
		app.setPalette(dark);
	}
	QMainWindow window;
	window.setGeometry(0, 0, 800, 600);
	window.statusBar();
	window.show();
	fake::window = &window;

	CHECK(dstreamy_ui_abi_version() == 4);
	dstreamy_ui_host good = make_host();
	CHECK(!dstreamy_ui_load(nullptr));
	dstreamy_ui_host old = good;
	old.abi_version = 3;
	CHECK(!dstreamy_ui_load(&old));
	dstreamy_ui_host partial = good;
	partial.panel_closed = nullptr;
	CHECK(!dstreamy_ui_load(&partial));
	partial = good;
	partial.release_track = nullptr;
	CHECK(!dstreamy_ui_load(&partial));
	fake::window = nullptr;
	CHECK(!dstreamy_ui_load(&good));
	fake::window = &window;
	CHECK(host.views == 0 && !find<QToolButton>("dstreamy-status"));

	host.json = view(idle, audio(false, false), true);
	CHECK(dstreamy_ui_load(&good));
	CHECK(!dstreamy_ui_load(&good));
	CHECK(fake::watches == 1);
	auto *status = window.statusBar()->findChild<QToolButton *>("dstreamy-status");
	CHECK(status && status->text() == QString::fromUtf8("D-Streamy idle ▴"));
	CHECK(status && status->toolTip() == QString::fromUtf8("Idle · Kids Lounge 🧃\nTrack 6"));
	CHECK(panel() && !panel()->isVisible());
	CHECK(host.loads == 0);

	status->click();
	app.processEvents();
	CHECK(panel() && panel()->isVisible());
	CHECK(host.loads == 1);
	auto *guilds = find<QComboBox>("dstreamy-guilds");
	auto *channels = find<QComboBox>("dstreamy-channels");
	auto *tracks = find<QComboBox>("dstreamy-tracks");
	CHECK(guilds && guilds->count() == 1 && guilds->itemText(0) == QString::fromUtf8("Loading…"));
	QRect above = panel() ? panel()->geometry() : QRect();
	QPoint anchor = status->mapToGlobal(QPoint(status->width(), 0));
	if (above.bottom() >= anchor.y() || above.right() != anchor.x() - 1)
		std::fprintf(stderr, "panel %d,%d %dx%d, status corner %d,%d\n", above.x(), above.y(), above.width(),
			     above.height(), anchor.x(), anchor.y());
	CHECK(above.bottom() < anchor.y() && above.right() == anchor.x() - 1);

	show(view(idle, audio(false, false)));
	CHECK(guilds->count() == 3 && guilds->currentText() == QLatin1String("Beste Chatgruppe"));
	CHECK(channels->count() == 2 && channels->currentText() == QString::fromUtf8("Kids Lounge 🧃"));
	CHECK(tracks->count() == 6 && tracks->currentText() == QLatin1String("Track 6"));
	emit guilds->activated(2);
	CHECK(host.guild == "h");
	emit guilds->activated(0);
	CHECK(host.guild.empty());
	emit channels->activated(1);
	CHECK(host.channel == "c");
	emit tracks->activated(2);
	CHECK(host.track == 3);

	CHECK(visible("dstreamy-audio-line") && visible("dstreamy-manage") && !visible("dstreamy-confirm"));
	CHECK(text("dstreamy-audio-line") ==
	      QLatin1String(
		      "Discord hears every source on Track 6. Let D-Streamy manage Track 6 to pick sources here."));
	CHECK(sources().isEmpty() || !visible("dstreamy-sources"));
	CHECK(text("dstreamy-state") == QString::fromUtf8("Idle · Kids Lounge 🧃"));
	CHECK(!visible("dstreamy-fix"));
	shoot("idle-unmanaged");
	find<QPushButton>("dstreamy-manage")->click();
	CHECK(host.take_overs == 1);

	show(view(idle, audio(false, true)));
	CHECK(visible("dstreamy-confirm") && !visible("dstreamy-manage") && !visible("dstreamy-audio-line"));
	CHECK(text("dstreamy-confirm-text").startsWith(QLatin1String("Turns Track 6 off on every audio source")));
	auto *confirm_text = find<QLabel>("dstreamy-confirm-text");
	CHECK(confirm_text->height() >= confirm_text->heightForWidth(confirm_text->width()));
	shoot("confirm");
	find<QPushButton>("dstreamy-confirm-cancel")->click();
	find<QPushButton>("dstreamy-confirm-ok")->click();
	CHECK((host.confirms == std::vector<bool>{false, true}));

	show(view(idle, audio(true, false)));
	QList<QCheckBox *> boxes = sources();
	CHECK(boxes.size() == 5 && visible("dstreamy-sources") && !visible("dstreamy-manage"));
	CHECK(boxes.size() == 5 && boxes[2]->text() == QLatin1String("Music") && boxes[2]->isChecked());
	CHECK(boxes.size() == 5 && boxes[0]->y() == boxes[1]->y() && boxes[0]->x() < boxes[1]->x());
	CHECK(boxes.size() == 5 && boxes[2]->y() > boxes[0]->y() && boxes[4]->x() == boxes[0]->x());
	shoot("managed");
	if (boxes.size() == 5)
		boxes[1]->click();
	CHECK(!host.ticks.empty() && host.ticks.back() == std::make_pair(std::string("u1"), true));
	spin(50);
	CHECK(boxes.size() == 5 && !boxes[1]->isChecked());
	find<QPushButton>("dstreamy-action")->click();
	CHECK(host.starts == 1);

	QJsonObject noted = audio(true, false);
	noted["line"] = QStringLiteral("Track 1 is OBS's streaming and recording track");
	show(view(idle, noted));
	CHECK(visible("dstreamy-audio-line") && visible("dstreamy-sources") && !visible("dstreamy-manage"));

	QJsonObject warned = audio(true, false);
	warned["notices"] =
		QJsonArray{QJsonObject{{"text", "Track 1 is OBS's streaming and recording track"},
				       {"release", QJsonObject{{"track", 1}, {"label", "Stop managing Track 1"}}}}};
	show(view(idle, warned));
	CHECK(visible("dstreamy-notices") && visible("dstreamy-release") && visible("dstreamy-sources"));
	CHECK(text("dstreamy-notice").startsWith(QLatin1String("Track 1 is OBS's")));
	find<QPushButton>("dstreamy-release")->click();
	CHECK((host.releases == std::vector<int>{1}));
	show(view(idle, audio(true, false)));
	CHECK(!visible("dstreamy-notices"));

	QJsonObject instead = audio(false, true);
	QJsonObject row = instead["confirm"].toObject();
	row["instead"] = QJsonObject{{"track", 6}, {"label", "Use Track 6 instead"}};
	instead["confirm"] = row;
	show(view(idle, instead));
	CHECK(visible("dstreamy-confirm-instead"));
	find<QPushButton>("dstreamy-confirm-instead")->click();
	CHECK(host.track == 6);

	QJsonObject no_controls = audio(false, false);
	no_controls["manage"] = QString();
	no_controls["line"] = QStringLiteral("D-Streamy can't change Discord audio on Windows yet.");
	show(view(idle, no_controls));
	CHECK(visible("dstreamy-audio-line") && !visible("dstreamy-manage") && !visible("dstreamy-sources"));

	show(view(live, audio(true, false)));
	CHECK(status->text() == QString::fromUtf8("● D-Streamy live ▴"));
	CHECK(status->toolTip().contains(QString::fromUtf8("6.0 Mbit/s · 60 fps · 12:34 · 0 dropped")));
	CHECK(text("dstreamy-state") == QString::fromUtf8("Live · 6.0 Mbit/s · 60 fps · 12:34"));
	CHECK(!guilds->isEnabled() && !channels->isEnabled());
	CHECK(sources().size() == 5 && sources()[2]->isChecked());
	shoot("live");
	find<QPushButton>("dstreamy-action")->click();
	CHECK(host.stops == 1);

	show(view(attention, audio(true, false)));
	CHECK(text("dstreamy-state") == QLatin1String("Service not running"));
	CHECK(visible("dstreamy-fix") && text("dstreamy-fix") == QLatin1String("Run `dstreamy service install`"));
	CHECK(find<QLabel>("dstreamy-fix")->toolTip().startsWith(QLatin1String("The D-Streamy service is not running")));
	CHECK(status->text() == QString::fromUtf8("⚠ D-Streamy ▴"));
	CHECK(find<QLabel>("dstreamy-mark")->text() == QString::fromUtf8("⚠"));
	shoot("attention");

	State long_state = live;
	long_state.line = "Live · 12.3 Mbit/s · 60 fps · 1:02:05 · 1234 dropped and more text that cannot fit";
	show(view(long_state, audio(true, false)));
	auto *state = find<QLabel>("dstreamy-state");
	QString shown = state ? state->property("shownText").toString() : QString();
	CHECK(state && state->text() == QString::fromUtf8(long_state.line));
	CHECK(!shown.isEmpty() && shown != state->text() && shown.endsWith(QChar(0x2026)));
	CHECK(state && !state->wordWrap() && state->height() < 2 * state->fontMetrics().height());
	CHECK(state && state->toolTip() == QString::fromUtf8(long_state.tooltip));

	host.null_view = true;
	spin(650);
	CHECK(state && state->text() == QString::fromUtf8(long_state.line));
	host.null_view = false;
	host.json = "not json";
	spin(650);
	CHECK(state && state->text() == QString::fromUtf8(long_state.line));

	int closed = host.closed;
	QKeyEvent esc(QEvent::KeyPress, Qt::Key_Escape, Qt::NoModifier);
	QApplication::sendEvent(panel(), &esc);
	app.processEvents();
	CHECK(panel() && !panel()->isVisible());
	CHECK(host.closed == closed + 1);
	status->click();
	app.processEvents();
	CHECK(panel() && panel()->isVisible() && host.loads == 2);
	status->click();
	app.processEvents();
	CHECK(panel() && !panel()->isVisible());

	CHECK(dstreamy_ui_open_panel());
	app.processEvents();
	CHECK(panel() && panel()->isVisible() && host.loads == 3);
	QPoint outside = panel()->mapFromGlobal(window.mapToGlobal(QPoint(10, 10)));
	QMouseEvent click(QEvent::MouseButtonPress, outside, panel()->mapToGlobal(outside), Qt::LeftButton,
			  Qt::LeftButton, Qt::NoModifier);
	QApplication::sendEvent(panel(), &click);
	app.processEvents();
	CHECK(panel() && !panel()->isVisible());

	window.statusBar()->hide();
	app.processEvents();
	CHECK(dstreamy_ui_open_panel());
	app.processEvents();
	QRect inside = QRect(window.mapToGlobal(QPoint(0, 0)), window.size());
	QRect hidden_bar = panel() ? panel()->geometry() : QRect();
	CHECK(panel() && panel()->isVisible() && inside.contains(hidden_bar));
	CHECK(inside.right() - hidden_bar.right() < 20 && inside.bottom() - hidden_bar.bottom() < 20);
	CHECK(dstreamy_ui_open_panel());
	app.processEvents();
	CHECK(panel() && panel()->isVisible() && panel()->geometry() == hidden_bar);
	window.statusBar()->show();
	app.processEvents();
	panel()->hide();
	CHECK(dstreamy_ui_open_panel());
	app.processEvents();
	QPoint corner = status->mapToGlobal(QPoint(status->width(), 0));
	CHECK(panel() && panel()->geometry().bottom() < corner.y() && panel()->geometry().right() == corner.x() - 1);

	std::vector<std::string> many;
	for (int i = 0; i < 20; i++)
		many.push_back(i == 3   ? "Music"
			       : i == 5 ? "Mic & Game"
					: "Source " + std::to_string(i) + " with a very long name from a plugin");
	show(view(idle, audio(true, false, 6, many)));
	QList<QCheckBox *> all = sources();
	auto *scroll = find<QScrollArea>("dstreamy-sources-scroll");
	CHECK(all.size() == 20 && scroll);
	CHECK(all.size() == 20 && all[0]->text().endsWith(QChar(0x2026)) &&
	      all[0]->toolTip() == QString::fromStdString(many[0]) && all[3]->text() == QLatin1String("Music"));
	CHECK(scroll && scroll->height() <= 8 * (all[0]->sizeHint().height() + 8) && scroll->height() > 0);
	CHECK(all.size() == 20 && all[0]->width() <= 160);
	CHECK(all.size() == 20 && all[5]->text() == QLatin1String("Mic && Game") &&
	      all[5]->toolTip() == QLatin1String("Mic & Game"));
	QRect screen = window.screen()->availableGeometry();
	CHECK(panel() && panel()->height() <= screen.height());
	auto *action = find<QPushButton>("dstreamy-action");
	CHECK(action && panel() && QRect(QPoint(0, 0), panel()->size()).contains(action->geometry()));
	shoot("many-sources");
	panel()->hide();

	dstreamy_ui_unload();
	host.unloaded = true;
	CHECK(!dstreamy_ui_open_panel());
	int views = host.views;
	spin(1200);
	CHECK(host.views == views && host.after_unload == 0);
	CHECK(fake::unwatches == 1);
	CHECK(!find<QToolButton>("dstreamy-status") && !panel());
	dstreamy_ui_unload();
	CHECK(fake::unwatches == 1);

	host.unloaded = false;
	host.json = view(idle, audio(false, false));
	CHECK(dstreamy_ui_load(&good));
	CHECK(window.statusBar()->findChild<QToolButton *>("dstreamy-status") != nullptr);
	fake::exit();
	host.unloaded = true;
	views = host.views;
	spin(1200);
	CHECK(host.views == views && host.after_unload == 0);
	CHECK(!find<QToolButton>("dstreamy-status") && !panel());
	dstreamy_ui_unload();
	CHECK(fake::unwatches == 1);

	if (failures) {
		std::fprintf(stderr, "dstreamy-ui-test: %d failures\n", failures);
		return 1;
	}
	std::printf("dstreamy-ui-test: PASS\n");
	return 0;
}
