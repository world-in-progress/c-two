import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createRequire } from 'node:module';
import { createServer } from 'node:net';
import { test } from 'node:test';
import { gzipSync } from 'node:zlib';

// Exercise the fetch implementation used by node-gyp's Node-header download.
const require = createRequire(import.meta.url);
const requireFromGyp = createRequire(require.resolve('node-gyp/package.json'));
const { Client, fetch } = requireFromGyp('undici');

for (const framing of ['content-length', 'chunked', 'valid-gzip']) {
  test(`node-gyp fetch handles ${framing} without an orphaned stream`, { timeout: 5000 }, async (t) => {
    const valid = framing === 'valid-gzip';
    let body = Buffer.from('node header download');
    for (let i = 0; i < 5; i++) body = gzipSync(body);
    const header = [
      'HTTP/1.1 200 OK',
      `Content-Encoding: ${Array(valid ? 5 : 6).fill('gzip').join(', ')}`,
      framing === 'chunked' ? 'Transfer-Encoding: chunked' : `Content-Length: ${valid ? body.length : 1}`,
      'Connection: close',
      '',
      '',
    ].join('\r\n');
    let socketClosed;
    const server = createServer((socket) => {
      socketClosed = once(socket, 'close');
      socket.once('data', () => socket.end(valid ? Buffer.concat([Buffer.from(header), body]) : header));
    });
    await once(server.listen(0, '127.0.0.1'), 'listening');
    const url = `http://127.0.0.1:${server.address().port}`;
    const client = new Client(url);
    t.after(async () => {
      await client.close();
      await new Promise((resolve) => server.close(resolve));
    });
    if (valid) {
      const response = await fetch(url, { dispatcher: client });
      assert.equal(await response.text(), 'node header download');
    } else {
      // GHSA-xjj6-f33q-929c: rejection must also dispose of the response stream.
      await assert.rejects(fetch(url, { dispatcher: client }), (error) => {
        assert.match(error.cause?.message, /too many content-encodings/);
        return true;
      });
    }
    assert.ok(socketClosed, 'the local peer accepted the request');
    await socketClosed;
  });
}
