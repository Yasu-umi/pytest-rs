"""Minimal config.pluginmanager: just enough for common conftest probes
(getplugin("logging-plugin") and friends). register/unregister are no-ops —
plugin loading is the Rust engine's job."""


class PluginManager:
    def getplugin(self, name):
        if name in ("logging-plugin", "logging"):
            from pytest import _logging

            return _logging.state
        return None

    get_plugin = getplugin

    def hasplugin(self, name):
        return self.getplugin(name) is not None

    has_plugin = hasplugin

    def register(self, plugin, name=None):
        return None

    def unregister(self, plugin=None, name=None):
        return None

    def is_registered(self, plugin):
        return False


pluginmanager = PluginManager()
