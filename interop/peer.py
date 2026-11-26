"""A fresh loopback-only tacet-endpoint and seekable HTTP audio source for each test."""
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
    device = 'null/'
    extra_args = ()

    async def asyncSetUp(self):
        binary = os.environ.get('TACET_ENDPOINT_BIN') or os.environ.get('DMR_TEST_BIN')
        if not binary:
            self.fail('use scripts/check.sh or set TACET_ENDPOINT_BIN to a built endpoint')
        env = os.environ.copy()
        proc = await asyncio.create_subprocess_exec(
            binary, '-serve', '127.0.0.1:0', '-controller', '127.0.0.1', '-device', self.device,
            *([] if self.multicast else ['-no-ssdp']), *self.extra_args,
            stdout=asyncio.subprocess.PIPE, env=env)
        self.addAsyncCleanup(self.stop_process, proc)
        self.openhome_location = None
        self.upnp_location = None
        self.songcast_uri = None
        expect_upnp = '-no-upnp' not in self.extra_args and '--no-upnp' not in self.extra_args
        async with asyncio.timeout(8):
            while line := await proc.stdout.readline():
                text = line.decode().strip()
                if text.startswith('TACET_OPENHOME_URL='):
                    self.openhome_location = text.split('=', 1)[1]
                elif text.startswith('TACET_UPNP_URL='):
                    self.upnp_location = text.split('=', 1)[1]
                elif text.startswith('TACET_SONGCAST_URL='):
                    self.songcast_uri = text.split('=', 1)[1]
                if self.openhome_location and (not expect_upnp or self.upnp_location):
                    break
            else:
                self.fail('endpoint exited without printing startup URLs')
        self.assertEqual(urlsplit(self.openhome_location).hostname, '127.0.0.1')
        session = ClientSession(timeout=ClientTimeout(total=4), trust_env=False)
        self.addAsyncCleanup(session.close)
        self.oh = await ControlPoint.connect(
            self.openhome_location, OriginRequester(self.openhome_location, session))
        if self.upnp_location:
            self.assertEqual(urlsplit(self.upnp_location).hostname, '127.0.0.1')
            self.cp = await ControlPoint.connect(
                self.upnp_location, OriginRequester(self.upnp_location, session))
        else:
            self.cp = None

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
