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
#include <QSignalBlocker>
#include <QStatusBar>
#include <QStyle>
#include <QTimer>
#include <QToolButton>
#include <QVBoxLayout>

namespace {

constexpr int poll_ms = 500;
constexpr int source_rows = 8;

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
		setToolTip(tooltip.isEmpty() ? text : tooltip);
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
		panel_hidden();
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
	QPointer<QScrollArea> scroll;
	QPointer<QWidget> sources;
	QPointer<QLabel> empty;
	QPointer<QPushButton> action;
	QByteArray shown;
	QString pressed;
	QString source_list;
};

Ui ui;

void render();

// After a control changed something: show the host's answer, not the widget's own state.
void rerender()
{
	ui.shown.clear();
	if (ui.status)
		QTimer::singleShot(0, ui.status, [] { render(); });
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
		int indicator = ui.sources->style()->pixelMetric(QStyle::PM_IndicatorWidth) +
				ui.sources->style()->pixelMetric(QStyle::PM_CheckBoxLabelSpacing);
		int column = (ui.panel->width() - 48) / 2 - indicator;
		int i = 0;
		for (const QJsonValue v : sources) {
			QJsonObject s = v.toObject();
			QString name = s.value("name").toString();
			auto *box = new QCheckBox(ui.sources);
			QString shown = box->fontMetrics().elidedText(name, Qt::ElideRight, column);
			box->setText(shown.replace(QLatin1Char('&'), QLatin1String("&&")));
			box->setToolTip(name);
			box->setProperty("name", name);
			box->setObjectName(QStringLiteral("dstreamy-source"));
			box->setProperty("uuid", s.value("uuid").toString());
			QObject::connect(box, &QCheckBox::clicked, box, [box](bool on) {
				if (!ui.active)
					return;
				ui.host.set_source(ui.host.ctx, box->property("uuid").toString().toUtf8().constData(),
						   on);
				rerender();
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

void place_panel()
{
	if (!ui.panel || !ui.status || !ui.window)
		return;
	bool bar = ui.status->isVisible();
	QWidget *anchor = bar ? static_cast<QWidget *>(ui.status) : ui.window.data();
	QRect room = anchor->screen() ? anchor->screen()->availableGeometry() : QRect();
	// Wrapped labels need the height for the panel's fixed width, which adjustSize doesn't ask for.
	ui.panel->layout()->activate();
	int width = ui.panel->width();
	int height = ui.panel->hasHeightForWidth() ? ui.panel->heightForWidth(width) : ui.panel->sizeHint().height();
	height = qMax(height, ui.panel->minimumSizeHint().height());
	if (room.isValid())
		height = qMin(height, room.height());
	ui.panel->resize(width, height);
	QSize size = ui.panel->size();
	QPoint corner = bar ? ui.status->mapToGlobal(QPoint(ui.status->width(), 0))
			    : ui.window->mapToGlobal(QPoint(ui.window->width() - 8, ui.window->height() - 8));
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
		ui.status->setText(view.value("status_bar").toString());
		ui.status->setStyleSheet(color);
		ui.status->setToolTip(view.value("status_tooltip").toString());
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
	fill_tracks(audio);
	bool managed = audio.value("managed").toBool();
	QJsonValue confirm = audio.value("confirm");
	bool confirming = confirm.isObject();
	ui.audio_line->setText(audio.value("line").toString());
	ui.audio_line->setVisible(!managed && !confirming);
	ui.manage->setText(audio.value("manage").toString());
	ui.manage->setVisible(!managed && !confirming);
	ui.confirm_row->setVisible(confirming);
	if (confirming) {
		QJsonObject c = confirm.toObject();
		ui.confirm_text->setText(c.value("text").toString());
		ui.confirm_ok->setText(c.value("ok").toString());
		ui.confirm_cancel->setText(c.value("cancel").toString());
	}
	ui.scroll->setVisible(managed);
	fill_sources(audio.value("sources").toArray());
	QString empty = audio.value("empty").toString();
	ui.empty->setText(empty);
	ui.empty->setVisible(managed && !empty.isEmpty());

	QJsonObject button = view.value("button").toObject();
	ui.action->setText(button.value("label").toString());
	ui.action->setEnabled(button.value("enabled").toBool());
	ui.pressed = button.value("action").toString();

	if (ui.panel->isVisible()) {
		place_panel();
		// Rows shown or hidden above settle their layout only once events run.
		QTimer::singleShot(0, ui.panel, [] { place_panel(); });
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
	ui.shown.clear();
	render();
	place_panel();
	ui.panel->show();
	// Showing polishes the panel with the theme, which can change its size.
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
		if (!ui.active)
			return;
		callback(ui.host.ctx, box->itemData(index).toString().toUtf8().constData());
		rerender();
	});
}

Panel *build_panel()
{
	auto *panel = new Panel();
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
		if (!ui.active)
			return;
		ui.host.set_track(ui.host.ctx, ui.tracks->itemData(index).toInt());
		rerender();
	});
	audio->addWidget(ui.audio_title);
	audio->addStretch(1);
	audio->addWidget(ui.tracks);
	rows->addLayout(audio);

	ui.audio_line = label("dstreamy-audio-line", panel, "text-muted");
	ui.audio_line->setWordWrap(true);
	rows->addWidget(ui.audio_line);
	ui.manage = button("dstreamy-manage", panel);
	QObject::connect(ui.manage, &QPushButton::clicked, ui.manage, [] {
		if (!ui.active)
			return;
		ui.host.request_take_over(ui.host.ctx);
		rerender();
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
			if (!ui.active)
				return;
			ui.host.confirm_take_over(ui.host.ctx, yes);
			rerender();
		});
	};
	answer(ui.confirm_ok, true);
	answer(ui.confirm_cancel, false);
	answers->addStretch(1);
	answers->addWidget(ui.confirm_cancel);
	answers->addWidget(ui.confirm_ok);
	confirm->addLayout(answers);
	rows->addWidget(ui.confirm_row);

	ui.scroll = new QScrollArea(panel);
	ui.scroll->setObjectName(QStringLiteral("dstreamy-sources-scroll"));
	ui.scroll->setFrameShape(QFrame::NoFrame);
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
	QObject::connect(ui.action, &QPushButton::clicked, ui.action, [] { press(); });
	rows->addWidget(ui.action, 0, Qt::AlignRight);
	panel->setFixedWidth(320);
	return panel;
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

void on_exit()
{
	exited = true;
	teardown();
}

} // namespace

uint32_t dstreamy_ui_abi_version(void)
{
	return DSTREAMY_UI_ABI_VERSION;
}

bool dstreamy_ui_load(const struct dstreamy_ui_host *host)
{
	if (ui.active || !host || host->abi_version != DSTREAMY_UI_ABI_VERSION || !host->view_json ||
	    !host->free_string || !host->start || !host->stop || !host->load_lists || !host->select_guild ||
	    !host->select_channel || !host->set_track || !host->request_take_over || !host->confirm_take_over ||
	    !host->set_source || !host->panel_closed)
		return false;
	QMainWindow *window = dstreamy_ui::main_window();
	if (!window)
		return false;

	ui.host = *host;
	ui.window = window;
	ui.panel = build_panel();

	ui.status = new QToolButton();
	ui.status->setObjectName(QStringLiteral("dstreamy-status"));
	ui.status->setAutoRaise(true);
	QObject::connect(ui.status, &QToolButton::clicked, ui.status, [] { toggle_panel(); });
	window->statusBar()->addPermanentWidget(ui.status);

	ui.timer = new QTimer(ui.status);
	ui.timer->setInterval(poll_ms);
	QObject::connect(ui.timer, &QTimer::timeout, ui.status, [] { render(); });
	ui.timer->start();

	exited = false;
	dstreamy_ui::watch_exit(on_exit);
	ui.active = true;
	render();
	return true;
}

bool dstreamy_ui_open_panel(void)
{
	return open_panel();
}

void dstreamy_ui_unload(void)
{
	// After OBS's exit event the frontend API is gone, so nothing may call it.
	if (exited)
		return;
	if (ui.window)
		dstreamy_ui::unwatch_exit();
	teardown();
}
