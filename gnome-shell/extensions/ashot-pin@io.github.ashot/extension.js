import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const PIN_WINDOW_TAG = 'ashot-pin';

export default class AshotPinExtension extends Extension {
    enable() {
        this._displaySignals = [
            global.display.connect('window-created', (_display, window) => {
                this._trackWindow(window);
            }),
        ];
        this._windowSignals = new Map();

        for (const actor of global.get_window_actors()) {
            const window = actor.meta_window ?? actor.get_meta_window?.();
            this._trackWindow(window);
        }
    }

    disable() {
        for (const signalId of this._displaySignals ?? [])
            global.display.disconnect(signalId);
        this._displaySignals = [];

        for (const [window, signalIds] of this._windowSignals ?? new Map()) {
            for (const signalId of signalIds)
                window.disconnect(signalId);
        }
        this._windowSignals = new Map();
    }

    _trackWindow(window) {
        if (!this._isPinWindow(window))
            return;
        if (this._windowSignals.has(window)) {
            this._enforcePinWindow(window);
            return;
        }

        const signalIds = [
            window.connect('shown', () => this._enforcePinWindow(window)),
            window.connect('raised', () => this._enforcePinWindow(window)),
            window.connect('workspace-changed', () => this._enforcePinWindow(window)),
            window.connect('notify::above', () => this._enforcePinWindow(window)),
            window.connect('notify::on-all-workspaces', () => this._enforcePinWindow(window)),
            window.connect('unmanaged', () => this._untrackWindow(window)),
        ];
        this._windowSignals.set(window, signalIds);
        this._enforcePinWindow(window);
    }

    _untrackWindow(window) {
        const signalIds = this._windowSignals.get(window);
        if (!signalIds)
            return;
        for (const signalId of signalIds)
            window.disconnect(signalId);
        this._windowSignals.delete(window);
    }

    _isPinWindow(window) {
        return Boolean(window) && window.get_tag() === PIN_WINDOW_TAG;
    }

    _enforcePinWindow(window) {
        if (!this._isPinWindow(window))
            return;
        if (!window.is_above())
            window.make_above();
        if (!window.is_on_all_workspaces())
            window.stick();
    }
}
