from datetime import datetime
from decimal import Decimal
from zoneinfo import ZoneInfo
import os

print('zone=' + str(datetime(2026, 1, 1, tzinfo=ZoneInfo('Asia/Singapore')).utcoffset()))
print('math=' + str(Decimal(17) * Decimal(19)))
assert not os.environ, os.environ
for path in ['/etc/passwd', '/Users', '/lib/../../etc/passwd', '/tz/../../etc/passwd']:
    try:
        open(path).read()
    except OSError:
        pass
    else:
        raise AssertionError('read escaped sandbox: ' + path)
for path in ['/result.txt', '/usr/local/lib/python3.14/result.txt', '/tz/result.txt']:
    try:
        open(path, 'w').write('blocked')
    except OSError:
        pass
    else:
        raise AssertionError('write escaped sandbox: ' + path)
# Other ways to change the host's files: every one must fail.
existing = '/usr/local/lib/python3.14/os.py'
for name, attempt in [
    ('mkdir', lambda: os.mkdir('/tz/new-dir')),
    ('unlink', lambda: os.unlink(existing)),
    ('rename', lambda: os.rename(existing, '/usr/local/lib/python3.14/moved.py')),
    ('open r+', lambda: open(existing, 'r+').write('x')),
    ('symlink', lambda: os.symlink(existing, '/tz/link')),
]:
    try:
        attempt()
    except (OSError, NotImplementedError):
        pass
    else:
        raise AssertionError(name + ' escaped sandbox')
# Report why the network is unreachable, so the test knows which guarantee it relied on.
# This CPython WASI build has a _socket module without real sockets or name lookup; WASI
# grants the guest no sockets either way. Connect by IP so DNS is not the only barrier.
try:
    import socket
except ImportError:
    print('network=no-socket-module')
else:
    try:
        connection = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    except (OSError, AttributeError):
        print('network=no-sockets')
    else:
        try:
            connection.settimeout(0.1)
            connection.connect(('1.1.1.1', 80))
        except OSError:
            print('network=refused')
        else:
            raise AssertionError('network escaped sandbox')
print('isolation=ok')
