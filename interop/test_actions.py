"""Black-box SOAP action coverage for both OpenHome Media and minimal UPnP AV."""
import asyncio

from async_upnp_client.exceptions import UpnpActionResponseError

from controlpoint import (
    AV,
    CM,
    OH_INFO,
    OH_PLAYLIST,
    OH_PRODUCT,
    OH_RECEIVER,
    OH_SENDER,
    OH_TIME,
    OH_VOLUME,
    RC,
)
from peer import PeerTest

DIDL = ('<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
        'xmlns:dc="http://purl.org/dc/elements/1.1/"><item id="1" parentID="0" restricted="1">'
        '<dc:title>Water &amp; Light</dc:title></item></DIDL-Lite>')


class ActionTests(PeerTest):
    def covered(self, cp, urn):
        self.assertEqual({name for service, name in cp.exercised if service == urn},
                         set(cp.device.service(urn).actions))

    async def test_openhome_services(self):
        self.assertEqual(
            set(self.oh.inventory()),
            {OH_PRODUCT, OH_PLAYLIST, OH_INFO, OH_TIME, OH_VOLUME, OH_RECEIVER, OH_SENDER},
        )
        # Product
        product = await self.oh.call(OH_PRODUCT, 'Product')
        self.assertTrue(product['Name'])
        source_count = (await self.oh.call(OH_PRODUCT, 'SourceCount'))['Value']
        self.assertEqual(source_count, 3)

        # Volume
        await self.oh.call(OH_VOLUME, 'SetVolume', Value=41)
        self.assertEqual((await self.oh.call(OH_VOLUME, 'Volume'))['Value'], 41)
        await self.oh.call(OH_VOLUME, 'VolumeInc')
        self.assertEqual((await self.oh.call(OH_VOLUME, 'Volume'))['Value'], 42)
        await self.oh.call(OH_VOLUME, 'SetMute', Value=True)
        self.assertIs((await self.oh.call(OH_VOLUME, 'Mute'))['Value'], True)
        await self.oh.call(OH_VOLUME, 'SetMute', Value=False)

        # Playlist + Info + Time
        uri = await self.serve_audio()
        inserted = await self.oh.call(OH_PLAYLIST, 'Insert', AfterId=0, Uri=uri, Metadata=DIDL)
        track_id = inserted['NewId']
        read = await self.oh.call(OH_PLAYLIST, 'Read', Id=track_id)
        self.assertEqual((read['Uri'], read['Metadata']), (uri, DIDL))
        await self.oh.call(OH_PLAYLIST, 'Play')
        async with asyncio.timeout(5):
            while (await self.oh.call(OH_PLAYLIST, 'TransportState'))['Value'] != 'Playing':
                await asyncio.sleep(0.03)
        details = await self.oh.call(OH_INFO, 'Details')
        self.assertEqual(details['SampleRate'], 8000)
        self.assertEqual(details['CodecName'], 'PCM')
        self.assertIs(details['Lossless'], True)
        time_info = await self.oh.call(OH_TIME, 'Time')
        self.assertGreaterEqual(time_info['Duration'], 11)
        await self.oh.call(OH_PLAYLIST, 'Pause')
        self.assertEqual((await self.oh.call(OH_PLAYLIST, 'TransportState'))['Value'], 'Paused')
        await self.oh.call(OH_PLAYLIST, 'Stop')
        self.assertEqual((await self.oh.call(OH_PLAYLIST, 'TransportState'))['Value'], 'Stopped')

        # Sender & Receiver baseline queries
        sender_status = await self.oh.call(OH_SENDER, 'Status')
        self.assertEqual(sender_status['Value'], 'Enabled')
        rx_state = await self.oh.call(OH_RECEIVER, 'TransportState')
        self.assertEqual(rx_state['Value'], 'Stopped')

    async def test_connection_manager(self):
        protocols = await self.cp.call(CM, 'GetProtocolInfo')
        self.assertEqual(protocols['Source'], '')
        sinks = set(protocols['Sink'].split(','))
        for mime in ('audio/flac', 'audio/x-flac', 'audio/wav', 'audio/aiff', 'audio/mpeg',
                     'audio/aac', 'audio/mp4', 'audio/x-m4a', 'audio/alac', 'audio/ogg',
                     'audio/opus', 'audio/webm'):
            self.assertIn(f'http-get:*:{mime}:*', sinks)
        for unsupported in ('audio/L16', 'audio/dsf', 'audio/wavpack', 'audio/x-ape', 'audio/x-ms-wma'):
            self.assertNotIn(f'http-get:*:{unsupported}:*', sinks)
        ids = await self.cp.call(CM, 'GetCurrentConnectionIDs')
        self.assertEqual(ids['ConnectionIDs'], '0')
        connection = await self.cp.call(CM, 'GetCurrentConnectionInfo', ConnectionID=0)
        self.assertEqual((connection['AVTransportID'], connection['RcsID'], connection['Direction'],
                          connection['PeerConnectionID'], connection['Status']), (0, 0, 'Input', -1, 'OK'))
        self.covered(self.cp, CM)

    async def test_rendering_control(self):
        async def rc(name, **args):
            return await self.cp.call(RC, name, InstanceID=0, **args)
        master = {'Channel': 'Master'}
        self.assertEqual((await rc('ListPresets'))['CurrentPresetNameList'], 'FactoryDefaults')
        await rc('SetVolume', **master, DesiredVolume=27)
        await rc('SetMute', **master, DesiredMute=True)
        self.assertEqual((await rc('GetVolume', **master))['CurrentVolume'], 27)
        self.assertIs((await rc('GetMute', **master))['CurrentMute'], True)
        await rc('SelectPreset', PresetName='FactoryDefaults')
        self.assertEqual((await rc('GetVolume', **master))['CurrentVolume'], 100)
        self.assertIs((await rc('GetMute', **master))['CurrentMute'], False)
        self.covered(self.cp, RC)

    async def test_av_transport(self):
        uri = await self.serve_audio()
        next_uri = uri + '?n=2&variant=B'
        self.assertEqual((await self.av('GetDeviceCapabilities'))['PlayMedia'], 'NETWORK')
        self.assertEqual((await self.av('GetTransportInfo'))['CurrentTransportState'], 'NO_MEDIA_PRESENT')
        await self.av('SetAVTransportURI', CurrentURI=uri, CurrentURIMetaData=DIDL)
        await self.av('SetNextAVTransportURI', NextURI=next_uri, NextURIMetaData=DIDL)
        media = await self.av('GetMediaInfo')
        self.assertEqual((media['CurrentURI'], media['NextURI'], media['NextURIMetaData']), (uri, next_uri, DIDL))
        await self.av('SetPlayMode', NewPlayMode='NORMAL')
        self.assertEqual((await self.av('GetTransportSettings'))['PlayMode'], 'NORMAL')
        await self.av('Play', Speed='1')
        await self.playing()
        await self.av('Pause')
        await self.av('Seek', Unit='REL_TIME', Target='0:00:04.500')
        async with asyncio.timeout(4):
            while (await self.av('GetPositionInfo'))['RelTime'] != '0:00:04':
                await asyncio.sleep(0.03)
        position = await self.av('GetPositionInfo')
        self.assertEqual((position['TrackURI'], position['TrackMetaData']), (uri, DIDL))
        self.assertEqual((await self.av('GetTransportInfo'))['CurrentTransportState'], 'PAUSED_PLAYBACK')
        await self.av('Stop')
        self.assertEqual((await self.av('GetTransportInfo'))['CurrentTransportState'], 'STOPPED')
        self.assertIn('Play', (await self.av('GetCurrentTransportActions'))['Actions'].split(','))
        self.covered(self.cp, AV)

    async def test_soap_fault_codes(self):
        for code, urn, name, args in [
            (718, AV, 'GetTransportInfo', dict(InstanceID=2)),
            (706, CM, 'GetCurrentConnectionInfo', dict(ConnectionID=27)),
            (402, RC, 'GetVolume', dict(InstanceID=0, Channel='LF')),
            (712, AV, 'SetPlayMode', dict(InstanceID=0, NewPlayMode='SHUFFLE')),
            (402, AV, 'SetAVTransportURI', dict(InstanceID=0, CurrentURI='file:///etc/passwd', CurrentURIMetaData='')),
        ]:
            with self.subTest(action=name), self.assertRaises(UpnpActionResponseError) as caught:
                await self.cp.call(urn, name, **args)
            self.assertEqual((caught.exception.status, caught.exception.error_code), (500, code))
