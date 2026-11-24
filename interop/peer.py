"""A fresh loopback-only renderer and seekable HTTP audio source for each test."""
import asyncio
import io
import os
import unittest
import wave
from urllib.parse import urlsplit

from aiohttp import ClientSession, ClientTimeout, web
from aiohttp.test_utils import TestServer

from controlpoint import AV, ControlPoint, OriginRequester


class PeerTest(unittest.IsolatedAsyncioTestCase):
    multicast = False
    extra_args = ()

    async def asyncSetUp(self):
        binary = os.environ.get('DMR_TEST_BIN')
        if not binary:
            self.fail('use scripts/check.sh or set DMR_TEST_BIN to a built renderer')
        env = os.environ.copy()
        if lib := env.get('DMR_TEST_LIB'):
            env['DYLD_LIBRARY_PATH'] = lib
        # No option to target another renderer. Even accidental CI environment
        # overrides must never aim the mutation suite at someone's speakers.
        proc = await asyncio.create_subprocess_exec(
            binary, '-serve', '127.0.0.1:0', '-controller', '127.0.0.1', '-device', 'null/',
            *([] if self.multicast else ['-no-ssdp']), *self.extra_args,
            stdout=asyncio.subprocess.PIPE, env=env)
        self.addAsyncCleanup(self.stop_process, proc)
        async with asyncio.timeout(8):
            while line := await proc.stdout.readline():
                if line.startswith(b'TACET_RENDERER_URL='):
                    self.location = line.decode().strip().split('=', 1)[1]
                    break
            else:
                self.fail('renderer exited without a description URL')
        self.assertEqual(urlsplit(self.location).hostname, '127.0.0.1')
        session = ClientSession(timeout=ClientTimeout(total=4), trust_env=False)
        self.addAsyncCleanup(session.close)
        self.cp = await ControlPoint.connect(self.location, OriginRequester(self.location, session))

    @staticmethod
    async def stop_process(proc):
        if proc.returncode is None:
            proc.terminate()
            try:
                await asyncio.wait_for(proc.wait(), timeout=5)
            except TimeoutError:
                proc.kill()
                await proc.wait()
                raise

    async def serve_audio(self):
        audio = io.BytesIO()
        with wave.open(audio, 'wb') as out:
            out.setparams((1, 2, 8000, 0, 'NONE', ''))
            out.writeframes(bytes(16000 * 12))

        data = audio.getvalue()

        async def track(request):
            headers = {'Accept-Ranges': 'bytes'}
            if 'Range' not in request.headers:
                return web.Response(body=data, content_type='audio/wav', headers=headers)
            try:
                start, stop, _ = request.http_range.indices(len(data))
            except ValueError:
                start = stop = 0
            if start >= stop:
                return web.Response(status=416, headers={'Content-Range': f'bytes */{len(data)}'})
            headers['Content-Range'] = f'bytes {start}-{stop - 1}/{len(data)}'
            return web.Response(status=206, body=data[start:stop], content_type='audio/wav', headers=headers)

        app = web.Application()
        app.router.add_get('/track.wav', track)
        server = TestServer(app, host='127.0.0.1')
        await server.start_server()
        self.addAsyncCleanup(server.close)
        return str(server.make_url('/track.wav'))

    async def av(self, action, **args):
        return await self.cp.call(AV, action, InstanceID=0, **args)

    async def playing(self):
        async with asyncio.timeout(5):
            while (await self.av('GetTransportInfo'))['CurrentTransportState'] != 'PLAYING':
                await asyncio.sleep(0.03)
