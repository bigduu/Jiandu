#!/usr/bin/env python3
"""Exercise real stdio MCP in a fresh temporary store; never touch personal data."""
import json, os, pathlib, subprocess, tempfile

binary = os.environ.get('JIANDU_BIN', 'jiandu')
root = tempfile.mkdtemp(prefix='jiandu-readme-demo-')
proc = subprocess.Popen([binary, '--data-dir', root], stdin=subprocess.PIPE,
                        stdout=subprocess.PIPE, text=True)
serial = 0

def request(method, params):
    global serial
    serial += 1
    proc.stdin.write(json.dumps(dict(jsonrpc='2.0', id=serial, method=method, params=params))+'\n')
    proc.stdin.flush()
    while True:
        result = json.loads(proc.stdout.readline())
        if result.get('id') == serial:
            if 'error' in result:
                raise RuntimeError(result['error'])
            return result['result']

context = {'io.github.bigduu.jiandu/context': {'project_id': 'readme-demo'}}
def memory(**args):
    response = request('tools/call', dict(name='memory', arguments=args, _meta=context))
    if response.get('isError'):
        raise RuntimeError(response)
    value = response.get('structuredContent')
    if value is None:
        value = json.loads(response['content'][0]['text'])
    print(json.dumps(dict(request=args, result=value), ensure_ascii=False))
    return value

try:
    request('initialize', dict(protocolVersion='2025-03-26', capabilities={},
                              clientInfo=dict(name='readme-demo', version='1.0')))
    proc.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
    proc.stdin.flush()
    # A fresh root has no lexical index. The server explicitly requests rebuild.
    try:
        memory(action='query', scope='project', query='demo release checklist')
    except RuntimeError as error:
        if 'lexical index is missing' not in str(error):
            raise
        print(json.dumps({'expected_error': str(error)}, ensure_ascii=False))
        memory(action='rebuild', scope='project')
        memory(action='query', scope='project', query='demo release checklist')
    saved = memory(action='write', scope='project', type='reference',
                   title='Demo: release checklist',
                   content='Synthetic demo project. Before each release, run the tests, review the changelog, and record the rollback command.',
                   tags=['demo', 'release'], keywords=['checklist', 'release'])
    hits = memory(action='query', scope='project', query='release checklist')
    assert saved['memory']['id'] in {hit['id'] for hit in hits['data']['items']}, 'Recall did not return the saved demo memory'
    item = memory(action='get', id=saved['memory']['id'])
    assert 'rollback command' in json.dumps(item)
    print(json.dumps({'demo_data_dir': root}))
finally:
    proc.terminate()
    proc.wait(timeout=5)
