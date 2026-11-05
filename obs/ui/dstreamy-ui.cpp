#include "dstreamy-ui.h"
#include "frontend.hpp"

#include <QCheckBox>
#include <QComboBox>
#include <QEvent>
#include <QFormLayout>
#include <QFrame>
#include <QGridLayout>
#include <QGuiApplication>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QKeyEvent>
#include <QLabel>
#include <QMainWindow>
#include <QPainter>
#include <QPointer>
#include <QPushButton>
#include <QScreen>
#include <QScrollArea>
#include <QScrollBar>
#include <QSignalBlocker>
#include <QStatusBar>
#include <QStyle>
#include <QTextDocument>
#include <QTimer>
#include <QToolButton>
#include <QVBoxLayout>

#include <cstring>
#include <memory>

namespace {

constexpr int poll_ms = 500;
constexpr int source_rows = 8;

// Tooltips guess rich text from their content, and Discord and source names are user-controlled.
QString plain_tip(const QString &text)
{
	return text.isEmpty() ? text : Qt::convertFromPlainText(text, Qt::WhiteSpaceNormal);
}

class ElidedLabel : public QLabel {
public:
	explicit ElidedLabel(QWidget *parent) : QLabel(parent)
	{
		setTextFormat(Qt::PlainText);
		setWordWrap(false);
		setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Fixed);
	}

	void show_text(const QString &text, const QString &tooltip)
	{
		setText(text);
		const QString &tip = tooltip.isEmpty() ? text : tooltip;
		setToolTip(plain_tip(tip));
		setAccessibleDescription(tip);
		update_shown();
	}

	QSize sizeHint() const override { return QSize(0, QLabel::sizeHint().height()); }
	QSize minimumSizeHint() const override { return QSize(0, QLabel::minimumSizeHint().height()); }

protected:
	void resizeEvent(QResizeEvent *event) override
	{
		QLabel::resizeEvent(event);
		update_shown();
	}

	void changeEvent(QEvent *event) override
	{
		QLabel::changeEvent(event);
		if (event->type() == QEvent::FontChange || event->type() == QEvent::StyleChange)
			update_shown();
	}

	void paintEvent(QPaintEvent *) override
	{
		QPainter painter(this);
		style()->drawItemText(&painter, contentsRect(), Qt::AlignLeft | Qt::AlignVCenter, palette(),
				      isEnabled(), property("shownText").toString(), foregroundRole());
	}

private:
	void update_shown()
	{
		setProperty("shownText", fontMetrics().elidedText(text(), Qt::ElideRight, contentsRect().width()));
		update();
	}
};

void panel_hidden();
void fail_later(const char *call);

// A slot runs from Qt's event loop, where an exception would end OBS; drop the panel instead.
template<typename F> void guarded(const char *call, F &&body)
{
	try {
		body();
	} catch (...) {
		fail_later(call);
	}
}

class Panel : public QFrame {
public:
	Panel() : QFrame(nullptr, Qt::Popup)
	{
		setObjectName(QStringLiteral("dstreamy-panel"));
		setFrameShape(QFrame::StyledPanel);
		setAttribute(Qt::WA_NoMouseReplay);
	}

protected:
	void keyPressEvent(QKeyEvent *event) override
	{
		if (event->key() == Qt::Key_Escape) {
			close();
			return;
		}
		QFrame::keyPressEvent(event);
	}

	void hideEvent(QHideEvent *event) override
	{
		QFrame::hideEvent(event);
		guarded("panel_closed", panel_hidden);
	}
};

struct Ui {
	dstreamy_ui_host host{};
	bool active = false;
	QPointer<QMainWindow> window;
	QPointer<QToolButton> status;
	QPointer<Panel> panel;
	QPointer<QTimer> timer;
	QPointer<QLabel> mark;
	QPointer<ElidedLabel> state_line;
	QPointer<ElidedLabel> state_fix;
	QPointer<QComboBox> guilds;
	QPointer<QComboBox> channels;
	QPointer<QLabel> audio_title;
	QPointer<QComboBox> tracks;
	QPointer<QLabel> audio_line;
	QPointer<QPushButton> manage;
	QPointer<QWidget> confirm_row;
	QPointer<QLabel> confirm_text;
	QPointer<QPushButton> confirm_ok;
	QPointer<QPushButton> confirm_cancel;
	QPointer<QPushButton> confirm_instead;
	QPointer<QWidget> notices;
	QPointer<QScrollArea> scroll;
	QPointer<QWidget> sources;
	QPointer<QLabel> empty;
	QPointer<QPushButton> action;
	QByteArray shown;
	QString pressed;
	QString source_list;
	QString notice_list;
};

Ui ui;

void render();

// After a control changed something: show the host's answer, not the widget's own state.
void rerender()
{
	ui.shown.clear();
	if (ui.status)
		QTimer::singleShot(0, ui.status, [] { guarded("render", render); });
}

void panel_hidden()
{
	if (ui.active)
		ui.host.panel_closed(ui.host.ctx);
}

QString tone_color(const QString &tone)
{
	if (tone == QLatin1String("live"))
		return QStringLiteral("#e5484d");
	if (tone == QLatin1String("busy"))
		return QStringLiteral("#f5a524");
	if (tone == QLatin1String("attention"))
		return QStringLiteral("#f76b15");
	return QStringLiteral("#8b8d98");
}

QLabel *label(const char *name, QWidget *parent, const char *classes = nullptr)
{
	auto *l = new QLabel(parent);
	l->setObjectName(QString::fromLatin1(name));
	l->setTextFormat(Qt::PlainText);
	if (classes)
		l->setProperty("class", QString::fromLatin1(classes));
	return l;
}

QPushButton *button(const char *name, QWidget *parent)
{
	auto *b = new QPushButton(parent);
	b->setObjectName(QString::fromLatin1(name));
	return b;
}

// Refilling closes an open drop-down, so only changed entries are refilled.
void fill(QComboBox *box, const QJsonObject &choices)
{
	QSignalBlocker block(box);
	QJsonArray items = choices.value("items").toArray();
	QString key = choices.value("placeholder").toString();
	for (const QJsonValue v : items)
		key += QLatin1Char('\n') + v.toObject().value("id").toString() + QLatin1Char('\t') +
		       v.toObject().value("name").toString();
	if (box->property("filled").toString() != key) {
		box->clear();
		box->addItem(choices.value("placeholder").toString(), QString());
		for (const QJsonValue v : items)
			box->addItem(v.toObject().value("name").toString(), v.toObject().value("id").toString());
		box->setProperty("filled", key);
	}
	int current = 0;
	for (int i = 0; i < items.size(); i++)
		if (items[i].toObject().value("selected").toBool())
			current = i + 1;
	box->setCurrentIndex(current);
	box->setEnabled(choices.value("enabled").toBool());
}

void fill_tracks(const QJsonObject &audio)
{
	QSignalBlocker block(ui.tracks);
	QJsonArray tracks = audio.value("tracks").toArray();
	if (ui.tracks->count() != tracks.size()) {
		ui.tracks->clear();
		for (const QJsonValue v : tracks)
			ui.tracks->addItem(v.toObject().value("name").toString(), v.toObject().value("track").toInt());
	}
	for (int i = 0; i < tracks.size(); i++)
		if (tracks[i].toObject().value("selected").toBool())
			ui.tracks->setCurrentIndex(i);
}

// The width one of the two source columns gets in the laid-out scroll area, a vertical scrollbar reserved.
int source_cell(QGridLayout *grid)
{
	ui.panel->ensurePolished();
	ui.panel->layout()->activate();
	QStyle *style = ui.scroll->style();
	int bar = qMax(ui.scroll->verticalScrollBar()->sizeHint().width(),
		       style->pixelMetric(QStyle::PM_ScrollBarExtent));
	int gap = grid->horizontalSpacing();
	if (gap < 0)
		gap = style->pixelMetric(QStyle::PM_LayoutHorizontalSpacing);
	int viewport = ui.scroll->width() - 2 * ui.scroll->frameWidth() - bar;
	return (viewport - qMax(gap, 0)) / 2;
}

// Elides `name` until the box's size hint, which includes the style's indicator and padding, fits `cell`.
QString fit_text(QCheckBox *box, const QString &name, int cell)
{
	int budget = cell;
	QString shown = name;
	for (int tries = 0; tries < 8; tries++) {
		shown = box->fontMetrics().elidedText(name, Qt::ElideRight, qMax(budget, 0));
		box->setText(QString(shown).replace(QLatin1Char('&'), QLatin1String("&&")));
		int over = box->sizeHint().width() - cell;
		if (over <= 0 || budget <= 0)
			break;
		budget -= over;
	}
	return shown;
}

void fill_sources(const QJsonArray &sources)
{
	QStringList ids;
	for (const QJsonValue v : sources)
		ids << v.toObject().value("uuid").toString() + QLatin1Char('\n') +
				v.toObject().value("name").toString();
	QString list = ids.join(QLatin1Char('\n'));
	auto *grid = static_cast<QGridLayout *>(ui.sources->layout());
	if (list != ui.source_list) {
		for (QCheckBox *box : ui.sources->findChildren<QCheckBox *>(QString(), Qt::FindDirectChildrenOnly))
			delete box;
		int cell = source_cell(grid);
		int i = 0;
		for (const QJsonValue v : sources) {
			QJsonObject s = v.toObject();
			QString name = s.value("name").toString();
			auto *box = new QCheckBox(ui.sources);
			box->setToolTip(plain_tip(name));
			box->setAccessibleName(name);
			box->setProperty("name", name);
			box->setObjectName(QStringLiteral("dstreamy-source"));
			box->setProperty("uuid", s.value("uuid").toString());
			box->ensurePolished();
			QString shown = fit_text(box, name, cell);
			if (shown != name)
				box->setAccessibleDescription(name);
			QObject::connect(box, &QCheckBox::clicked, box, [box](bool on) {
				guarded("set_source", [box, on] {
					if (!ui.active)
						return;
					QByteArray uuid = box->property("uuid").toString().toUtf8();
					ui.host.set_source(ui.host.ctx, uuid.constData(), on);
					rerender();
				});
			});
			grid->addWidget(box, i / 2, i % 2);
			i++;
		}
		ui.source_list = list;
		int rows = qMin((i + 1) / 2, source_rows);
		int spacing = grid->verticalSpacing();
		if (spacing < 0)
			spacing = ui.sources->style()->pixelMetric(QStyle::PM_LayoutVerticalSpacing);
		QList<QCheckBox *> made = ui.sources->findChildren<QCheckBox *>();
		int row = made.isEmpty() ? 0 : made.first()->sizeHint().height();
		ui.scroll->setFixedHeight(rows ? rows * row + (rows - 1) * qMax(spacing, 0) : 0);
	}
	const QList<QCheckBox *> boxes = ui.sources->findChildren<QCheckBox *>();
	for (const QJsonValue v : sources) {
		QJsonObject s = v.toObject();
		for (QCheckBox *box : boxes) {
			if (box->property("uuid").toString() == s.value("uuid").toString()) {
				QSignalBlocker block(box);
				box->setChecked(s.value("on").toBool());
			}
		}
	}
}

void fill_notices(const QJsonArray &notices)
{
	QString list = QString::fromUtf8(QJsonDocument(notices).toJson(QJsonDocument::Compact));
	if (list == ui.notice_list)
		return;
	ui.notice_list = list;
	for (QWidget *w : ui.notices->findChildren<QWidget *>(QString(), Qt::FindDirectChildrenOnly))
		delete w;
	auto *rows = static_cast<QVBoxLayout *>(ui.notices->layout());
	for (const QJsonValue v : notices) {
		QJsonObject n = v.toObject();
		QLabel *text = label("dstreamy-notice", ui.notices, "text-warning");
		text->setWordWrap(true);
		text->setText(n.value("text").toString());
		rows->addWidget(text);
		QJsonValue release = n.value("release");
		if (!release.isObject())
			continue;
		QPushButton *b = button("dstreamy-release", ui.notices);
		b->setText(release.toObject().value("label").toString());
		int track = release.toObject().value("track").toInt();
		QObject::connect(b, &QPushButton::clicked, b, [track] {
			guarded("release_track", [track] {
				if (!ui.active)
					return;
				ui.host.release_track(ui.host.ctx, track);
				rerender();
			});
		});
		rows->addWidget(b, 0, Qt::AlignLeft);
	}
	ui.notices->setVisible(!notices.isEmpty());
}

void order_tabs()
{
	QList<QWidget *> order{ui.guilds, ui.channels, ui.tracks};
	const QList<QPushButton *> releases =
		ui.notices->findChildren<QPushButton *>(QString(), Qt::FindDirectChildrenOnly);
	for (QPushButton *b : releases)
		order << b;
	order << ui.manage << ui.confirm_cancel << ui.confirm_instead << ui.confirm_ok;
	const QList<QCheckBox *> boxes = ui.sources->findChildren<QCheckBox *>(QString(), Qt::FindDirectChildrenOnly);
	for (QCheckBox *box : boxes)
		order << box;
	order << ui.action;
	for (qsizetype i = 1; i < order.size(); i++)
		QWidget::setTabOrder(order[i - 1], order[i]);
}

void place_panel()
{
	if (!ui.panel || !ui.status || !ui.window)
		return;
	bool bar = ui.status->isVisible();
	QWidget *anchor = bar ? static_cast<QWidget *>(ui.status) : ui.window.data();
	QPoint corner = bar ? ui.status->mapToGlobal(QPoint(ui.status->width(), 0))
			    : ui.window->mapToGlobal(QPoint(ui.window->width() - 8, ui.window->height() - 8));
	// screen() is the top-level window's screen, which can differ from the one under the corner.
	QScreen *screen = QGuiApplication::screenAt(corner - QPoint(1, 1));
	if (!screen)
		screen = anchor->screen();
	QRect room = screen ? screen->availableGeometry() : QRect();
	// Wrapped labels need the height for the panel's fixed width, which adjustSize doesn't ask for.
	ui.panel->layout()->activate();
	int width = ui.panel->width();
	int height = ui.panel->hasHeightForWidth() ? ui.panel->heightForWidth(width) : ui.panel->sizeHint().height();
	height = qMax(height, ui.panel->minimumSizeHint().height());
	if (room.isValid())
		height = qMin(height, room.height());
	ui.panel->resize(width, height);
	QSize size = ui.panel->size();
	int x = corner.x() - size.width();
	int y = corner.y() - size.height();
	if (room.isValid()) {
		x = qBound(room.left(), x, qMax(room.left(), room.right() - size.width() + 1));
		y = qBound(room.top(), y, qMax(room.top(), room.bottom() - size.height() + 1));
	}
	ui.panel->move(x, y);
}

void render()
{
	if (!ui.active || !ui.panel)
		return;
	char *raw = ui.host.view_json(ui.host.ctx);
	if (!raw)
		return;
	QByteArray json(raw);
	ui.host.free_string(raw);
	if (json == ui.shown)
		return;
	QJsonDocument doc = QJsonDocument::fromJson(json);
	if (!doc.isObject())
		return;
	ui.shown = json;

	QJsonObject view = doc.object();
	QString color = QStringLiteral("color: %1;").arg(tone_color(view.value("tone").toString()));
	if (ui.status) {
		ui.status->setText(view.value("status_bar").toString().replace(QLatin1Char('&'), QLatin1String("&&")));
		ui.status->setStyleSheet(color);
		QString tip = view.value("status_tooltip").toString();
		ui.status->setToolTip(plain_tip(tip));
		ui.status->setAccessibleDescription(tip);
	}

	ui.panel->setProperty("tone", view.value("tone").toString());
	ui.mark->setText(view.value("mark").toString());
	ui.mark->setStyleSheet(color);
	QString tooltip = view.value("state_tooltip").toString();
	ui.state_line->show_text(view.value("state_line").toString(), tooltip);
	QString fix = view.value("state_fix").toString();
	ui.state_fix->show_text(fix, tooltip);
	ui.state_fix->setVisible(!fix.isEmpty());

	fill(ui.guilds, view.value("guilds").toObject());
	fill(ui.channels, view.value("channels").toObject());

	QJsonObject audio = view.value("audio").toObject();
	ui.audio_title->setText(audio.value("title").toString());
	ui.tracks->setAccessibleName(audio.value("title").toString());
	fill_tracks(audio);
	bool managed = audio.value("managed").toBool();
	QJsonValue confirm = audio.value("confirm");
	bool confirming = confirm.isObject();
	QString line = audio.value("line").toString();
	ui.audio_line->setText(line);
	ui.audio_line->setVisible(!confirming && !line.isEmpty());
	QString manage = audio.value("manage").toString();
	ui.manage->setText(manage);
	ui.manage->setVisible(!managed && !confirming && !manage.isEmpty());
	ui.confirm_row->setVisible(confirming);
	if (confirming) {
		QJsonObject c = confirm.toObject();
		ui.confirm_text->setText(c.value("text").toString());
		ui.confirm_ok->setText(c.value("ok").toString());
		ui.confirm_cancel->setText(c.value("cancel").toString());
		QJsonObject instead = c.value("instead").toObject();
		ui.confirm_instead->setText(instead.value("label").toString());
		ui.confirm_instead->setProperty("track", instead.value("track").toInt());
		ui.confirm_instead->setVisible(!instead.isEmpty());
	}
	fill_notices(audio.value("notices").toArray());
	ui.scroll->setVisible(managed);
	fill_sources(audio.value("sources").toArray());
	QString empty = audio.value("empty").toString();
	ui.empty->setText(empty);
	ui.empty->setVisible(managed && !empty.isEmpty());
	order_tabs();

	QJsonObject button = view.value("button").toObject();
	ui.action->setText(button.value("label").toString());
	ui.action->setEnabled(button.value("enabled").toBool());
	ui.pressed = button.value("action").toString();

	if (ui.panel->isVisible()) {
		place_panel();
		// Rows shown or hidden above settle their layout only once events run.
		QTimer::singleShot(0, ui.panel, [] { guarded("place_panel", place_panel); });
	}
}

void press()
{
	if (!ui.active)
		return;
	if (ui.pressed == QLatin1String("start"))
		ui.host.start(ui.host.ctx);
	else if (ui.pressed == QLatin1String("stop"))
		ui.host.stop(ui.host.ctx);
	rerender();
}

bool open_panel()
{
	if (!ui.active || !ui.panel)
		return false;
	if (ui.panel->isVisible()) {
		ui.panel->raise();
		ui.panel->activateWindow();
		return true;
	}
	ui.host.load_lists(ui.host.ctx);
	ui.panel->ensurePolished();
	ui.shown.clear();
	render();
	place_panel();
	ui.panel->show();
	// The offscreen platform moves a new window by its emulated frame on first show.
	place_panel();
	ui.panel->raise();
	ui.panel->activateWindow();
	return ui.panel->isVisible();
}

void toggle_panel()
{
	if (ui.panel && ui.panel->isVisible())
		ui.panel->hide();
	else
		open_panel();
}

void choose(QComboBox *box, void (*callback)(void *, const char *))
{
	QObject::connect(box, &QComboBox::activated, box, [box, callback](int index) {
		guarded("select", [box, callback, index] {
			if (!ui.active)
				return;
			QByteArray id = box->itemData(index).toString().toUtf8();
			callback(ui.host.ctx, id.constData());
			rerender();
		});
	});
}

Panel *build_panel()
{
	auto owner = std::make_unique<Panel>();
	Panel *panel = owner.get();
	auto *rows = new QVBoxLayout(panel);

	auto *state = new QHBoxLayout();
	ui.mark = label("dstreamy-mark", panel);
	ui.mark->setSizePolicy(QSizePolicy::Fixed, QSizePolicy::Fixed);
	ui.state_line = new ElidedLabel(panel);
	ui.state_line->setObjectName(QStringLiteral("dstreamy-state"));
	QFont bold = ui.state_line->font();
	bold.setBold(true);
	ui.state_line->setFont(bold);
	state->addWidget(ui.mark);
	state->addWidget(ui.state_line, 1);
	rows->addLayout(state);
	ui.state_fix = new ElidedLabel(panel);
	ui.state_fix->setObjectName(QStringLiteral("dstreamy-fix"));
	ui.state_fix->setProperty("class", QStringLiteral("text-muted text-small"));
	rows->addWidget(ui.state_fix);

	auto *form = new QFormLayout();
	ui.guilds = new QComboBox(panel);
	ui.guilds->setObjectName(QStringLiteral("dstreamy-guilds"));
	ui.channels = new QComboBox(panel);
	ui.channels->setObjectName(QStringLiteral("dstreamy-channels"));
	form->addRow(QStringLiteral("Server"), ui.guilds);
	form->addRow(QStringLiteral("Channel"), ui.channels);
	rows->addLayout(form);
	choose(ui.guilds, [](void *ctx, const char *id) { ui.host.select_guild(ctx, id); });
	choose(ui.channels, [](void *ctx, const char *id) { ui.host.select_channel(ctx, id); });

	auto *line = new QFrame(panel);
	line->setFrameShape(QFrame::HLine);
	line->setFrameShadow(QFrame::Sunken);
	rows->addWidget(line);

	auto *audio = new QHBoxLayout();
	ui.audio_title = label("dstreamy-audio-title", panel);
	QFont title = ui.audio_title->font();
	title.setBold(true);
	ui.audio_title->setFont(title);
	ui.tracks = new QComboBox(panel);
	ui.tracks->setObjectName(QStringLiteral("dstreamy-tracks"));
	QObject::connect(ui.tracks, &QComboBox::activated, ui.tracks, [](int index) {
		guarded("set_track", [index] {
			if (!ui.active)
				return;
			ui.host.set_track(ui.host.ctx, ui.tracks->itemData(index).toInt());
			rerender();
		});
	});
	audio->addWidget(ui.audio_title);
	audio->addStretch(1);
	audio->addWidget(ui.tracks);
	rows->addLayout(audio);

	ui.notices = new QWidget(panel);
	ui.notices->setObjectName(QStringLiteral("dstreamy-notices"));
	auto *notice_rows = new QVBoxLayout(ui.notices);
	notice_rows->setContentsMargins(0, 0, 0, 0);
	ui.notices->setVisible(false);
	rows->addWidget(ui.notices);

	ui.audio_line = label("dstreamy-audio-line", panel, "text-muted");
	ui.audio_line->setWordWrap(true);
	rows->addWidget(ui.audio_line);
	ui.manage = button("dstreamy-manage", panel);
	QObject::connect(ui.manage, &QPushButton::clicked, ui.manage, [] {
		guarded("request_take_over", [] {
			if (!ui.active)
				return;
			ui.host.request_take_over(ui.host.ctx);
			rerender();
		});
	});
	rows->addWidget(ui.manage, 0, Qt::AlignLeft);

	ui.confirm_row = new QWidget(panel);
	ui.confirm_row->setObjectName(QStringLiteral("dstreamy-confirm"));
	auto *confirm = new QVBoxLayout(ui.confirm_row);
	confirm->setContentsMargins(0, 0, 0, 0);
	ui.confirm_text = label("dstreamy-confirm-text", ui.confirm_row);
	ui.confirm_text->setWordWrap(true);
	confirm->addWidget(ui.confirm_text);
	auto *answers = new QHBoxLayout();
	ui.confirm_ok = button("dstreamy-confirm-ok", ui.confirm_row);
	ui.confirm_cancel = button("dstreamy-confirm-cancel", ui.confirm_row);
	auto answer = [](QPushButton *b, bool yes) {
		QObject::connect(b, &QPushButton::clicked, b, [yes] {
			guarded("confirm_take_over", [yes] {
				if (!ui.active)
					return;
				ui.host.confirm_take_over(ui.host.ctx, yes);
				rerender();
			});
		});
	};
	answer(ui.confirm_ok, true);
	answer(ui.confirm_cancel, false);
	ui.confirm_instead = button("dstreamy-confirm-instead", ui.confirm_row);
	QObject::connect(ui.confirm_instead, &QPushButton::clicked, ui.confirm_instead, [] {
		guarded("set_track", [] {
			if (!ui.active)
				return;
			ui.host.set_track(ui.host.ctx, ui.confirm_instead->property("track").toInt());
			rerender();
		});
	});
	answers->addStretch(1);
	answers->addWidget(ui.confirm_cancel);
	answers->addWidget(ui.confirm_instead);
	answers->addWidget(ui.confirm_ok);
	confirm->addLayout(answers);
	rows->addWidget(ui.confirm_row);

	ui.scroll = new QScrollArea(panel);
	ui.scroll->setObjectName(QStringLiteral("dstreamy-sources-scroll"));
	ui.scroll->setFrameShape(QFrame::NoFrame);
	ui.scroll->setFocusPolicy(Qt::NoFocus);
	ui.scroll->setWidgetResizable(true);
	ui.scroll->setHorizontalScrollBarPolicy(Qt::ScrollBarAlwaysOff);
	ui.sources = new QWidget(ui.scroll);
	ui.sources->setObjectName(QStringLiteral("dstreamy-sources"));
	auto *grid = new QGridLayout(ui.sources);
	grid->setContentsMargins(0, 0, 0, 0);
	ui.scroll->setWidget(ui.sources);
	rows->addWidget(ui.scroll);
	ui.empty = label("dstreamy-empty", panel, "text-muted");
	ui.empty->setWordWrap(true);
	rows->addWidget(ui.empty);

	ui.action = button("dstreamy-action", panel);
	ui.action->setEnabled(false);
	QObject::connect(ui.action, &QPushButton::clicked, ui.action, [] { guarded("start_stop", press); });
	rows->addWidget(ui.action, 0, Qt::AlignRight);
	panel->setFixedWidth(320);
	return owner.release();
}

void teardown()
{
	ui.active = false;
	delete ui.timer;
	if (ui.window && ui.status)
		ui.window->statusBar()->removeWidget(ui.status);
	delete ui.status;
	delete ui.panel;
	ui = Ui{};
}

bool exited;
bool watching;
bool failing;

void on_exit()
{
	exited = true;
	watching = false;
	teardown();
}

bool load(const struct dstreamy_ui_host *host)
{
	if (ui.active || !host || host->abi_version != DSTREAMY_UI_ABI_VERSION || !host->view_json ||
	    !host->free_string || !host->start || !host->stop || !host->load_lists || !host->select_guild ||
	    !host->select_channel || !host->set_track || !host->request_take_over || !host->confirm_take_over ||
	    !host->set_source || !host->panel_closed || !host->release_track || failing)
		return false;
	QMainWindow *window = dstreamy_ui::main_window();
	if (!window)
		return false;
	exited = false;

	ui.host = *host;
	ui.window = window;
	ui.panel = build_panel();
	ui.panel->ensurePolished();

	ui.status = new QToolButton();
	ui.status->setObjectName(QStringLiteral("dstreamy-status"));
	ui.status->setAutoRaise(true);
	QObject::connect(ui.status, &QToolButton::clicked, ui.status, [] { guarded("open_panel", toggle_panel); });
	window->statusBar()->addPermanentWidget(ui.status);

	ui.timer = new QTimer(ui.status);
	ui.timer->setInterval(poll_ms);
	QObject::connect(ui.timer, &QTimer::timeout, ui.status, [] { guarded("render", render); });
	ui.timer->start();

	dstreamy_ui::watch_exit(on_exit);
	watching = true;
	ui.active = true;
	render();
	return true;
}

void unload()
{
	// After OBS's exit event the frontend API is gone, so nothing may call it.
	if (exited)
		return;
	if (watching) {
		watching = false;
		dstreamy_ui::unwatch_exit();
	}
	teardown();
}

// An exception must not unwind into the plugin's Rust frames; drop the panel and leave Tools > D-Streamy.
void failed(const char *call)
{
	dstreamy_ui::warn(call);
	try {
		unload();
	} catch (...) {
		ui.active = false;
	}
}

// The throwing slot may belong to a widget teardown deletes, so the teardown waits for the event loop.
void fail_later(const char *call)
{
	dstreamy_ui::warn(call);
	// The panel goes without a hide event the host would see, so the host hears it closed here.
	bool was_open = ui.active && ui.panel && ui.panel->isVisible() && std::strcmp(call, "panel_closed") != 0;
	ui.active = false;
	if (was_open) {
		try {
			ui.host.panel_closed(ui.host.ctx);
		} catch (...) {
		}
	}
	if (failing)
		return;
	failing = true;
	QTimer::singleShot(0, QCoreApplication::instance(), [] {
		failing = false;
		try {
			unload();
		} catch (...) {
		}
	});
}

} // namespace

uint32_t dstreamy_ui_abi_version(void)
{
	return DSTREAMY_UI_ABI_VERSION;
}

bool dstreamy_ui_load(const struct dstreamy_ui_host *host)
{
	try {
		return load(host);
	} catch (...) {
		failed("dstreamy_ui_load");
		return false;
	}
}

bool dstreamy_ui_open_panel(void)
{
	try {
		return open_panel();
	} catch (...) {
		failed("dstreamy_ui_open_panel");
		return false;
	}
}

void dstreamy_ui_unload(void)
{
	try {
		unload();
	} catch (...) {
		dstreamy_ui::warn("dstreamy_ui_unload");
		ui.active = false;
	}
}
