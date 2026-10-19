# WASI's host resource table is capped at 128 total resources. Exceeding it traps
# the interpreter, so an adversarial loop cannot exhaust the host's descriptors.
files = []
for _ in range(1000):
    files.append(open('/usr/local/lib/python3.14/os.py'))
raise AssertionError('descriptor limit did not apply')
