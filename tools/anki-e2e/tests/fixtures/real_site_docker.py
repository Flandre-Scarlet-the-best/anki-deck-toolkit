#!/usr/bin/python3
"""Модель Docker CLI: состояние и журнал операций изолированы каждым тестом."""
import json
import os
import sys
from pathlib import Path

args = sys.argv[1:]
path = Path(os.environ['DOCKER_FIXTURE_STATE'])
state = json.loads(path.read_text())
with Path(str(path) + '.log').open('a') as log:
    log.write(json.dumps(args) + '\n')
label = 'io.github.anki-deck-toolkit.e2e.real-site'
container = 'anki-e2e-real-site'


def save():
    path.write_text(json.dumps(state))


def fail():
    sys.stderr.write('fixture: отказ Docker\n')
    sys.exit(1)


if args[0] == 'version':
    print('{}')
elif args[0] == 'info':
    print(json.dumps({'OSType': 'linux', 'Architecture': 'x86_64'}))
elif args[:2] == ['container', 'ls']:
    print(container if state['container'] else '')
elif args[:2] == ['volume', 'ls']:
    print('\n'.join(state['volumes']))
elif args[:2] in (['container', 'inspect'], ['volume', 'inspect']):
    if args[0] == 'container':
        print(json.dumps([state['container']]))
    else:
        print(json.dumps([{'Labels': state['volumes'][args[-1]]}]))
elif args[0] == 'inspect':
    print(state['container']['State']['Status'])
elif args[0] == 'exec':
    if args[2:] == ['pronunciation-assets', 'forvo', 'brave', 'setup']:
        print('{"outcome":"ok"}')
    elif args[-1] == 'sandbox-check':
        print('{"renderers":1,"isolated":1}')
    elif args[-1] == 'version':
        print('Brave fixture')
    else:
        print('1')
elif args[0] == 'rm':
    if state.get('remove_failure'):
        fail()
    state['container'] = None
    save()
elif args[:2] == ['volume', 'rm']:
    del state['volumes'][args[-1]]
    save()
elif args[:2] == ['volume', 'create']:
    state['volumes'][args[-1]] = {label: '1'}
    save()
elif args[:2] == ['image', 'inspect']:
    print('sha256:fixture')
elif args[0] == 'images':
    print('')
elif args[0] == 'run':
    ports = {}
    for i, arg in enumerate(args):
        if arg == '-p':
            host, port, target = args[i + 1].split(':')
            ports[target + '/tcp'] = [{'HostIp': host, 'HostPort': port}]
    state['container'] = {'Config': {'Labels': {label: '1'}}, 'State': {'Status': 'running'},
                          'NetworkSettings': {'Ports': ports}}
    save()
elif args[0] == 'logs':
    print('real-site session ready')
else:
    fail()
