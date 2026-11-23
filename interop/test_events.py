import asyncio
from datetime import timedelta
import xml.etree.ElementTree as ET

from async_upnp_client.aiohttp import AiohttpNotifyServer

from controlpoint import AV, CM, RC
from peer import PeerTest


class EventTests(PeerTest):
    async def test_real_gena_callback_and_renewal(self):
        server = AiohttpNotifyServer(self.cp.requester, ('127.0.0.1', 0))
        await server.async_start_server()
        self.addAsyncCleanup(server.async_stop_server)
        handler = server.event_handler
        for urn in (CM, RC, AV):
            with self.subTest(service=urn):
                service = self.cp.device.service(urn)
                events = asyncio.Queue()
                service.on_event = lambda _, changed: events.put_nowait({v.name: v.value for v in changed})
                sid, duration = await handler.async_subscribe(service, timedelta(seconds=60))
                self.assertTrue(sid.startswith('uuid:'))
                self.assertEqual(duration, timedelta(seconds=60))
                values = await asyncio.wait_for(events.get(), 3)
                if urn == CM:
                    self.assertEqual(values['CurrentConnectionIDs'], '0')
                    self.assertIn('audio/wav', values['SinkProtocolInfo'])
                else:
                    ns = 'AVT' if urn == AV else 'RCS'
                    event = ET.fromstring(values['LastChange'])
                    self.assertEqual(event.tag, '{urn:schemas-upnp-org:metadata-1-0/' + ns + '/}Event')
                    self.assertEqual(event.find('{*}InstanceID').get('val'), '0')
                renewed, duration = await handler.async_resubscribe(sid, timedelta(seconds=90))
                self.assertEqual((sid, timedelta(seconds=90)), (renewed, duration))
                if urn == RC:
                    await self.cp.call(RC, 'SetVolume', InstanceID=0, Channel='Master', DesiredVolume=23)
                    notice = await asyncio.wait_for(events.get(), 3)
                    volume = ET.fromstring(notice['LastChange']).find('.//{*}Volume')
                    self.assertEqual((volume.get('channel'), volume.get('val')), ('Master', '23'))
                if urn == AV:
                    uri = 'http://127.0.0.1/unused?a=1&b=2'
                    await self.av('SetAVTransportURI', CurrentURI=uri, CurrentURIMetaData='')
                    notice = await asyncio.wait_for(events.get(), 3)
                    changed = ET.fromstring(notice['LastChange'])
                    self.assertEqual(changed.find('.//{*}TransportState').get('val'), 'STOPPED')
                    self.assertEqual(changed.find('.//{*}CurrentTrackURI').get('val'), uri)
                await handler.async_unsubscribe(sid)
                self.assertIsNone(handler.service_for_sid(sid))
