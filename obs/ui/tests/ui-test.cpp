#include "../dstreamy-ui.h"
#include "fake-frontend.hpp"

#include <QApplication>
#include <QCheckBox>
#include <QComboBox>
#include <QElapsedTimer>
#include <QEventLoop>
#include <QFrame>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QKeyEvent>
#include <QMouseEvent>
#include <QScreen>
#include <QScrollArea>
#include <QLabel>
#include <QLayout>
#include <QMainWindow>
#include <QMessageBox>
#include <QPalette>
#include <QPointer>
#include <QPushButton>
#include <QStatusBar>
#include <QStyleFactory>
#include <QTextDocument>
#include <QTimer>
#include <QToolButton>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <new>
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
	int logins = 0;
	int cancels = 0;
	int adds = 0;
	std::vector<std::string> selects;
	std::vector<std::string> logouts;
	std::vector<bool> logout_answers;
	std::vector<bool> confirms;
	std::vector<int> releases;
	std::string guild = "-";
	std::string channel = "-";
	int track = 0;
	std::vector<std::pair<std::string, bool>> ticks;
	int after_unload = 0;
	bool throw_view = false;
	bool throw_load = false;
	bool throw_track = false;
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
	if (host.throw_view)
		throw std::bad_alloc();
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
		if (host.throw_load)
			throw std::bad_alloc();
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
		if (host.throw_track)
			throw std::bad_alloc();
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
	h.login = [](void *) {
		called();
		host.logins++;
	};
	h.cancel_login = [](void *) {
		called();
		host.cancels++;
	};
	h.select_account = [](void *, const char *name) {
		called();
		host.selects.emplace_back(name);
	};
	h.add_account = [](void *) {
		called();
		host.adds++;
	};
	h.logout_account = [](void *, const char *name) {
		called();
		host.logouts.emplace_back(name);
	};
	h.confirm_logout = [](void *, bool yes) {
		called();
		host.logout_answers.push_back(yes);
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
	const char *dot;
	const char *line;
	const char *fix;
	const char *tooltip;
	const char *button;
	const char *action;
	bool enabled;
};

const State idle{"idle",
		 "●",
		 "idle",
		 "Idle · Kids Lounge 🧃",
		 "",
		 "Idle · Kids Lounge 🧃\nTrack 6",
		 "Start D-Streamy",
		 "start",
		 true};
const State live{"live",
		 "●",
		 "live",
		 "Live · 6.0 Mbit/s · 60 fps · 12:34",
		 "",
		 "Live · Kids Lounge 🧃\n6.0 Mbit/s · 60 fps · 12:34 · 0 dropped\nTrack 6",
		 "Stop D-Streamy",
		 "stop",
		 true};
const State attention{
	"attention",
	"⚠",
	"error",
	"Service not running",
	"Run `dstreamy service install`",
	"The D-Streamy service is not running (no service is listening at /run/user/1000/dstreamy/serve.sock; "
	"start it with `dstreamy service install`, or run `dstreamy serve`)",
	"Start D-Streamy",
	"start",
	true};

QJsonObject account_row(bool enabled, const char *tooltip = "", const char *logout_tooltip = "")
{
	return QJsonObject{
		{"label", "Account"},
		{"items", QJsonArray{QJsonObject{{"name", "h_cloud_streamy"}, {"user", "Streamy"}, {"selected", true}},
				     QJsonObject{{"name", "burner"}, {"user", ""}, {"selected", false}}}},
		{"add", QString::fromUtf8("Add account…")},
		{"logout", "Log out"},
		{"enabled", enabled},
		{"tooltip", tooltip},
		{"logout_tooltip", logout_tooltip}};
}

QJsonObject counter(const char *id, const char *icon, const char *text, const char *level = "normal")
{
	return QJsonObject{{"id", id},
			   {"icon", icon},
			   {"text", QString::fromUtf8(text)},
			   {"level", level},
			   {"label", QStringLiteral("%1 %2").arg(QLatin1String(id), QString::fromUtf8(text))}};
}

// The status bar of a healthy stream, or dashes while not live.
QJsonObject status_of(const State &s)
{
	bool live = std::strcmp(s.dot, "live") == 0;
	const char *dash = "–";
	return QJsonObject{{"state", s.dot},
			   {"counters", QJsonArray{counter("missed", "missed", live ? "0" : dash),
						   counter("audio", "audio", live ? "0" : dash),
						   counter("reconnects", "reconnects", live ? "0" : dash),
						   counter("viewers", "viewers", live ? "3" : dash)}},
			   {"duration", live ? "12:34" : ""},
			   {"tooltip", QString::fromUtf8(s.tooltip)}};
}

// Replaces the next views' status bar while set.
QJsonObject status_override;

const char *const go_live_question = "Go live in Kids Lounge 🧃 on Beste Chatgruppe?";
// Replaces Start's question in the next views while set.
const char *question_override = nullptr;

QJsonObject control_of(const State &s)
{
	bool stop = std::strcmp(s.action, "stop") == 0;
	QString reason = QString::fromUtf8(s.line);
	if (*s.fix)
		reason += QLatin1Char('\n') + QString::fromUtf8(s.fix);
	return QJsonObject{{"action", s.action},
			   {"icon", stop ? "player-stop" : "player-play"},
			   {"enabled", s.enabled},
			   {"tooltip", s.enabled ? QString::fromUtf8(s.button) : reason},
			   {"confirm",
			    QJsonObject{{"text", stop ? QStringLiteral("Stop the Discord stream?")
						      : QString::fromUtf8(question_override ? question_override
											    : go_live_question)},
					{"ok", stop ? "Stop" : "Go live"},
					{"cancel", "Cancel"},
					{"default_ok", !stop}}}};
}

std::string view(const State &s, const QJsonObject &a, bool loading = false, const char *login = nullptr,
		 const char *login_action = "login", const QJsonValue &account = QJsonValue())
{
	bool lists = std::strcmp(s.tone, "live") != 0;
	QJsonObject v{
		{"tone", s.tone},
		{"mark", QString::fromUtf8(s.mark)},
		{"status", status_override.isEmpty() ? status_of(s) : status_override},
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
		 QJsonObject{{"label", QString::fromUtf8(s.button)}, {"action", s.action}, {"enabled", s.enabled}}},
		{"control", control_of(s)},
		{"login", login ? QJsonValue(QJsonObject{{"label", QString::fromUtf8(login)}, {"action", login_action}})
				: QJsonValue()},
		{"account", account}};
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

// Waits for `done` on a bounded deadline; runners can stall for hundreds of milliseconds.
template<typename F> bool wait_for(F done, int ms = 10000)
{
	QElapsedTimer clock;
	clock.start();
	while (!done()) {
		if (clock.elapsed() > ms)
			return false;
		spin(10);
	}
	return true;
}

// Waits until the panel has asked for the view again, so a poll has rendered whatever it got.
bool polled()
{
	int views = host.views;
	return wait_for([views] { return host.views > views; });
}

void show(const std::string &json)
{
	host.json = json;
	CHECK(polled());
}

// Tooltips are set as escaped rich text; this is what they show.
QString plain(const QString &tooltip)
{
	QTextDocument doc;
	doc.setHtml(tooltip);
	return doc.toPlainText();
}

// The widgets Tab visits from `first`, in order, until it comes back.
QStringList tab_chain(QWidget *first)
{
	QStringList names;
	QWidget *w = first;
	do {
		if (w->isVisible() && (w->focusPolicy() & Qt::TabFocus))
			names << (qobject_cast<QCheckBox *>(w) ? QStringLiteral("box:") + w->property("name").toString()
							       : w->objectName());
		w = w->nextInFocusChain();
	} while (w && w != first);
	return names;
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

// With DSTREAMY_UI_SHOTS=DIR, saves the status bar as DIR/status-NAME.png.
void shoot_status(const char *name)
{
	QByteArray dir = qgetenv("DSTREAMY_UI_SHOTS");
	auto *bar = fake::window ? fake::window->statusBar() : nullptr;
	if (dir.isEmpty() || !bar)
		return;
	QString path = QString::fromLocal8Bit(dir) + QStringLiteral("/status-%1.png").arg(QLatin1String(name));
	if (!bar->grab().save(path))
		std::fprintf(stderr, "could not save %s\n", qPrintable(path));
}

QToolButton *status_button()
{
	return find<QToolButton>("dstreamy-status");
}

QToolButton *status_action()
{
	return find<QToolButton>("dstreamy-status-action");
}

// The open Start/Stop confirm; one closed by an answer waits for its deferred delete, hidden.
QMessageBox *confirm_box()
{
	QMessageBox *open = nullptr;
	for (QWidget *top : QApplication::topLevelWidgets())
		if (top->objectName() == QLatin1String("dstreamy-stream-confirm") && top->isVisible()) {
			CHECK(!open);
			open = qobject_cast<QMessageBox *>(top);
		}
	return open;
}

QPushButton *confirm_button(const char *which)
{
	QMessageBox *box = confirm_box();
	return box ? box->findChild<QPushButton *>(
			     QStringLiteral("dstreamy-stream-confirm-%1").arg(QLatin1String(which)))
		   : nullptr;
}

void answer(const char *which)
{
	QPushButton *b = confirm_button(which);
	CHECK(b != nullptr);
	if (b)
		b->click();
	CHECK(wait_for([] { return !confirm_box(); }));
}

// A real press and release, not QAbstractButton::click, which a disabled button ignores.
void mouse_click(QWidget *w)
{
	QPoint at = w->rect().center();
	QMouseEvent press(QEvent::MouseButtonPress, at, w->mapToGlobal(at), Qt::LeftButton, Qt::LeftButton,
			  Qt::NoModifier);
	QMouseEvent release(QEvent::MouseButtonRelease, at, w->mapToGlobal(at), Qt::LeftButton, Qt::NoButton,
			    Qt::NoModifier);
	QApplication::sendEvent(w, &press);
	QApplication::sendEvent(w, &release);
	QApplication::processEvents();
}

void key(QWidget *w, Qt::Key k)
{
	QKeyEvent press(QEvent::KeyPress, k, Qt::NoModifier);
	QApplication::sendEvent(w, &press);
	QKeyEvent release(QEvent::KeyRelease, k, Qt::NoModifier);
	QApplication::sendEvent(w, &release);
	QApplication::processEvents();
}

// With DSTREAMY_UI_SHOTS=DIR, saves the confirm as DIR/confirm-NAME.png.
void shoot_confirm(const char *name)
{
	QByteArray dir = qgetenv("DSTREAMY_UI_SHOTS");
	QMessageBox *box = confirm_box();
	if (dir.isEmpty() || !box)
		return;
	QString path = QString::fromLocal8Bit(dir) + QStringLiteral("/confirm-%1.png").arg(QLatin1String(name));
	if (!box->grab().save(path))
		std::fprintf(stderr, "could not save %s\n", qPrintable(path));
}

// The status button's shown parts in layout order: "dot:STATE", then each icon's name and text, then the duration.
QStringList status_parts()
{
	QStringList parts;
	QToolButton *b = status_button();
	if (!b || !b->layout())
		return parts;
	for (int i = 0; i < b->layout()->count(); i++) {
		auto *l = qobject_cast<QLabel *>(b->layout()->itemAt(i)->widget());
		if (!l || !l->isVisibleTo(b))
			continue;
		QString name = l->objectName();
		if (name == QLatin1String("dstreamy-status-dot"))
			parts << QStringLiteral("dot:") + l->property("state").toString();
		else if (name.startsWith(QLatin1String("dstreamy-status-icon-")))
			parts << l->property("icon").toString();
		else
			parts << l->text();
	}
	return parts;
}

QLabel *status_part(const char *kind, const char *id)
{
	QToolButton *b = status_button();
	return b ? b->findChild<QLabel *>(QStringLiteral("dstreamy-status-%1-%2").arg(QLatin1String(kind), QLatin1String(id)))
		 : nullptr;
}

QString color_of(QLabel *l)
{
	return l ? l->palette().color(l->foregroundRole()).name() : QString();
}

void check_status_icons(QToolButton *status)
{
	int height = status->fontMetrics().height();
	for (const char *id : {"missed", "audio", "reconnects", "viewers"}) {
		QLabel *icon = status_part("icon", id);
		CHECK(icon && !icon->pixmap().isNull());
		CHECK(icon && icon->pixmap().deviceIndependentSize().toSize() == QSize(height, height));
		CHECK(icon && icon->pixmap().devicePixelRatio() == status->devicePixelRatioF());
		CHECK(icon && icon->testAttribute(Qt::WA_TransparentForMouseEvents));
		QLabel *text = status_part("text", id);
		CHECK(icon && text && QColor(icon->property("color").toString()) == text->palette().color(text->foregroundRole()));
	}
	auto *dot = status->findChild<QLabel *>("dstreamy-status-dot");
	CHECK(dot && !dot->pixmap().isNull());
	QImage image = status_part("icon", "missed")->pixmap().toImage();
	bool inked = false;
	for (int y = 0; y < image.height(); y++)
		for (int x = 0; x < image.width(); x++)
			if (qAlpha(image.pixel(x, y)) > 128)
				inked = true;
	CHECK(inked);
}

// Amber and red counters take the theme's text-warning and text-danger colours, or D-Streamy's own without them.
void check_problem_status(QApplication &app, QToolButton *status)
{
	QColor plain_color = status_part("text", "missed")->palette().color(QPalette::WindowText);
	status_override = QJsonObject{
		{"state", "degraded"},
		{"counters",
		 QJsonArray{counter("missed", "missed", "5", "warn"), counter("audio", "audio-lost", "2", "error"),
			    counter("reconnects", "reconnects", "1", "warn"), counter("viewers", "viewers", "–")}},
		{"duration", "1:02:05"},
		{"tooltip",
		 "Live · Kids Lounge\nMissed frames: 5, 5 in the last 10 seconds\nAudio dropouts: 2, 1 in the last 10 "
		 "seconds, audio lost (only 10 of 50 frames a second reach Discord)\nReconnects: 1\nViewers: "
		 "unknown\nDuration: 1:02:05"}};
	show(view(live, audio(true, false)));
	CHECK(status_parts() == (QStringList{"dot:degraded", "missed", "5", "audio-lost", "2", "reconnects", "1",
					     "viewers", "–", "1:02:05"}));
	CHECK(color_of(status_part("text", "missed")) == QLatin1String("#b88a16"));
	CHECK(color_of(status_part("text", "audio")) == QLatin1String("#c01c37"));
	CHECK(color_of(status_part("text", "reconnects")) == QLatin1String("#b88a16"));
	CHECK(status_part("text", "viewers")->palette().color(QPalette::WindowText) == plain_color);
	CHECK(status_part("icon", "audio")->property("level").toString() == QLatin1String("error"));
	CHECK(QColor(status_part("icon", "audio")->property("color").toString()).name() == QLatin1String("#c01c37"));
	CHECK(QColor(status_button()->findChild<QLabel *>("dstreamy-status-dot")->property("color").toString()).name() ==
	      QLatin1String("#b88a16"));
	QString tip = plain(status->toolTip());
	CHECK(tip.contains(QLatin1String("Audio dropouts: 2")) && tip.contains(QLatin1String("audio lost")));
	CHECK(status->accessibleDescription().contains(QLatin1String("Missed frames: 5")));
	check_status_icons(status);
	shoot_status("problem");

	QString sheet = app.styleSheet();
	app.setStyleSheet(sheet + QStringLiteral(" .text-warning { color: #0a7cff; } .text-success { color: #00aa44; }"));
	CHECK(wait_for([] { return color_of(status_part("text", "missed")) == QLatin1String("#0a7cff"); }));
	CHECK(QColor(status_part("icon", "missed")->property("color").toString()).name() == QLatin1String("#0a7cff"));
	CHECK(color_of(status_part("text", "audio")) == QLatin1String("#c01c37"));
	status_override = QJsonObject();
	show(view(live, audio(true, false)));
	CHECK(QColor(status_button()->findChild<QLabel *>("dstreamy-status-dot")->property("color").toString()).name() ==
	      QLatin1String("#00aa44"));
	CHECK(status_part("text", "missed")->palette().color(QPalette::WindowText) == plain_color);
	app.setStyleSheet(sheet);
	show(view(live, audio(true, false)));

	status_override = QJsonObject{
		{"state", "degraded"},
		{"counters", QJsonArray{counter("missed", "missed", "0"), counter("audio", "audio-silent", "0", "warn"),
					counter("reconnects", "reconnects", "0"), counter("viewers", "viewers", "3")}},
		{"duration", "12:34"},
		{"tooltip",
		 "Live · Kids Lounge\nMissed frames: 0\nAudio dropouts: 0, no source feeds Track 6\nReconnects: "
		 "0\nViewers: 3\nDuration: 12:34"}};
	show(view(live, audio(true, false)));
	CHECK(status_parts() == (QStringList{"dot:degraded", "missed", "0", "audio-silent", "0", "reconnects", "0",
					     "viewers", "3", "12:34"}));
	CHECK(color_of(status_part("text", "audio")) == QLatin1String("#b88a16"));
	CHECK(QColor(status_part("icon", "audio")->property("color").toString()).name() == QLatin1String("#b88a16"));
	CHECK(status_part("text", "missed")->palette().color(QPalette::WindowText) == plain_color);
	CHECK(plain(status->toolTip()).contains(QLatin1String("no source feeds Track 6")));
	check_status_icons(status);
	shoot_status("audio-silent");
	status_override = QJsonObject();
	show(view(live, audio(true, false)));
}

// The status bar's Start/Stop: its icon and reason, the confirm it asks, and that it never opens the panel.
void check_status_action(QToolButton *status)
{
	show(view(idle, audio(true, false)));
	QToolButton *act = status_action();
	CHECK(act && act->isVisible() && act->isEnabled());
	if (!act)
		return;
	int height = status->fontMetrics().height();
	CHECK(act->property("iconName").toString() == QLatin1String("player-play"));
	CHECK(!act->icon().isNull() && act->iconSize() == QSize(height * 4 / 5, height * 4 / 5));
	CHECK(act->styleSheet().contains(QLatin1String("border-radius")));
	auto *divider = status->findChild<QFrame *>("dstreamy-status-divider");
	CHECK(divider && divider->isVisible() && divider->frameShape() == QFrame::VLine);
	QColor play = QColor(act->property("color").toString());
	CHECK(play.green() > play.red() && play.green() > play.blue());
	CHECK(plain(act->toolTip()) == QLatin1String("Start D-Streamy"));
	auto *row = status->layout();
	int at = row->indexOf(act);
	CHECK(at > row->indexOf(status_part("text", "viewers")) &&
	      at < row->indexOf(status->findChild<QLabel *>("dstreamy-status-duration")));
	CHECK(panel() && !panel()->isVisible());
	shoot_status("idle");

	int starts = host.starts, stops = host.stops, closed = host.closed, loads = host.loads;
	act->click();
	QMessageBox *box = confirm_box();
	CHECK(box && box->text() == QString::fromUtf8(go_live_question));
	CHECK(box && box->parentWidget() == fake::window && box->windowModality() != Qt::NonModal);
	CHECK(box && box->defaultButton() == confirm_button("ok"));
	CHECK(confirm_button("ok") && confirm_button("ok")->text() == QLatin1String("Go live"));
	CHECK(confirm_button("cancel") && confirm_button("cancel")->text() == QLatin1String("Cancel"));
	CHECK(panel() && !panel()->isVisible() && host.closed == closed);
	shoot_confirm("start");
	act->click();
	CHECK(confirm_box() == box);
	if (box)
		key(box, Qt::Key_Escape);
	CHECK(wait_for([] { return !confirm_box(); }));
	CHECK(host.starts == starts);
	mouse_click(act);
	CHECK(confirm_box() && panel() && !panel()->isVisible());
	if (confirm_box())
		key(confirm_box(), Qt::Key_Return);
	CHECK(wait_for([] { return !confirm_box(); }));
	CHECK(host.starts == starts + 1 && host.loads == loads && host.closed == closed);
	act->click();
	answer("cancel");
	CHECK(host.starts == starts + 1);
	act->click();
	QPointer<QPushButton> ok = confirm_button("ok");
	CHECK(ok != nullptr);
	if (ok)
		ok->click();
	if (ok)
		ok->click();
	CHECK(wait_for([] { return !confirm_box(); }));
	CHECK(host.starts == starts + 2);

	act->click();
	CHECK(confirm_box() != nullptr);
	question_override = "Go live in Music on Beste Chatgruppe?";
	show(view(idle, audio(true, false)));
	CHECK(wait_for([] { return !confirm_box(); }));
	CHECK(host.starts == starts + 2);
	act->click();
	CHECK(confirm_box() && confirm_box()->text() == QLatin1String("Go live in Music on Beste Chatgruppe?"));
	question_override = nullptr;
	show(view(idle, audio(true, false)));
	CHECK(wait_for([] { return !confirm_box(); }));
	CHECK(host.starts == starts + 2);

	act->click();
	CHECK(confirm_box() != nullptr);
	show(view(live, audio(true, false)));
	CHECK(wait_for([] { return !confirm_box(); }));
	CHECK(host.starts == starts + 2 && host.stops == stops);
	CHECK(act->property("iconName").toString() == QLatin1String("player-stop") && act->isEnabled());
	QColor stop = QColor(act->property("color").toString());
	CHECK(stop.red() > stop.green() && stop.red() > stop.blue());
	CHECK(plain(act->toolTip()) == QLatin1String("Stop D-Streamy"));
	act->click();
	CHECK(confirm_box() && confirm_box()->text() == QLatin1String("Stop the Discord stream?"));
	CHECK(confirm_box() && confirm_box()->defaultButton() == confirm_button("cancel"));
	CHECK(confirm_button("ok") && confirm_button("ok")->text() == QLatin1String("Stop"));
	shoot_confirm("stop");
	if (confirm_box())
		key(confirm_box(), Qt::Key_Return);
	CHECK(wait_for([] { return !confirm_box(); }));
	CHECK(host.stops == stops);
	act->click();
	answer("ok");
	CHECK(host.stops == stops + 1);

	act->click();
	QMessageBox *stop_box = confirm_box();
	CHECK(stop_box != nullptr);
	for (int i = 0; i < 3; i++) {
		status_override = status_of(live);
		status_override["duration"] = QStringLiteral("12:3%1").arg(5 + i);
		status_override["tooltip"] = QStringLiteral("Live · Kids Lounge\nDuration: 12:3%1").arg(5 + i);
		QJsonArray counters = status_override["counters"].toArray();
		counters[3] = counter("viewers", "viewers", i % 2 ? "4" : "5");
		status_override["counters"] = counters;
		show(view(live, audio(true, false)));
		CHECK(confirm_box() == stop_box);
	}
	status_override = QJsonObject();
	show(view(live, audio(true, false)));
	CHECK(confirm_box() == stop_box);
	answer("ok");
	CHECK(host.stops == stops + 2);

	const State waiting{"busy",
			    "●",
			    "busy",
			    "Logging in…",
			    "Waiting for login in the browser window…",
			    "Logging in…\nWaiting for login in the browser window…",
			    "Start D-Streamy",
			    "start",
			    false};
	show(view(waiting, audio(true, false)));
	CHECK(!act->isEnabled() && act->property("iconName").toString() == QLatin1String("player-play"));
	CHECK(plain(act->toolTip()) == QString::fromUtf8("Logging in…\nWaiting for login in the browser window…"));
	act->click();
	mouse_click(act);
	CHECK(!confirm_box() && panel() && !panel()->isVisible());
	CHECK(host.starts == starts + 2 && host.loads == loads && host.closed == closed);
	shoot_status("disabled");

	show(view(idle, audio(true, false)));
	CHECK(dstreamy_ui_open_panel());
	QApplication::processEvents();
	closed = host.closed;
	find<QPushButton>("dstreamy-action")->click();
	CHECK(confirm_box() && panel() && !panel()->isVisible() && host.closed == closed + 1);
	answer("cancel");
	CHECK(host.starts == starts + 2);
}

// The panel's size when it is shown, before anything can move or resize it again.
struct ShowSize : QObject {
	QSize at_show;
	bool eventFilter(QObject *watched, QEvent *event) override
	{
		if (event->type() == QEvent::Show)
			at_show = static_cast<QWidget *>(watched)->size();
		return false;
	}
};

QList<QCheckBox *> sources()
{
	QWidget *box = find<QWidget>("dstreamy-sources");
	return box ? box->findChildren<QCheckBox *>() : QList<QCheckBox *>();
}

// What a failed row check saw, so a CI failure explains itself.
void describe_rows(const char *why)
{
	auto *scroll = find<QScrollArea>("dstreamy-sources-scroll");
	const QList<QCheckBox *> boxes = sources();
	QFont font = boxes.isEmpty() ? QApplication::font() : boxes.first()->font();
	std::fprintf(stderr, "rows (%s): style=%s font=%s %.1fpt/%dpx panel=%d widget=%d viewport=%d\n", why,
		     qPrintable(QApplication::style()->name()), qPrintable(font.family()), font.pointSizeF(),
		     font.pixelSize(), panel() ? panel()->width() : -1, scroll ? scroll->widget()->width() : -1,
		     scroll ? scroll->viewport()->width() : -1);
	for (QCheckBox *box : boxes)
		std::fprintf(stderr, "  %-40s width=%d hint=%d right=%d\n", qPrintable(box->text()), box->width(),
			     box->sizeHint().width(),
			     scroll ? box->mapTo(scroll->viewport(), QPoint(box->width(), 0)).x() : -1);
}

// Every source row lies inside the scroll area's viewport once the layout has settled: nothing scrolls sideways.
bool rows_fit()
{
	auto *scroll = find<QScrollArea>("dstreamy-sources-scroll");
	const QList<QCheckBox *> boxes = sources();
	if (!scroll || boxes.isEmpty() || scroll->widget()->width() > scroll->viewport()->width())
		return false;
	for (QCheckBox *box : boxes)
		if (box->mapTo(scroll->viewport(), QPoint(box->width(), 0)).x() > scroll->viewport()->width())
			return false;
	return true;
}

bool rows_settle()
{
	bool settled = wait_for(rows_fit);
	if (!settled)
		describe_rows("a row is outside the viewport");
	return settled;
}

// A row shows `name`, or a prefix of it ending in an ellipsis when the font is too wide; '&' is doubled for Qt.
bool shows(QCheckBox *box, const QString &name)
{
	QString text = box->text().replace(QLatin1String("&&"), QLatin1String("&"));
	if (text == name)
		return box->accessibleDescription().isEmpty();
	bool elided = text.endsWith(QChar(0x2026)) && name.startsWith(text.chopped(1));
	if (!elided)
		describe_rows(qPrintable(QLatin1String("not shown as ") + name));
	return elided && box->accessibleDescription() == name;
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
	// A theme that changes sizes and fonts when it polishes the panel, as OBS's do.
	app.setStyleSheet(QStringLiteral(
		"#dstreamy-panel { border: 8px solid gray; } #dstreamy-panel QCheckBox { font-size: 19px; }"));
	QMainWindow window;
	window.setGeometry(0, 0, 800, 600);
	window.statusBar();
	window.show();
	fake::window = &window;

	CHECK(dstreamy_ui_abi_version() == 6);
	dstreamy_ui_host good = make_host();
	CHECK(!dstreamy_ui_load(nullptr));
	dstreamy_ui_host old = good;
	old.abi_version = 5;
	CHECK(!dstreamy_ui_load(&old));
	old.abi_version = 4;
	CHECK(!dstreamy_ui_load(&old));
	dstreamy_ui_host partial = good;
	partial.panel_closed = nullptr;
	CHECK(!dstreamy_ui_load(&partial));
	partial = good;
	partial.release_track = nullptr;
	CHECK(!dstreamy_ui_load(&partial));
	partial = good;
	partial.login = nullptr;
	CHECK(!dstreamy_ui_load(&partial));
	partial = good;
	partial.cancel_login = nullptr;
	CHECK(!dstreamy_ui_load(&partial));
	partial = good;
	partial.select_account = nullptr;
	CHECK(!dstreamy_ui_load(&partial));
	partial = good;
	partial.add_account = nullptr;
	CHECK(!dstreamy_ui_load(&partial));
	partial = good;
	partial.logout_account = nullptr;
	CHECK(!dstreamy_ui_load(&partial));
	partial = good;
	partial.confirm_logout = nullptr;
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
	CHECK(status && status->text().isEmpty());
	CHECK(status && status->accessibleName() == QLatin1String("D-Streamy"));
	CHECK(status_parts() == (QStringList{"dot:idle", "missed", "–", "audio", "–", "reconnects", "–", "viewers", "–"}));
	shoot_status("idle");
	CHECK(status && plain(status->toolTip()) == QString::fromUtf8("Idle · Kids Lounge 🧃\nTrack 6"));
	CHECK(panel() && !panel()->isVisible());
	CHECK(host.loads == 0);

	ShowSize show_size;
	if (panel())
		panel()->installEventFilter(&show_size);
	status->click();
	app.processEvents();
	CHECK(panel() && panel()->isVisible());
	CHECK(host.loads == 1);
	CHECK(panel() && show_size.at_show == panel()->size());
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
	CHECK(tracks->accessibleName() == QLatin1String("Discord audio"));
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
	CHECK(!visible("dstreamy-fix") && !visible("dstreamy-login"));
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
	CHECK(wait_for(
		[&] { return boxes.size() == 5 && boxes[0]->y() == boxes[1]->y() && boxes[0]->x() < boxes[1]->x(); }));
	CHECK(wait_for(
		[&] { return boxes.size() == 5 && boxes[2]->y() > boxes[0]->y() && boxes[4]->x() == boxes[0]->x(); }));
	if (boxes.size() == 5 && !(boxes[0]->x() < boxes[1]->x()))
		std::fprintf(stderr, "grid: box0 %d,%d box1 %d,%d box2 %d,%d sources %dx%d\n", boxes[0]->x(),
			     boxes[0]->y(), boxes[1]->x(), boxes[1]->y(), boxes[2]->x(), boxes[2]->y(),
			     find<QWidget>("dstreamy-sources")->width(), find<QWidget>("dstreamy-sources")->height());
	shoot("managed");
	if (boxes.size() == 5)
		boxes[1]->click();
	CHECK(!host.ticks.empty() && host.ticks.back() == std::make_pair(std::string("u1"), true));
	CHECK(boxes.size() == 5 && wait_for([&] { return !boxes[1]->isChecked(); }));
	find<QPushButton>("dstreamy-action")->click();
	CHECK(host.starts == 0 && confirm_box() && !panel()->isVisible());
	CHECK(confirm_box() && confirm_box()->text() == QString::fromUtf8(go_live_question));
	answer("ok");
	CHECK(host.starts == 1);
	CHECK(dstreamy_ui_open_panel());
	app.processEvents();

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
	CHECK((tab_chain(guilds) == QStringList{"dstreamy-guilds", "dstreamy-channels", "dstreamy-tracks",
						"dstreamy-release", "box:Alerts", "box:Mic", "box:Music",
						"box:Music-Out", "box:Stream", "dstreamy-action"}));
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
	CHECK((tab_chain(guilds) == QStringList{"dstreamy-guilds", "dstreamy-channels", "dstreamy-tracks",
						"dstreamy-confirm-cancel", "dstreamy-confirm-instead",
						"dstreamy-confirm-ok", "dstreamy-action"}));
	find<QPushButton>("dstreamy-confirm-instead")->click();
	CHECK(host.track == 6);

	QJsonObject no_manage = audio(false, false);
	no_manage["manage"] = QString();
	show(view(idle, no_manage));
	CHECK(visible("dstreamy-audio-line") && !visible("dstreamy-manage") && !visible("dstreamy-sources"));

	show(view(live, audio(true, false)));
	CHECK(status_parts() ==
	      (QStringList{"dot:live", "missed", "0", "audio", "0", "reconnects", "0", "viewers", "3", "12:34"}));
	CHECK(plain(status->toolTip()) == QString::fromUtf8(live.tooltip));
	CHECK(status_part("text", "audio") && status_part("text", "audio")->accessibleName() == QLatin1String("audio 0"));
	check_status_icons(status);
	shoot_status("healthy");
	check_problem_status(app, status);
	CHECK(text("dstreamy-state") == QString::fromUtf8("Live · 6.0 Mbit/s · 60 fps · 12:34"));
	CHECK(!guilds->isEnabled() && !channels->isEnabled());
	CHECK(sources().size() == 5 && sources()[2]->isChecked());
	shoot("live");
	find<QPushButton>("dstreamy-action")->click();
	CHECK(host.stops == 0 && confirm_box() && confirm_box()->text() == QLatin1String("Stop the Discord stream?"));
	answer("cancel");
	CHECK(host.stops == 0);
	CHECK(dstreamy_ui_open_panel());
	app.processEvents();
	find<QPushButton>("dstreamy-action")->click();
	answer("ok");
	CHECK(host.stops == 1);
	CHECK(dstreamy_ui_open_panel());
	app.processEvents();

	show(view(attention, audio(true, false)));
	CHECK(text("dstreamy-state") == QLatin1String("Service not running"));
	CHECK(visible("dstreamy-fix") && text("dstreamy-fix") == QLatin1String("Run `dstreamy service install`"));
	CHECK(plain(find<QLabel>("dstreamy-fix")->toolTip())
		      .startsWith(QLatin1String("The D-Streamy service is not running")));
	CHECK(status_parts().first() == QLatin1String("dot:error"));
	CHECK(find<QLabel>("dstreamy-mark")->text() == QString::fromUtf8("⚠"));
	CHECK(!visible("dstreamy-login"));
	shoot("attention");

	const State expired{"attention",
			    "⚠",
			    "error",
			    "Discord login expired",
			    "",
			    "Discord rejected the saved login: press Log in again",
			    "Start D-Streamy",
			    "start",
			    true};
	show(view(expired, audio(true, false), false, "Log in again"));
	auto *login = find<QPushButton>("dstreamy-login");
	CHECK(login && visible("dstreamy-login") && login->text() == QLatin1String("Log in again"));
	CHECK(!visible("dstreamy-fix"));
	CHECK(login && tab_chain(login).mid(0, 2) == (QStringList{"dstreamy-login", "dstreamy-guilds"}));
	shoot("login");
	int views_before = host.views;
	if (login)
		login->click();
	CHECK(host.logins == 1);
	CHECK(wait_for([views_before] { return host.views > views_before; }));
	show(view(expired, audio(true, false), false, "Log in"));
	CHECK(login && login->text() == QLatin1String("Log in"));

	const State waiting{"busy",
			    "●",
			    "D-Streamy logging in… ▴",
			    "Logging in…",
			    "Waiting for login in the browser window…",
			    "Logging in…\nWaiting for login in the browser window…",
			    "Start D-Streamy",
			    "start",
			    false};
	show(view(waiting, audio(true, false), false, "Cancel login", "cancel"));
	CHECK(visible("dstreamy-login") && login->text() == QLatin1String("Cancel login"));
	CHECK(visible("dstreamy-fix") &&
	      text("dstreamy-fix") == QString::fromUtf8("Waiting for login in the browser window…"));
	CHECK(!find<QPushButton>("dstreamy-action")->isEnabled());
	if (login)
		login->click();
	CHECK(host.cancels == 1 && host.logins == 1);
	show(view(waiting, audio(true, false)));
	CHECK(!visible("dstreamy-login"));

	CHECK(!visible("dstreamy-account"));
	show(view(idle, audio(true, false), false, nullptr, "login", account_row(true)));
	auto *accounts = find<QComboBox>("dstreamy-accounts");
	auto *logout = find<QPushButton>("dstreamy-logout");
	CHECK(visible("dstreamy-account") && accounts && logout);
	CHECK(text("dstreamy-account-label") == QLatin1String("Account"));
	CHECK(accounts && accounts->count() == 3 && accounts->currentText() == QLatin1String("h_cloud_streamy"));
	CHECK(accounts && accounts->itemText(2) == QString::fromUtf8("Add account…"));
	CHECK(accounts && accounts->itemData(0, Qt::ToolTipRole).toString() == QLatin1String("Streamy"));
	CHECK(accounts && accounts->isEnabled() && logout && logout->isEnabled());
	CHECK(logout && logout->text() == QLatin1String("Log out"));
	CHECK((tab_chain(accounts).mid(0, 3) ==
	       QStringList{"dstreamy-accounts", "dstreamy-logout", "dstreamy-guilds"}));
	shoot("accounts");
	emit accounts->activated(0);
	CHECK(host.selects.empty());
	emit accounts->activated(1);
	CHECK((host.selects == std::vector<std::string>{"burner"}));
	CHECK(wait_for([accounts] { return accounts->currentIndex() == 0; }));
	emit accounts->activated(2);
	CHECK(host.adds == 1 && host.selects.size() == 1);
	CHECK(wait_for([accounts] { return accounts->currentIndex() == 0; }));
	logout->click();
	CHECK((host.logouts == std::vector<std::string>{"h_cloud_streamy"}));
	CHECK(!visible("dstreamy-logout-confirm"));

	QJsonObject asking = account_row(true);
	asking["confirm"] = QJsonObject{{"text", "Log out h_cloud_streamy?"}, {"ok", "Log out"}, {"cancel", "Not now"}};
	show(view(idle, audio(true, false), false, nullptr, "login", asking));
	CHECK(visible("dstreamy-logout-confirm") && !visible("dstreamy-logout"));
	CHECK(text("dstreamy-logout-text") == QLatin1String("Log out h_cloud_streamy?"));
	CHECK(find<QPushButton>("dstreamy-logout-ok")->text() == QLatin1String("Log out"));
	CHECK((tab_chain(accounts).mid(0, 4) ==
	       QStringList{"dstreamy-accounts", "dstreamy-logout-cancel", "dstreamy-logout-ok", "dstreamy-guilds"}));
	shoot("logout-confirm");
	find<QPushButton>("dstreamy-logout-cancel")->click();
	find<QPushButton>("dstreamy-logout-ok")->click();
	CHECK((host.logout_answers == std::vector<bool>{false, true}));
	show(view(idle, audio(true, false), false, nullptr, "login", account_row(true)));
	CHECK(!visible("dstreamy-logout-confirm") && visible("dstreamy-logout"));

	QJsonObject pending = account_row(false, "Switching account…", "Switching account…");
	QJsonArray items = pending["items"].toArray();
	QJsonObject first = items[0].toObject(), second = items[1].toObject();
	first["selected"] = false;
	second["selected"] = true;
	pending["items"] = QJsonArray{first, second};
	show(view(idle, audio(true, false), false, nullptr, "login", pending));
	CHECK(accounts->currentText() == QLatin1String("burner") && !accounts->isEnabled());

	show(view(waiting, audio(true, false), false, "Cancel login", "cancel",
		  account_row(false, "Finish or cancel the login first", "Finish or cancel the login first")));
	CHECK(plain(accounts->toolTip()) == QLatin1String("Finish or cancel the login first"));
	CHECK(plain(logout->toolTip()) == QLatin1String("Finish or cancel the login first"));

	show(view(live, audio(true, false), false, nullptr, "login",
		  account_row(false, "Stop the stream to switch accounts", "Stop the stream to log out")));
	CHECK(visible("dstreamy-account") && !accounts->isEnabled() && !logout->isEnabled());
	CHECK(plain(accounts->toolTip()) == QLatin1String("Stop the stream to switch accounts"));
	CHECK(plain(logout->toolTip()) == QLatin1String("Stop the stream to log out"));
	shoot("accounts-live");
	logout->click();
	CHECK(host.logouts.size() == 1);

	show(view(expired, audio(true, false), false, "Log in"));
	CHECK(!visible("dstreamy-account") && visible("dstreamy-login"));

	State long_state = live;
	long_state.line = "Live · 12.3 Mbit/s · 60 fps · 1:02:05 · 1234 dropped and more text that cannot fit";
	show(view(long_state, audio(true, false)));
	auto *state = find<QLabel>("dstreamy-state");
	QString shown = state ? state->property("shownText").toString() : QString();
	CHECK(state && state->text() == QString::fromUtf8(long_state.line));
	CHECK(!shown.isEmpty() && shown != state->text() && shown.endsWith(QChar(0x2026)));
	CHECK(state && !state->wordWrap() && state->height() < 2 * state->fontMetrics().height());
	CHECK(state && plain(state->toolTip()) == QString::fromUtf8(long_state.tooltip));

	State tagged = idle;
	tagged.line = "Idle · <b>Kids</b> & Co";
	tagged.tooltip = "Idle · <b>Kids</b> & Co\nTrack 6";
	show(view(tagged, audio(true, false, 6, {"<i>Mic</i>", "Music"})));
	CHECK(plain(status->toolTip()) == QString::fromUtf8(tagged.tooltip));
	CHECK(state && plain(state->toolTip()) == QString::fromUtf8(tagged.tooltip));
	CHECK(state && state->text() == QString::fromUtf8(tagged.line));
	CHECK(sources().size() == 2 && plain(sources()[0]->toolTip()) == QLatin1String("<i>Mic</i>"));
	CHECK(status->accessibleDescription() == QString::fromUtf8(tagged.tooltip));
	CHECK(state && state->accessibleDescription() == QString::fromUtf8(tagged.tooltip));
	CHECK(sources().size() == 2 && shows(sources()[0], QLatin1String("<i>Mic</i>")) &&
	      sources()[0]->accessibleName() == QLatin1String("<i>Mic</i>"));
	show(view(long_state, audio(true, false)));

	host.null_view = true;
	CHECK(polled());
	CHECK(state && state->text() == QString::fromUtf8(long_state.line));
	host.null_view = false;
	host.json = "not json";
	CHECK(polled());
	CHECK(state && state->text() == QString::fromUtf8(long_state.line));

	int closed = host.closed;
	QKeyEvent esc(QEvent::KeyPress, Qt::Key_Escape, Qt::NoModifier);
	QApplication::sendEvent(panel(), &esc);
	app.processEvents();
	CHECK(panel() && !panel()->isVisible());
	CHECK(host.closed == closed + 1);
	status->click();
	app.processEvents();
	CHECK(panel() && panel()->isVisible() && host.loads == 5);
	CHECK(dstreamy_ui_open_panel());
	CHECK(panel() && panel()->isVisible() && host.loads == 5);
	QApplication::sendEvent(panel(), &esc);
	app.processEvents();
	CHECK(panel() && !panel()->isVisible());

	CHECK(dstreamy_ui_open_panel());
	app.processEvents();
	CHECK(panel() && panel()->isVisible() && host.loads == 6);
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
	      plain(all[0]->toolTip()) == QString::fromStdString(many[0]) && shows(all[3], QLatin1String("Music")));
	CHECK(all.size() == 20 && all[0]->accessibleName() == QString::fromStdString(many[0]) &&
	      shows(all[0], QString::fromStdString(many[0])));
	CHECK(scroll && scroll->height() <= 8 * (all[0]->sizeHint().height() + 8) && scroll->height() > 0);
	CHECK(all.size() == 20 && all[0]->width() <= 160);
	CHECK(rows_settle());
	CHECK(all.size() == 20 && shows(all[5], QLatin1String("Mic & Game")) &&
	      plain(all[5]->toolTip()) == QLatin1String("Mic & Game") &&
	      all[5]->accessibleName() == QLatin1String("Mic & Game"));
	QRect screen = window.screen()->availableGeometry();
	CHECK(panel() && panel()->height() <= screen.height());
	auto *action = find<QPushButton>("dstreamy-action");
	CHECK(action && panel() && QRect(QPoint(0, 0), panel()->size()).contains(action->geometry()));
	shoot("many-sources");
	qApp->setStyleSheet(QStringLiteral("QFrame#dstreamy-panel { border: 14px solid transparent; }"));
	std::vector<std::string> themed = many;
	themed[0] += " in a theme with a wide panel border";
	show(view(idle, audio(true, false, 6, themed)));
	CHECK(rows_settle());
	qApp->setStyleSheet(QString());
	panel()->hide();

	check_status_action(status);
	status_action()->click();
	CHECK(confirm_box() != nullptr);
	dstreamy_ui_unload();
	CHECK(!confirm_box());
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
	if (status_action())
		status_action()->click();
	CHECK(confirm_box() != nullptr);
	fake::exit();
	CHECK(!confirm_box());
	host.unloaded = true;
	views = host.views;
	spin(1200);
	CHECK(host.views == views && host.after_unload == 0);
	CHECK(!find<QToolButton>("dstreamy-status") && !panel());
	dstreamy_ui_unload();
	CHECK(fake::unwatches == 1);

	host.unloaded = false;
	int watches = fake::watches;
	host.throw_view = true;
	CHECK(!dstreamy_ui_load(&good));
	host.throw_view = false;
	CHECK(fake::warnings == 1 && fake::watches == watches + 1 && fake::unwatches == 2);
	CHECK(!find<QToolButton>("dstreamy-status") && !panel());
	CHECK(dstreamy_ui_load(&good));
	host.throw_load = true;
	CHECK(!dstreamy_ui_open_panel());
	host.throw_load = false;
	CHECK(fake::warnings == 2 && fake::unwatches == 3);
	CHECK(!find<QToolButton>("dstreamy-status") && !panel());
	CHECK(!dstreamy_ui_open_panel());
	dstreamy_ui_unload();
	CHECK(fake::unwatches == 3);
	CHECK(dstreamy_ui_load(&good));
	dstreamy_ui_unload();
	CHECK(fake::unwatches == 4);

	host.json = view(idle, audio(true, false, 6, many));
	CHECK(dstreamy_ui_load(&good));
	status = window.statusBar()->findChild<QToolButton *>("dstreamy-status");
	if (status)
		status->click();
	app.processEvents();
	all = sources();
	CHECK(panel() && panel()->isVisible() && all.size() == 20);
	CHECK(rows_settle());

	closed = host.closed;
	host.throw_view = true;
	CHECK(wait_for([] { return !find<QToolButton>("dstreamy-status") && !panel(); }));
	host.throw_view = false;
	CHECK(fake::warnings == 3 && fake::unwatches == 5);
	CHECK(host.closed == closed + 1);
	CHECK(!dstreamy_ui_open_panel());

	host.json = view(idle, audio(false, false));
	CHECK(dstreamy_ui_load(&good));
	CHECK(dstreamy_ui_open_panel());
	tracks = find<QComboBox>("dstreamy-tracks");
	closed = host.closed;
	host.throw_track = true;
	if (tracks)
		emit tracks->activated(1);
	host.throw_track = false;
	CHECK(fake::warnings == 4 && !dstreamy_ui_open_panel());
	CHECK(wait_for([] { return !find<QToolButton>("dstreamy-status") && !panel(); }));
	CHECK(fake::unwatches == 6);
	CHECK(host.closed == closed + 1);
	CHECK(dstreamy_ui_load(&good));
	dstreamy_ui_unload();
	CHECK(fake::unwatches == 7);

	// A first view without a status bar, or with no counters, must not leave the bar half built.
	for (bool keep_status : {false, true}) {
		QJsonObject first = QJsonDocument::fromJson(QByteArray::fromStdString(view(idle, audio(false, false)))).object();
		if (keep_status) {
			QJsonObject bare = first.value("status").toObject();
			bare["counters"] = QJsonArray();
			first["status"] = bare;
		} else {
			first.remove("status");
		}
		host.json = QJsonDocument(first).toJson(QJsonDocument::Compact).toStdString();
		CHECK(dstreamy_ui_load(&good));
		CHECK(polled());
		CHECK(status_parts() == QStringList{keep_status ? QStringLiteral("dot:idle") : QStringLiteral("dot:")});
		show(view(live, audio(true, false)));
		CHECK(status_parts().size() == 10 && status_parts().last() == QLatin1String("12:34"));
		dstreamy_ui_unload();
	}
	CHECK(fake::unwatches == 9);

	if (failures) {
		std::fprintf(stderr, "dstreamy-ui-test: %d failures\n", failures);
		return 1;
	}
	std::printf("dstreamy-ui-test: PASS\n");
	return 0;
}
