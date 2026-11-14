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
#include <QIcon>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QKeyEvent>
#include <QLabel>
#include <QMainWindow>
#include <QMessageBox>
#include <QPainter>
#include <QPointer>
#include <QPushButton>
#include <QScreen>
#include <QScrollArea>
#include <QScrollBar>
#include <QSignalBlocker>
#include <QStatusBar>
#include <QStyle>
#include <QSvgRenderer>
#include <QTextDocument>
#include <QTimer>
#include <QToolButton>
#include <QVBoxLayout>

#include <cstring>
#include <memory>
#include <utility>

static void init_icons()
{
	Q_INIT_RESOURCE(icons);
}

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
void restyle_status();

// The dot, icons and counters are child labels, so the button's size comes from their layout.
class StatusButton : public QToolButton {
public:
	StatusButton()
	{
		setObjectName(QStringLiteral("dstreamy-status"));
		setAutoRaise(true);
		setAccessibleName(QStringLiteral("D-Streamy"));
		auto *row = new QHBoxLayout(this);
		row->setContentsMargins(6, 0, 6, 0);
		row->setSpacing(4);
	}

	QSize sizeHint() const override
	{
		QSize inner = layout()->sizeHint();
		return QSize(inner.width(), qMax(inner.height(), QToolButton::sizeHint().height()));
	}
	QSize minimumSizeHint() const override { return sizeHint(); }

protected:
	void changeEvent(QEvent *event) override
	{
		QToolButton::changeEvent(event);
		if (event->type() == QEvent::StyleChange || event->type() == QEvent::PaletteChange ||
		    event->type() == QEvent::FontChange)
			restyle_status();
	}

	bool event(QEvent *event) override
	{
#if QT_VERSION >= QT_VERSION_CHECK(6, 6, 0)
		if (event->type() == QEvent::DevicePixelRatioChange)
			restyle_status();
#endif
		return QToolButton::event(event);
	}
};

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
	QPointer<StatusButton> status;
	QPointer<QLabel> status_dot;
	QPointer<QLabel> status_duration;
	QPointer<QToolButton> status_action;
	QPointer<QMessageBox> confirm_box;
	QJsonObject control;
	QStringList status_ids;
	int style_generation = 0;
	QPointer<Panel> panel;
	QPointer<QTimer> timer;
	QPointer<QLabel> mark;
	QPointer<ElidedLabel> state_line;
	QPointer<ElidedLabel> state_fix;
	QPointer<QPushButton> login;
	QPointer<QWidget> account_row;
	QPointer<QLabel> account_label;
	QPointer<QComboBox> accounts;
	QPointer<QPushButton> logout;
	QPointer<QWidget> logout_row;
	QPointer<QLabel> logout_text;
	QPointer<QPushButton> logout_ok;
	QPointer<QPushButton> logout_cancel;
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
	QString login_action;
	QString account;
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

void restyle_status()
{
	ui.style_generation++;
	ui.shown.clear();
	rerender();
}

// The OBS theme class for a level or dot state, and its colour where the theme has no such class.
struct Tint {
	const char *klass;
	const char *fallback;
};

Tint level_tint(const QString &level)
{
	if (level == QLatin1String("error"))
		return {"text-danger", "#c01c37"};
	if (level == QLatin1String("warn"))
		return {"text-warning", "#b88a16"};
	return {"", nullptr};
}

Tint state_tint(const QString &state)
{
	if (state == QLatin1String("live"))
		return {"text-success", "#25a231"};
	if (state == QLatin1String("error"))
		return level_tint(QStringLiteral("error"));
	if (state == QLatin1String("busy") || state == QLatin1String("degraded"))
		return level_tint(QStringLiteral("warn"));
	return {"text-muted", nullptr};
}

void set_class(QLabel *l, const QString &klass)
{
	l->setProperty("class", klass);
	// A style sheet change makes Qt match the class rules again, as OBS's setClasses does.
	l->setStyleSheet(QStringLiteral("/* */"));
	l->setStyleSheet(QString());
}

// Applies the theme's class to `l` and returns the colour it draws text in.
QColor apply_tint(QLabel *l, const Tint &tint)
{
	QString klass = QString::fromLatin1(tint.klass);
	set_class(l, QString());
	QColor plain = l->palette().color(l->foregroundRole());
	if (klass.isEmpty())
		return plain;
	set_class(l, klass);
	QColor color = l->palette().color(l->foregroundRole());
	if (color == plain) {
		if (tint.fallback) {
			color = QColor(QString::fromLatin1(tint.fallback));
		} else {
			color = plain;
			color.setAlphaF(0.55f);
		}
		l->setStyleSheet(QStringLiteral("color: rgba(%1, %2, %3, %4);")
					 .arg(color.red())
					 .arg(color.green())
					 .arg(color.blue())
					 .arg(color.alpha()));
	}
	return color;
}

QPixmap icon_pixmap(const QString &name, int size, qreal ratio, const QColor &color)
{
	QPixmap pixmap(QSize(size, size) * ratio);
	pixmap.setDevicePixelRatio(ratio);
	pixmap.fill(Qt::transparent);
	QSvgRenderer svg(QStringLiteral(":/dstreamy/icons/%1.svg").arg(name));
	QPainter painter(&pixmap);
	painter.setRenderHint(QPainter::Antialiasing);
	svg.render(&painter, QRectF(0, 0, size, size));
	painter.setCompositionMode(QPainter::CompositionMode_SourceIn);
	painter.fillRect(QRectF(0, 0, size, size), color);
	return pixmap;
}

QPixmap dot_pixmap(int size, qreal ratio, const QColor &color)
{
	QPixmap pixmap(QSize(size, size) * ratio);
	pixmap.setDevicePixelRatio(ratio);
	pixmap.fill(Qt::transparent);
	QPainter painter(&pixmap);
	painter.setRenderHint(QPainter::Antialiasing);
	painter.setPen(Qt::NoPen);
	painter.setBrush(color);
	qreal d = size * 0.6;
	painter.drawEllipse(QRectF((size - d) / 2, (size - d) / 2, d, d));
	return pixmap;
}

QLabel *status_label(const QString &name)
{
	auto *l = new QLabel(ui.status);
	l->setObjectName(name);
	l->setTextFormat(Qt::PlainText);
	l->setAttribute(Qt::WA_TransparentForMouseEvents);
	l->show();
	return l;
}

void ask();

QToolButton *status_action()
{
	auto *b = new QToolButton(ui.status);
	b->setObjectName(QStringLiteral("dstreamy-status-action"));
	b->setCursor(Qt::PointingHandCursor);
	// Clicks on the button, enabled or not, never reach the item, which opens the panel.
	b->setAttribute(Qt::WA_NoMousePropagation);
	QObject::connect(b, &QToolButton::clicked, b, [] { guarded("start_stop", ask); });
	b->show();
	return b;
}

// One icon and one text label per counter, rebuilt only when the counters themselves change.
void build_counters(const QJsonArray &counters)
{
	QStringList ids;
	for (const QJsonValue v : counters)
		ids << v.toObject().value("id").toString();
	if (ui.status_dot && ids == ui.status_ids)
		return;
	ui.status_ids = ids;
	auto *row = static_cast<QHBoxLayout *>(ui.status->layout());
	while (QLayoutItem *item = row->takeAt(0)) {
		delete item->widget();
		delete item;
	}
	ui.status_dot = status_label(QStringLiteral("dstreamy-status-dot"));
	row->addWidget(ui.status_dot);
	for (const QString &id : ids) {
		row->addSpacing(6);
		row->addWidget(status_label(QStringLiteral("dstreamy-status-icon-") + id));
		row->addWidget(status_label(QStringLiteral("dstreamy-status-text-") + id));
	}
	row->addSpacing(8);
	auto *divider = new QFrame(ui.status);
	divider->setObjectName(QStringLiteral("dstreamy-status-divider"));
	divider->setFrameShape(QFrame::VLine);
	divider->setFrameShadow(QFrame::Plain);
	divider->setFixedHeight(ui.status->fontMetrics().height());
	divider->show();
	row->addWidget(divider);
	row->addSpacing(6);
	ui.status_action = status_action();
	row->addWidget(ui.status_action);
	ui.status_duration = status_label(QStringLiteral("dstreamy-status-duration"));
	row->addWidget(ui.status_duration);
}

// Tinting restyles the label, so it happens only when what it shows or the theme changed.
bool tint_due(QWidget *l, const QString &look, int size, qreal ratio)
{
	QString key = QStringLiteral("%1|%2|%3|%4").arg(look).arg(size).arg(ratio).arg(ui.style_generation);
	if (l->property("tintKey").toString() == key)
		return false;
	l->setProperty("tintKey", key);
	return true;
}

void render_status(const QJsonObject &status)
{
	QJsonArray counters = status.value("counters").toArray();
	build_counters(counters);
	if (!ui.status_dot || !ui.status_duration)
		return;
	int size = ui.status->fontMetrics().height();
	qreal ratio = ui.status->devicePixelRatioF();
	QString state = status.value("state").toString();
	if (tint_due(ui.status_dot, state, size, ratio)) {
		QColor dot = apply_tint(ui.status_dot, state_tint(state));
		ui.status_dot->setPixmap(dot_pixmap(size, ratio, dot));
		ui.status_dot->setProperty("state", state);
		ui.status_dot->setProperty("color", dot.name(QColor::HexArgb));
	}
	for (const QJsonValue v : counters) {
		QJsonObject c = v.toObject();
		QString id = c.value("id").toString();
		QString level = c.value("level").toString();
		auto *icon = ui.status->findChild<QLabel *>(QStringLiteral("dstreamy-status-icon-") + id);
		auto *text = ui.status->findChild<QLabel *>(QStringLiteral("dstreamy-status-text-") + id);
		if (!icon || !text)
			continue;
		text->setText(c.value("text").toString());
		text->setAccessibleName(c.value("label").toString());
		QString name = c.value("icon").toString();
		if (!tint_due(text, level + QLatin1Char('|') + name, size, ratio))
			continue;
		text->setProperty("level", level);
		QColor color = apply_tint(text, level_tint(level));
		icon->setPixmap(icon_pixmap(name, size, ratio, color));
		icon->setProperty("icon", name);
		icon->setProperty("level", level);
		icon->setProperty("color", color.name(QColor::HexArgb));
	}
	QString duration = status.value("duration").toString();
	ui.status_duration->setText(duration);
	ui.status_duration->setVisible(!duration.isEmpty());
	QString tip = status.value("tooltip").toString();
	ui.status->setToolTip(plain_tip(tip));
	ui.status->setAccessibleDescription(tip);
	ui.status->updateGeometry();
}

bool still_offered(const QString &action, const QString &question)
{
	return ui.control.value("enabled").toBool() && ui.control.value("action").toString() == action &&
	       ui.control.value("confirm").toObject().value("text").toString() == question;
}

void render_control(const QJsonObject &control)
{
	ui.control = control;
	QToolButton *b = ui.status_action;
	if (!b)
		return;
	b->setVisible(!control.isEmpty());
	if (auto *divider = ui.status->findChild<QFrame *>(QStringLiteral("dstreamy-status-divider")))
		divider->setVisible(!control.isEmpty());
	if (control.isEmpty())
		return;
	QString icon = control.value("icon").toString();
	int size = ui.status->fontMetrics().height();
	qreal ratio = ui.status->devicePixelRatioF();
	if (tint_due(b, icon, size, ratio)) {
		auto *probe = b->findChild<QLabel *>(QStringLiteral("dstreamy-status-action-tint"));
		if (!probe) {
			probe = new QLabel(b);
			probe->setObjectName(QStringLiteral("dstreamy-status-action-tint"));
			probe->hide();
		}
		bool stop = icon == QLatin1String("player-stop");
		QColor color = apply_tint(probe, stop ? level_tint(QStringLiteral("error"))
						      : state_tint(QStringLiteral("live")));
		QColor text = b->palette().color(QPalette::WindowText);
		QColor faded = text;
		faded.setAlphaF(0.4f);
		QColor idle = text;
		idle.setAlphaF(0.12f);
		auto rgba = [](const QColor &c) {
			return QStringLiteral("rgba(%1, %2, %3, %4)")
				.arg(c.red())
				.arg(c.green())
				.arg(c.blue())
				.arg(c.alpha());
		};
		b->setStyleSheet(QStringLiteral("QToolButton { background: %1; border: none; border-radius: %4px; "
						"padding: 1px %5px; }"
						"QToolButton:hover { background: %2; }"
						"QToolButton:pressed { background: %3; }"
						"QToolButton:disabled { background: %6; }")
					 .arg(rgba(color), rgba(color.lighter(115)), rgba(color.darker(120)))
					 .arg(size / 2)
					 .arg(size / 2)
					 .arg(rgba(idle)));
		QIcon pixmaps;
		pixmaps.addPixmap(icon_pixmap(icon, size, ratio, Qt::white), QIcon::Normal);
		pixmaps.addPixmap(icon_pixmap(icon, size, ratio, faded), QIcon::Disabled);
		b->setIcon(pixmaps);
		b->setIconSize(QSize(size * 4 / 5, size * 4 / 5));
		b->setProperty("iconName", icon);
		b->setProperty("color", color.name(QColor::HexArgb));
	}
	b->setEnabled(control.value("enabled").toBool());
	b->setProperty("action", control.value("action").toString());
	QString tip = control.value("tooltip").toString();
	b->setToolTip(plain_tip(tip));
	b->setAccessibleName(tip);
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

// The last entry is "Add account…", told apart by its "add" role.
void fill_accounts(const QJsonObject &row)
{
	QJsonArray items = row.value("items").toArray();
	ui.account_row->setVisible(!items.isEmpty());
	ui.account.clear();
	if (items.isEmpty()) {
		ui.logout_row->setVisible(false);
		return;
	}
	ui.account_label->setText(row.value("label").toString());
	QSignalBlocker block(ui.accounts);
	QString key = row.value("add").toString();
	for (const QJsonValue v : items)
		key += QLatin1Char('\n') + v.toObject().value("name").toString() + QLatin1Char('\t') +
		       v.toObject().value("user").toString();
	if (ui.accounts->property("filled").toString() != key) {
		ui.accounts->clear();
		for (const QJsonValue v : items) {
			QJsonObject a = v.toObject();
			QString name = a.value("name").toString();
			QString user = a.value("user").toString();
			ui.accounts->addItem(name, name);
			if (!user.isEmpty())
				ui.accounts->setItemData(ui.accounts->count() - 1, user, Qt::ToolTipRole);
		}
		ui.accounts->addItem(row.value("add").toString(), QString());
		ui.accounts->setItemData(ui.accounts->count() - 1, QStringLiteral("add"), Qt::UserRole + 1);
		ui.accounts->setProperty("filled", key);
	}
	int current = -1;
	for (int i = 0; i < items.size(); i++)
		if (items[i].toObject().value("selected").toBool()) {
			current = i;
			ui.account = items[i].toObject().value("name").toString();
		}
	ui.accounts->setCurrentIndex(current);
	bool enabled = row.value("enabled").toBool();
	ui.accounts->setEnabled(enabled);
	ui.accounts->setToolTip(plain_tip(row.value("tooltip").toString()));
	ui.logout->setText(row.value("logout").toString());
	ui.logout->setEnabled(enabled && !ui.account.isEmpty());
	ui.logout->setToolTip(plain_tip(row.value("logout_tooltip").toString()));
	QJsonValue confirm = row.value("confirm");
	bool confirming = confirm.isObject();
	ui.logout->setVisible(!confirming);
	ui.logout_row->setVisible(confirming);
	if (confirming) {
		QJsonObject c = confirm.toObject();
		ui.logout_text->setText(c.value("text").toString());
		ui.logout_ok->setText(c.value("ok").toString());
		ui.logout_cancel->setText(c.value("cancel").toString());
	}
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
	QList<QWidget *> order{ui.login,     ui.accounts, ui.logout,   ui.logout_cancel,
			       ui.logout_ok, ui.guilds,   ui.channels, ui.tracks};
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
		render_status(view.value("status").toObject());
		render_control(view.value("control").toObject());
	}
	if (ui.confirm_box && !still_offered(ui.confirm_box->property("action").toString(),
					     ui.confirm_box->property("question").toString()))
		ui.confirm_box->reject();

	ui.panel->setProperty("tone", view.value("tone").toString());
	ui.mark->setText(view.value("mark").toString());
	ui.mark->setStyleSheet(color);
	QString tooltip = view.value("state_tooltip").toString();
	ui.state_line->show_text(view.value("state_line").toString(), tooltip);
	QString fix = view.value("state_fix").toString();
	ui.state_fix->show_text(fix, tooltip);
	ui.state_fix->setVisible(!fix.isEmpty());
	QJsonObject login = view.value("login").toObject();
	ui.login->setText(login.value("label").toString());
	ui.login->setVisible(!login.isEmpty());
	ui.login_action = login.value("action").toString();
	fill_accounts(view.value("account").toObject());

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

	if (ui.panel->isVisible()) {
		place_panel();
		// Rows shown or hidden above settle their layout only once events run.
		QTimer::singleShot(0, ui.panel, [] { guarded("place_panel", place_panel); });
	}
}

// Acts only if the view still offers what the user confirmed.
void answered(bool yes, const QString &action, const QString &question)
{
	ui.confirm_box = nullptr;
	if (!yes || !ui.active)
		return;
	ui.shown.clear();
	render();
	if (!still_offered(action, question))
		return;
	if (action == QLatin1String("start"))
		ui.host.start(ui.host.ctx);
	else if (action == QLatin1String("stop"))
		ui.host.stop(ui.host.ctx);
	rerender();
}

// open(), not exec(): OBS's exit event must be able to tear the dialog down.
void ask()
{
	if (!ui.active || ui.confirm_box || !ui.window)
		return;
	rerender();
	if (!ui.active || !ui.control.value("enabled").toBool())
		return;
	QString action = ui.control.value("action").toString();
	QJsonObject question = ui.control.value("confirm").toObject();
	if (ui.panel && ui.panel->isVisible())
		ui.panel->hide();
	auto *box = new QMessageBox(ui.window);
	box->setObjectName(QStringLiteral("dstreamy-stream-confirm"));
	box->setWindowTitle(QStringLiteral("D-Streamy"));
	box->setIcon(QMessageBox::Question);
	box->setTextFormat(Qt::PlainText);
	box->setText(question.value("text").toString());
	QPushButton *ok = box->addButton(question.value("ok").toString(), QMessageBox::AcceptRole);
	ok->setObjectName(QStringLiteral("dstreamy-stream-confirm-ok"));
	QPushButton *cancel = box->addButton(question.value("cancel").toString(), QMessageBox::RejectRole);
	cancel->setObjectName(QStringLiteral("dstreamy-stream-confirm-cancel"));
	box->setDefaultButton(question.value("default_ok").toBool() ? ok : cancel);
	box->setEscapeButton(cancel);
	box->setProperty("action", action);
	box->setProperty("question", box->text());
	QObject::connect(box, &QMessageBox::finished, box, [box, ok] {
		if (ui.confirm_box != box)
			return;
		box->deleteLater();
		guarded("confirm_start_stop", [box, ok] {
			answered(box->clickedButton() == ok, box->property("action").toString(),
				 box->property("question").toString());
		});
	});
	ui.confirm_box = box;
	box->open();
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
	ui.login = button("dstreamy-login", panel);
	ui.login->setVisible(false);
	QObject::connect(ui.login, &QPushButton::clicked, ui.login, [] {
		guarded("login", [] {
			if (!ui.active)
				return;
			if (ui.login_action == QLatin1String("cancel"))
				ui.host.cancel_login(ui.host.ctx);
			else if (ui.login_action == QLatin1String("login"))
				ui.host.login(ui.host.ctx);
			rerender();
		});
	});
	rows->addWidget(ui.login, 0, Qt::AlignLeft);

	ui.account_row = new QWidget(panel);
	ui.account_row->setObjectName(QStringLiteral("dstreamy-account"));
	auto *account = new QHBoxLayout(ui.account_row);
	account->setContentsMargins(0, 0, 0, 0);
	ui.account_label = label("dstreamy-account-label", ui.account_row);
	ui.accounts = new QComboBox(ui.account_row);
	ui.accounts->setObjectName(QStringLiteral("dstreamy-accounts"));
	ui.account_label->setBuddy(ui.accounts);
	ui.logout = button("dstreamy-logout", ui.account_row);
	account->addWidget(ui.account_label);
	account->addWidget(ui.accounts, 1);
	account->addWidget(ui.logout);
	ui.account_row->setVisible(false);
	rows->addWidget(ui.account_row);
	QObject::connect(ui.accounts, &QComboBox::activated, ui.accounts, [](int index) {
		guarded("select_account", [index] {
			if (!ui.active)
				return;
			if (ui.accounts->itemData(index, Qt::UserRole + 1).toString() == QLatin1String("add")) {
				ui.host.add_account(ui.host.ctx);
			} else {
				QByteArray name = ui.accounts->itemData(index).toString().toUtf8();
				if (!name.isEmpty() && QString::fromUtf8(name) != ui.account)
					ui.host.select_account(ui.host.ctx, name.constData());
			}
			rerender();
		});
	});
	ui.logout_row = new QWidget(panel);
	ui.logout_row->setObjectName(QStringLiteral("dstreamy-logout-confirm"));
	auto *logout_confirm = new QHBoxLayout(ui.logout_row);
	logout_confirm->setContentsMargins(0, 0, 0, 0);
	ui.logout_text = label("dstreamy-logout-text", ui.logout_row, "text-warning");
	ui.logout_cancel = button("dstreamy-logout-cancel", ui.logout_row);
	ui.logout_ok = button("dstreamy-logout-ok", ui.logout_row);
	logout_confirm->addWidget(ui.logout_text, 1);
	logout_confirm->addWidget(ui.logout_cancel);
	logout_confirm->addWidget(ui.logout_ok);
	ui.logout_row->setVisible(false);
	rows->addWidget(ui.logout_row);
	for (auto [b, choice] : {std::pair{ui.logout_ok.data(), true}, std::pair{ui.logout_cancel.data(), false}}) {
		const bool yes = choice;
		QObject::connect(b, &QPushButton::clicked, b, [yes] {
			guarded("confirm_logout", [yes] {
				if (!ui.active)
					return;
				ui.host.confirm_logout(ui.host.ctx, yes);
				rerender();
			});
		});
	}
	QObject::connect(ui.logout, &QPushButton::clicked, ui.logout, [] {
		guarded("logout_account", [] {
			if (!ui.active || ui.account.isEmpty())
				return;
			QByteArray name = ui.account.toUtf8();
			ui.host.logout_account(ui.host.ctx, name.constData());
			rerender();
		});
	});

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
	QObject::connect(ui.action, &QPushButton::clicked, ui.action, [] { guarded("start_stop", ask); });
	rows->addWidget(ui.action, 0, Qt::AlignRight);
	panel->setFixedWidth(320);
	return owner.release();
}

void teardown()
{
	ui.active = false;
	delete ui.timer;
	if (ui.confirm_box) {
		QObject::disconnect(ui.confirm_box, nullptr, nullptr, nullptr);
		delete ui.confirm_box.data();
	}
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
	    !host->set_source || !host->panel_closed || !host->release_track || !host->login || !host->cancel_login ||
	    !host->select_account || !host->add_account || !host->logout_account || !host->confirm_logout || failing)
		return false;
	QMainWindow *window = dstreamy_ui::main_window();
	if (!window)
		return false;
	exited = false;

	ui.host = *host;
	ui.window = window;
	ui.panel = build_panel();
	ui.panel->ensurePolished();

	init_icons();
	ui.status = new StatusButton();
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
