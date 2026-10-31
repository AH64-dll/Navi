"""KDE window handles and focus actions through the installed KWin API."""
import json
import os
from pathlib import Path
import re

from gi.repository import Gio, GLib
from ipc import RUNTIME


class Windows:
    def __init__(self, bus):
        self.bus = bus
        self.counter = 0
        self.reply = None
        self.loop = None
        xml = '<node><interface name="dev.navi.Windows"><method name="Reply"><arg type="s" direction="in"/></method></interface></node>'
        info = Gio.DBusNodeInfo.new_for_xml(xml)
        self.registration = bus.register_object('/dev/navi/Windows', info.interfaces[0], self.response, None, None)

    def response(self, connection, sender, path, interface, method, parameters, invocation):
        owner = self.bus.call_sync('org.freedesktop.DBus','/org/freedesktop/DBus','org.freedesktop.DBus','GetNameOwner',GLib.Variant('(s)',('org.kde.KWin',)),GLib.VariantType.new('(s)'),Gio.DBusCallFlags.NONE,1000,None).unpack()[0]
        if sender != owner:
            invocation.return_dbus_error('dev.navi.Untrusted','Only KWin can publish window handles')
            return
        data = parameters.unpack()[0]
        if len(data) <= 64000:
            self.reply = json.loads(data)
        invocation.return_value(GLib.Variant('()',()))
        if self.loop:self.loop.quit()

    def run(self, window=None):
        if window is not None and not re.fullmatch(r'\{?[0-9a-fA-F-]{36}\}?',window):
            raise ValueError('Invalid native window handle')
        self.counter += 1
        name = f'navi-window-{os.getpid()}-{self.counter}'
        path = RUNTIME / f'{name}.js'
        # No model-authored code: identifiers and receiver are JSON literals.
        source = 'var all = workspace.stackingOrder.filter(w => !w.specialWindow && !w.deleted);\n'
        if window is not None:
            source += 'var target = all.find(w => String(w.internalId) === '+json.dumps(window)+');\nif(target){target.minimized=false;workspace.activeWindow=target;}\n'
        source += 'var result = all.slice(0,100).map(w => ({id:String(w.internalId),name:w.caption.slice(0,200),app:String(w.resourceClass),active:w.active}));\n'
        source += 'callDBus('+json.dumps(self.bus.get_unique_name())+',"/dev/navi/Windows","dev.navi.Windows","Reply",JSON.stringify(result));\n'
        path.write_text(source)
        path.chmod(0o600)
        self.reply = None
        try:
            number = self.bus.call_sync('org.kde.KWin','/Scripting','org.kde.kwin.Scripting','loadScript',GLib.Variant('(ss)',(str(path),name)),GLib.VariantType.new('(i)'),Gio.DBusCallFlags.NONE,2000,None).unpack()[0]
            if number < 0:raise RuntimeError('KWin rejected the window adapter')
            self.loop = GLib.MainLoop()
            self.bus.call_sync('org.kde.KWin',f'/Scripting/Script{number}','org.kde.kwin.Script','run',None,None,Gio.DBusCallFlags.NONE,2000,None)
            timer = GLib.timeout_add(2000,lambda:self.loop.quit() or False)
            self.loop.run()
            if self.reply is not None:GLib.source_remove(timer)
            if self.reply is None:raise RuntimeError('KWin window observation timed out')
            return self.reply
        finally:
            self.loop = None
            try:self.bus.call_sync('org.kde.KWin','/Scripting','org.kde.kwin.Scripting','unloadScript',GLib.Variant('(s)',(name,)),None,Gio.DBusCallFlags.NONE,2000,None)
            finally:path.unlink(missing_ok=True)


if __name__ == '__main__':
    import sys
    result=Windows(Gio.bus_get_sync(Gio.BusType.SESSION,None)).run(sys.argv[1] if len(sys.argv)>1 else None)
    print(json.dumps(result))
