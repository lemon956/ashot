import Cairo from 'gi://cairo';
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

const _ = text => text;

const SHELL_DBUS_NAME = 'io.github.ashot.Shell';
const SHELL_DBUS_PATH = '/io/github/ashot/Shell';
const SHELL_DBUS_INTERFACE = 'io.github.ashot.Shell';

const APP_DBUS_NAME = 'io.github.ashot.Service';
const APP_DBUS_PATH = '/io/github/ashot/App';
const APP_DBUS_INTERFACE = 'io.github.ashot.App';

const TOOL_TEXT = 'text';
const TOOL_ARROW = 'arrow';
const TOOL_BRUSH = 'brush';
const TOOL_RECTANGLE = 'rectangle';
const TOOL_MOSAIC = 'mosaic';
const TOOL_SELECT = 'select';

const TOOL_LABELS = new Map([
    [TOOL_SELECT, _('Select')],
    [TOOL_TEXT, _('Text')],
    [TOOL_ARROW, _('Arrow')],
    [TOOL_BRUSH, _('Brush')],
    [TOOL_RECTANGLE, _('Rect')],
    [TOOL_MOSAIC, _('Mosaic')],
]);

const COLOR_PRESETS = [
    {name: _('Red'), rgba: {r: 232, g: 62, b: 38, a: 255}},
    {name: _('Yellow'), rgba: {r: 241, g: 196, b: 15, a: 255}},
    {name: _('Blue'), rgba: {r: 52, g: 152, b: 219, a: 255}},
    {name: _('Green'), rgba: {r: 46, g: 204, b: 113, a: 255}},
    {name: _('White'), rgba: {r: 255, g: 255, b: 255, a: 255}},
];

const STROKE_PRESETS = [2, 4, 8, 12];
const TEXT_SIZE_PRESETS = [16, 20, 28, 36];
const MOSAIC_PRESETS = [8, 16, 24];
const TEXT_WEIGHTS = ['Regular', 'Semibold', 'Bold'];

const SHELL_BRIDGE_IFACE = `
<node>
  <interface name="io.github.ashot.Shell">
    <method name="StartCapture"/>
  </interface>
</node>`;

function clamp(value, min, max) {
    return Math.min(Math.max(value, min), max);
}

function rectFromPoints(start, end) {
    return {
        x: Math.min(start.x, end.x),
        y: Math.min(start.y, end.y),
        width: Math.abs(end.x - start.x),
        height: Math.abs(end.y - start.y),
    };
}

function rgbaString(color, alpha = color.a / 255) {
    return `rgba(${color.r}, ${color.g}, ${color.b}, ${alpha})`;
}

function copyColor(color) {
    return {r: color.r, g: color.g, b: color.b, a: color.a};
}

function setCairoColor(cr, color, alphaScale = 1.0) {
    cr.setSourceRGBA(color.r / 255, color.g / 255, color.b / 255, (color.a / 255) * alphaScale);
}

function colorButtonLabel(colorPreset) {
    return colorPreset.name;
}

function toAnnotationRecord(annotation) {
    const data = {};
    switch (annotation.kind) {
    case TOOL_TEXT:
        data.Text = {
            origin: annotation.origin,
            text: annotation.text,
            style: {
                size: annotation.size,
                weight: annotation.weight,
                color: annotation.color,
            },
        };
        break;
    case TOOL_ARROW:
        data.Arrow = {
            start: annotation.start,
            end: annotation.end,
            color: annotation.color,
            stroke_width: annotation.strokeWidth,
        };
        break;
    case TOOL_BRUSH:
        data.Brush = {
            points: annotation.points,
            color: annotation.color,
            stroke_width: annotation.strokeWidth,
        };
        break;
    case TOOL_RECTANGLE:
        data.Rectangle = {
            rect: annotation.rect,
            color: annotation.color,
            stroke_width: annotation.strokeWidth,
        };
        break;
    case TOOL_MOSAIC:
        data.Mosaic = {
            rect: annotation.rect,
            pixel_size: annotation.pixelSize,
        };
        break;
    default:
        return null;
    }

    return {
        id: annotation.id,
        data,
    };
}

function drawArrow(cr, start, end, color, strokeWidth) {
    setCairoColor(cr, color);
    cr.setLineWidth(strokeWidth);
    cr.setLineCap(Cairo.LineCap.ROUND);
    cr.moveTo(start.x, start.y);
    cr.lineTo(end.x, end.y);
    cr.stroke();

    const angle = Math.atan2(end.y - start.y, end.x - start.x);
    const headLength = Math.max(strokeWidth * 2.6, 10);
    const left = {
        x: end.x - headLength * Math.cos(angle - Math.PI / 6),
        y: end.y - headLength * Math.sin(angle - Math.PI / 6),
    };
    const right = {
        x: end.x - headLength * Math.cos(angle + Math.PI / 6),
        y: end.y - headLength * Math.sin(angle + Math.PI / 6),
    };

    cr.moveTo(end.x, end.y);
    cr.lineTo(left.x, left.y);
    cr.moveTo(end.x, end.y);
    cr.lineTo(right.x, right.y);
    cr.stroke();
}

function drawBrush(cr, points, color, strokeWidth) {
    if (!points || points.length < 2)
        return;

    setCairoColor(cr, color);
    cr.setLineWidth(strokeWidth);
    cr.setLineCap(Cairo.LineCap.ROUND);
    cr.moveTo(points[0].x, points[0].y);
    for (let i = 1; i < points.length; i++)
        cr.lineTo(points[i].x, points[i].y);
    cr.stroke();
}

function drawRectangle(cr, rect, color, strokeWidth) {
    setCairoColor(cr, color);
    cr.setLineWidth(strokeWidth);
    cr.rectangle(rect.x, rect.y, rect.width, rect.height);
    cr.stroke();
}

function drawText(cr, annotation) {
    setCairoColor(cr, annotation.color);
    const weight = annotation.weight === 'Bold'
        ? Cairo.FontWeight.BOLD
        : annotation.weight === 'Semibold'
            ? Cairo.FontWeight.BOLD
            : Cairo.FontWeight.NORMAL;
    cr.selectFontFace('Cantarell', Cairo.FontSlant.NORMAL, weight);
    cr.setFontSize(annotation.size);

    const lines = annotation.text.split('\n');
    let y = annotation.origin.y;
    for (const line of lines) {
        cr.moveTo(annotation.origin.x, y);
        cr.showText(line);
        y += annotation.size * 1.25;
    }
}

function drawMosaicPreview(cr, rect, pixelSize) {
    cr.save();
    cr.rectangle(rect.x, rect.y, rect.width, rect.height);
    cr.clip();
    cr.setLineWidth(1);
    cr.setSourceRGBA(1, 1, 1, 0.22);
    for (let x = rect.x; x <= rect.x + rect.width; x += pixelSize) {
        cr.moveTo(x, rect.y);
        cr.lineTo(x, rect.y + rect.height);
    }
    for (let y = rect.y; y <= rect.y + rect.height; y += pixelSize) {
        cr.moveTo(rect.x, y);
        cr.lineTo(rect.x + rect.width, y);
    }
    cr.stroke();
    cr.restore();

    cr.save();
    cr.setSourceRGBA(0, 0, 0, 0.16);
    cr.rectangle(rect.x, rect.y, rect.width, rect.height);
    cr.fill();
    cr.restore();
}

class CaptureSession {
    constructor(extension) {
        this._extension = extension;
        this._selection = null;
        this._annotations = [];
        this._draft = null;
        this._dragOrigin = null;
        this._dragMode = null;
        this._tool = TOOL_SELECT;
        this._colorIndex = 0;
        this._strokeIndex = 1;
        this._textSizeIndex = 1;
        this._mosaicIndex = 1;
        this._textWeightIndex = 2;
        this._saving = false;

        this._overlay = null;
        this._canvas = null;
        this._toolbar = null;
        this._hint = null;
        this._textEntry = null;
        this._infoLabel = null;
        this._toolButtons = new Map();
        this._colorButton = null;
        this._strokeButton = null;
        this._sizeButton = null;
        this._weightButton = null;
        this._grab = null;
    }

    open() {
        this._overlay = new St.Widget({
            style_class: 'ashot-overlay',
            reactive: true,
            can_focus: true,
            track_hover: true,
            layout_manager: new Clutter.BinLayout(),
            x: 0,
            y: 0,
            width: global.stage.width,
            height: global.stage.height,
        });

        this._canvas = new St.DrawingArea({
            reactive: false,
            x_expand: true,
            y_expand: true,
        });
        this._canvas.connect('repaint', this._repaint.bind(this));
        this._canvas.set_size(global.stage.width, global.stage.height);
        this._overlay.add_child(this._canvas);

        this._toolbar = this._buildToolbar();
        this._toolbar.hide();
        this._overlay.add_child(this._toolbar);

        this._hint = new St.Label({
            style_class: 'ashot-hint',
            text: _('Drag to select an area. Esc cancels, Enter saves.'),
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.START,
            y: 28,
        });
        this._overlay.add_child(this._hint);

        this._overlay.connect('button-press-event', this._onButtonPress.bind(this));
        this._overlay.connect('motion-event', this._onMotion.bind(this));
        this._overlay.connect('button-release-event', this._onButtonRelease.bind(this));
        this._overlay.connect('key-press-event', this._onKeyPress.bind(this));

        Main.uiGroup.add_child(this._overlay);
        this._grab = Main.pushModal(this._overlay);
        this._overlay.grab_key_focus();
        this._queueRepaint();
    }

    destroy() {
        if (this._grab) {
            Main.popModal(this._grab);
            this._grab = null;
        }

        if (this._overlay) {
            this._overlay.destroy();
            this._overlay = null;
        }
    }

    _buildToolbar() {
        const toolbar = new St.BoxLayout({
            style_class: 'ashot-toolbar',
            vertical: false,
            visible: true,
        });

        toolbar.add_child(this._button(_('Reselect'), () => this._resetSelection()));

        this._toolButtons.set(TOOL_TEXT, this._button(TOOL_LABELS.get(TOOL_TEXT), () => this._setTool(TOOL_TEXT)));
        this._toolButtons.set(TOOL_ARROW, this._button(TOOL_LABELS.get(TOOL_ARROW), () => this._setTool(TOOL_ARROW)));
        this._toolButtons.set(TOOL_BRUSH, this._button(TOOL_LABELS.get(TOOL_BRUSH), () => this._setTool(TOOL_BRUSH)));
        this._toolButtons.set(TOOL_RECTANGLE, this._button(TOOL_LABELS.get(TOOL_RECTANGLE), () => this._setTool(TOOL_RECTANGLE)));
        this._toolButtons.set(TOOL_MOSAIC, this._button(TOOL_LABELS.get(TOOL_MOSAIC), () => this._setTool(TOOL_MOSAIC)));

        for (const button of this._toolButtons.values())
            toolbar.add_child(button);

        this._colorButton = this._button('', () => this._cycleColor());
        toolbar.add_child(this._colorButton);

        this._strokeButton = this._button('', () => this._cycleStrokeWidth());
        toolbar.add_child(this._strokeButton);

        this._sizeButton = this._button('', () => this._cycleTextSize());
        toolbar.add_child(this._sizeButton);

        this._weightButton = this._button('', () => this._cycleTextWeight());
        toolbar.add_child(this._weightButton);

        this._textEntry = new St.Entry({
            style_class: 'ashot-entry',
            hint_text: _('Text'),
            text: _('Note'),
            can_focus: true,
        });
        toolbar.add_child(this._textEntry);

        this._infoLabel = new St.Label({
            style_class: 'ashot-pill',
            text: _('No area'),
            y_align: Clutter.ActorAlign.CENTER,
        });
        toolbar.add_child(this._infoLabel);

        toolbar.add_child(this._button(_('Save'), () => void this._save()));
        toolbar.add_child(this._button(_('Cancel'), () => this._extension.endCapture()));

        this._refreshToolbarState();
        return toolbar;
    }

    _button(label, callback) {
        const button = new St.Button({
            style_class: 'ashot-toolbar-button',
            label,
            can_focus: true,
            reactive: true,
        });
        button.connect('clicked', callback);
        return button;
    }

    _refreshToolbarState() {
        for (const [tool, button] of this._toolButtons.entries()) {
            if (tool === this._tool)
                button.add_style_class_name('active');
            else
                button.remove_style_class_name('active');
        }

        const colorPreset = COLOR_PRESETS[this._colorIndex];
        this._colorButton.set_label(colorButtonLabel(colorPreset));
        this._colorButton.set_style(`background-color: ${rgbaString(colorPreset.rgba, 0.24)}; color: white;`);
        this._strokeButton.set_label(`W${STROKE_PRESETS[this._strokeIndex]}`);
        this._sizeButton.set_label(`T${TEXT_SIZE_PRESETS[this._textSizeIndex]}`);
        this._weightButton.set_label(TEXT_WEIGHTS[this._textWeightIndex]);
        if (this._selection)
            this._infoLabel.set_text(`${Math.round(this._selection.width)}×${Math.round(this._selection.height)}`);
        else
            this._infoLabel.set_text(_('No area'));
    }

    _setTool(tool) {
        this._tool = tool;
        this._hint.set_text(this._hintForTool());
        this._refreshToolbarState();
    }

    _cycleColor() {
        this._colorIndex = (this._colorIndex + 1) % COLOR_PRESETS.length;
        this._queueRepaint();
        this._refreshToolbarState();
    }

    _cycleStrokeWidth() {
        this._strokeIndex = (this._strokeIndex + 1) % STROKE_PRESETS.length;
        this._queueRepaint();
        this._refreshToolbarState();
    }

    _cycleTextSize() {
        this._textSizeIndex = (this._textSizeIndex + 1) % TEXT_SIZE_PRESETS.length;
        this._refreshToolbarState();
    }

    _cycleTextWeight() {
        this._textWeightIndex = (this._textWeightIndex + 1) % TEXT_WEIGHTS.length;
        this._refreshToolbarState();
    }

    _hintForTool() {
        switch (this._tool) {
        case TOOL_TEXT:
            return _('Click inside the selection to place text.');
        case TOOL_ARROW:
            return _('Drag inside the selection to place an arrow.');
        case TOOL_BRUSH:
            return _('Drag inside the selection to draw.');
        case TOOL_RECTANGLE:
            return _('Drag inside the selection to draw a rectangle.');
        case TOOL_MOSAIC:
            return _('Drag inside the selection to pixelate part of the image.');
        default:
            return _('Drag to select an area. Esc cancels, Enter saves.');
        }
    }

    _currentColor() {
        return copyColor(COLOR_PRESETS[this._colorIndex].rgba);
    }

    _currentStrokeWidth() {
        return STROKE_PRESETS[this._strokeIndex];
    }

    _currentTextSize() {
        return TEXT_SIZE_PRESETS[this._textSizeIndex];
    }

    _currentTextWeight() {
        return TEXT_WEIGHTS[this._textWeightIndex];
    }

    _currentMosaicSize() {
        return MOSAIC_PRESETS[this._mosaicIndex];
    }

    _resetSelection() {
        this._selection = null;
        this._annotations = [];
        this._draft = null;
        this._dragOrigin = null;
        this._dragMode = null;
        this._tool = TOOL_SELECT;
        this._toolbar.hide();
        this._hint.set_text(_('Drag to select an area. Esc cancels, Enter saves.'));
        this._refreshToolbarState();
        this._queueRepaint();
    }

    _pointFromEvent(event) {
        const [x, y] = event.get_coords();
        return {
            x: clamp(x, 0, global.stage.width),
            y: clamp(y, 0, global.stage.height),
        };
    }

    _pointInSelection(point) {
        if (!this._selection)
            return false;
        return point.x >= this._selection.x &&
            point.y >= this._selection.y &&
            point.x <= this._selection.x + this._selection.width &&
            point.y <= this._selection.y + this._selection.height;
    }

    _toSelectionPoint(point) {
        return {
            x: point.x - this._selection.x,
            y: point.y - this._selection.y,
        };
    }

    _beginSelection(point) {
        this._dragMode = TOOL_SELECT;
        this._dragOrigin = point;
        this._selection = rectFromPoints(point, point);
        this._annotations = [];
        this._draft = null;
        this._toolbar.hide();
        this._queueRepaint();
    }

    _beginAnnotation(point) {
        const relativePoint = this._toSelectionPoint(point);
        this._dragMode = this._tool;
        this._dragOrigin = point;

        switch (this._tool) {
        case TOOL_TEXT:
            this._draft = {kind: TOOL_TEXT, origin: relativePoint};
            break;
        case TOOL_ARROW:
            this._draft = {
                kind: TOOL_ARROW,
                start: relativePoint,
                end: relativePoint,
                color: this._currentColor(),
                strokeWidth: this._currentStrokeWidth(),
            };
            break;
        case TOOL_BRUSH:
            this._draft = {
                kind: TOOL_BRUSH,
                points: [relativePoint],
                color: this._currentColor(),
                strokeWidth: this._currentStrokeWidth(),
            };
            break;
        case TOOL_RECTANGLE:
            this._draft = {
                kind: TOOL_RECTANGLE,
                rect: {x: relativePoint.x, y: relativePoint.y, width: 0, height: 0},
                color: this._currentColor(),
                strokeWidth: this._currentStrokeWidth(),
            };
            break;
        case TOOL_MOSAIC:
            this._draft = {
                kind: TOOL_MOSAIC,
                rect: {x: relativePoint.x, y: relativePoint.y, width: 0, height: 0},
                pixelSize: this._currentMosaicSize(),
            };
            break;
        default:
            this._draft = null;
            break;
        }
    }

    _onButtonPress(_actor, event) {
        if (event.get_button() !== 1 || this._saving)
            return Clutter.EVENT_STOP;

        const point = this._pointFromEvent(event);
        if (!this._selection || this._tool === TOOL_SELECT || !this._pointInSelection(point))
            this._beginSelection(point);
        else
            this._beginAnnotation(point);

        return Clutter.EVENT_STOP;
    }

    _onMotion(_actor, event) {
        if (!this._dragOrigin || this._saving)
            return Clutter.EVENT_STOP;

        const point = this._pointFromEvent(event);
        if (this._dragMode === TOOL_SELECT) {
            this._selection = rectFromPoints(this._dragOrigin, point);
            this._refreshToolbarState();
            this._queueRepaint();
            return Clutter.EVENT_STOP;
        }

        if (!this._selection || !this._draft)
            return Clutter.EVENT_STOP;

        const relativePoint = this._toSelectionPoint(point);
        switch (this._dragMode) {
        case TOOL_ARROW:
            this._draft.end = relativePoint;
            break;
        case TOOL_BRUSH:
            this._draft.points.push(relativePoint);
            break;
        case TOOL_RECTANGLE:
            this._draft.rect = rectFromPoints(this._toSelectionPoint(this._dragOrigin), relativePoint);
            break;
        case TOOL_MOSAIC:
            this._draft.rect = rectFromPoints(this._toSelectionPoint(this._dragOrigin), relativePoint);
            break;
        default:
            break;
        }
        this._queueRepaint();
        return Clutter.EVENT_STOP;
    }

    _onButtonRelease(_actor, event) {
        if (event.get_button() !== 1 || !this._dragOrigin || this._saving)
            return Clutter.EVENT_STOP;

        const point = this._pointFromEvent(event);

        if (this._dragMode === TOOL_SELECT) {
            this._selection = rectFromPoints(this._dragOrigin, point);
            if (this._selection.width >= 4 && this._selection.height >= 4) {
                this._toolbar.show();
                this._setTool(TOOL_ARROW);
                this._positionToolbar();
            } else {
                this._resetSelection();
            }
            this._dragOrigin = null;
            this._dragMode = null;
            this._draft = null;
            this._queueRepaint();
            return Clutter.EVENT_STOP;
        }

        if (this._selection && this._draft) {
            if (this._dragMode === TOOL_TEXT) {
                const text = this._textEntry.text.trim();
                if (text.length > 0) {
                    this._annotations.push({
                        id: GLib.uuid_string_random(),
                        kind: TOOL_TEXT,
                        origin: this._toSelectionPoint(point),
                        text,
                        color: this._currentColor(),
                        size: this._currentTextSize(),
                        weight: this._currentTextWeight(),
                    });
                }
            } else if (this._dragMode === TOOL_BRUSH) {
                if (this._draft.points.length > 1)
                    this._annotations.push({...this._draft, id: GLib.uuid_string_random()});
            } else if (this._dragMode === TOOL_ARROW) {
                this._annotations.push({...this._draft, id: GLib.uuid_string_random()});
            } else if (this._dragMode === TOOL_RECTANGLE || this._dragMode === TOOL_MOSAIC) {
                if (this._draft.rect.width >= 2 && this._draft.rect.height >= 2)
                    this._annotations.push({...this._draft, id: GLib.uuid_string_random()});
            }
        }

        this._draft = null;
        this._dragOrigin = null;
        this._dragMode = null;
        this._positionToolbar();
        this._queueRepaint();
        return Clutter.EVENT_STOP;
    }

    _onKeyPress(_actor, event) {
        const symbol = event.get_key_symbol();
        switch (symbol) {
        case Clutter.KEY_Escape:
            this._extension.endCapture();
            return Clutter.EVENT_STOP;
        case Clutter.KEY_Return:
        case Clutter.KEY_KP_Enter:
            void this._save();
            return Clutter.EVENT_STOP;
        case Clutter.KEY_r:
        case Clutter.KEY_R:
            this._resetSelection();
            return Clutter.EVENT_STOP;
        case Clutter.KEY_1:
            this._setTool(TOOL_TEXT);
            return Clutter.EVENT_STOP;
        case Clutter.KEY_2:
            this._setTool(TOOL_ARROW);
            return Clutter.EVENT_STOP;
        case Clutter.KEY_3:
            this._setTool(TOOL_BRUSH);
            return Clutter.EVENT_STOP;
        case Clutter.KEY_4:
            this._setTool(TOOL_RECTANGLE);
            return Clutter.EVENT_STOP;
        case Clutter.KEY_5:
            this._setTool(TOOL_MOSAIC);
            return Clutter.EVENT_STOP;
        default:
            return Clutter.EVENT_PROPAGATE;
        }
    }

    _positionToolbar() {
        if (!this._selection || !this._toolbar.visible)
            return;

        const [, naturalWidth] = this._toolbar.get_preferred_width(-1);
        const [, naturalHeight] = this._toolbar.get_preferred_height(-1);
        const x = clamp(this._selection.x, 16, global.stage.width - naturalWidth - 16);
        const y = this._selection.y > naturalHeight + 24
            ? this._selection.y - naturalHeight - 12
            : clamp(this._selection.y + this._selection.height + 12, 16, global.stage.height - naturalHeight - 16);

        this._toolbar.set_position(x, y);
    }

    _queueRepaint() {
        if (this._canvas)
            this._canvas.queue_repaint();
        this._positionToolbar();
        this._refreshToolbarState();
    }

    _repaint(area) {
        const cr = area.get_context();
        const [width, height] = area.get_surface_size();

        cr.save();
        cr.setOperator(Cairo.Operator.CLEAR);
        cr.paint();
        cr.restore();

        cr.setOperator(Cairo.Operator.OVER);
        cr.setSourceRGBA(0, 0, 0, 0.48);
        cr.rectangle(0, 0, width, height);
        cr.fill();

        if (!this._selection)
            return;

        cr.save();
        cr.setOperator(Cairo.Operator.CLEAR);
        cr.rectangle(this._selection.x, this._selection.y, this._selection.width, this._selection.height);
        cr.fill();
        cr.restore();

        cr.save();
        cr.rectangle(this._selection.x, this._selection.y, this._selection.width, this._selection.height);
        cr.clip();
        this._drawAnnotations(cr);
        cr.restore();

        cr.save();
        cr.setSourceRGBA(1, 1, 1, 0.92);
        cr.setLineWidth(2);
        cr.rectangle(this._selection.x + 1, this._selection.y + 1, this._selection.width - 2, this._selection.height - 2);
        cr.stroke();
        cr.restore();
    }

    _drawAnnotations(cr) {
        for (const annotation of this._annotations)
            this._drawAnnotation(cr, annotation);
        if (this._draft)
            this._drawAnnotation(cr, this._draft);
    }

    _drawAnnotation(cr, annotation) {
        cr.save();
        cr.translate(this._selection.x, this._selection.y);

        switch (annotation.kind) {
        case TOOL_TEXT:
            drawText(cr, annotation);
            break;
        case TOOL_ARROW:
            drawArrow(cr, annotation.start, annotation.end, annotation.color, annotation.strokeWidth);
            break;
        case TOOL_BRUSH:
            drawBrush(cr, annotation.points, annotation.color, annotation.strokeWidth);
            break;
        case TOOL_RECTANGLE:
            drawRectangle(cr, annotation.rect, annotation.color, annotation.strokeWidth);
            break;
        case TOOL_MOSAIC:
            drawMosaicPreview(cr, annotation.rect, annotation.pixelSize);
            break;
        default:
            break;
        }

        cr.restore();
    }

    async _save() {
        if (!this._selection || this._saving)
            return;

        const width = Math.round(this._selection.width);
        const height = Math.round(this._selection.height);
        if (width < 4 || height < 4) {
            Main.notify('aShot', _('Select a larger area before saving.'));
            return;
        }

        this._saving = true;
        this._toolbar.hide();
        this._hint.hide();
        this._queueRepaint();
        await this._sleep(40);

        try {
            const sourcePath = await this._captureSelectionToTempFile();
            const outcome = await this._finalizeCapture(sourcePath);
            if (outcome?.message)
                Main.notify('aShot', outcome.message);
            else
                Main.notify('aShot', _('Annotated screenshot saved.'));
            this._extension.endCapture();
        } catch (error) {
            logError(error, 'Failed to save aShot capture');
            Main.notifyError('aShot', error.message ?? String(error));
            this._toolbar.show();
            this._hint.show();
            this._saving = false;
            this._queueRepaint();
        }
    }

    async _captureSelectionToTempFile() {
        const picturesDir = GLib.get_user_special_dir(GLib.UserDirectory.DIRECTORY_PICTURES) || GLib.get_home_dir();
        const tempDir = GLib.build_filenamev([picturesDir, 'Screenshots', '.ashot-tmp']);
        GLib.mkdir_with_parents(tempDir, 0o755);
        const tempFile = GLib.build_filenamev([tempDir, `${GLib.uuid_string_random()}.png`]);

        const result = await Gio.DBus.session.call(
            'org.gnome.Shell.Screenshot',
            '/org/gnome/Shell/Screenshot',
            'org.gnome.Shell.Screenshot',
            'ScreenshotArea',
            new GLib.Variant('(iiiibs)', [
                Math.round(this._selection.x),
                Math.round(this._selection.y),
                Math.round(this._selection.width),
                Math.round(this._selection.height),
                false,
                tempFile,
            ]),
            null,
            Gio.DBusCallFlags.NONE,
            -1,
            null
        );

        const [success, filenameUsed] = result.deepUnpack();
        if (!success)
            throw new Error(_('GNOME Shell refused to capture the selected area.'));
        return filenameUsed;
    }

    async _finalizeCapture(sourcePath) {
        await this._ensureAshotService();
        const sourceUri = GLib.filename_to_uri(sourcePath, null);
        const annotations = this._annotations
            .map(annotation => toAnnotationRecord(annotation))
            .filter(annotation => annotation !== null);

        const result = await Gio.DBus.session.call(
            APP_DBUS_NAME,
            APP_DBUS_PATH,
            APP_DBUS_INTERFACE,
            'FinalizeCapture',
            new GLib.Variant('(ss)', [sourceUri, JSON.stringify(annotations)]),
            null,
            Gio.DBusCallFlags.NONE,
            -1,
            null
        );

        const unpacked = result.deepUnpack();
        const outcome = Array.isArray(unpacked) && unpacked.length === 1 ? unpacked[0] : unpacked;
        if (Array.isArray(outcome)) {
            const [, message, fileUri] = outcome;
            return {message, fileUri};
        }

        return outcome;
    }

    async _ensureAshotService() {
        if (await this._nameHasOwner(APP_DBUS_NAME))
            return;

        const commands = [
            'flatpak run --command=ashot-app io.github.ashot.App --service',
            'ashot-app --service',
        ];

        for (const command of commands) {
            try {
                GLib.spawn_command_line_async(command);
            } catch (error) {
                logError(error, `Failed to start aShot service with \`${command}\``);
            }
        }

        for (let attempt = 0; attempt < 20; attempt++) {
            await this._sleep(150);
            if (await this._nameHasOwner(APP_DBUS_NAME))
                return;
        }

        throw new Error(_('aShot background service did not appear on DBus.'));
    }

    async _nameHasOwner(name) {
        const result = await Gio.DBus.session.call(
            'org.freedesktop.DBus',
            '/org/freedesktop/DBus',
            'org.freedesktop.DBus',
            'NameHasOwner',
            new GLib.Variant('(s)', [name]),
            new GLib.VariantType('(b)'),
            Gio.DBusCallFlags.NONE,
            -1,
            null
        );
        const [hasOwner] = result.deepUnpack();
        return hasOwner;
    }

    _sleep(milliseconds) {
        return new Promise(resolve => {
            GLib.timeout_add(GLib.PRIORITY_DEFAULT, milliseconds, () => {
                resolve();
                return GLib.SOURCE_REMOVE;
            });
        });
    }
}

class ShellBridge {
    constructor(extension) {
        this._extension = extension;
    }

    StartCapture() {
        this._extension.startCapture();
    }
}

class AshotIndicator extends PanelMenu.Button {
    static {
        GObject.registerClass(this);
    }

    constructor(extension) {
        super(0.0, _('aShot'));

        this._extension = extension;

        const icon = new St.Icon({
            icon_name: 'camera-photo-symbolic',
            style_class: 'system-status-icon',
        });
        this.add_child(icon);

        const startItem = new PopupMenu.PopupMenuItem(_('Area Capture'));
        startItem.connect('activate', () => this._extension.startCapture());
        this.menu.addMenuItem(startItem);

        const cancelItem = new PopupMenu.PopupMenuItem(_('Cancel Current Capture'));
        cancelItem.connect('activate', () => this._extension.endCapture());
        this.menu.addMenuItem(cancelItem);
    }
}

export default class AshotShellExtension extends Extension {
    enable() {
        this._indicator = new AshotIndicator(this);
        Main.panel.addToStatusArea('ashot-shell', this._indicator);

        this._bridge = new ShellBridge(this);
        this._dbusImpl = Gio.DBusExportedObject.wrapJSObject(SHELL_BRIDGE_IFACE, this._bridge);
        this._dbusImpl.export(Gio.DBus.session, SHELL_DBUS_PATH);
        this._nameOwner = Gio.DBus.session.own_name(
            SHELL_DBUS_NAME,
            Gio.BusNameOwnerFlags.NONE,
            null,
            null
        );

        this._session = null;
    }

    disable() {
        this.endCapture();

        if (this._indicator) {
            this._indicator.destroy();
            this._indicator = null;
        }

        if (this._dbusImpl) {
            this._dbusImpl.unexport();
            this._dbusImpl = null;
        }

        if (this._nameOwner) {
            Gio.DBus.session.unown_name(this._nameOwner);
            this._nameOwner = 0;
        }

        this._bridge = null;
    }

    startCapture() {
        if (this._session)
            return;

        this._session = new CaptureSession(this);
        this._session.open();
    }

    endCapture() {
        if (!this._session)
            return;

        this._session.destroy();
        this._session = null;
    }
}
