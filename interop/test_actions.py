"""Every action we advertise must have a successful black-box scenario."""
import asyncio
import xml.etree.ElementTree as ET

from async_upnp_client.exceptions import UpnpActionResponseError

from controlpoint import AV, CM, PQ, RC, playlist
from peer import PeerTest

DIDL = ('<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
        'xmlns:dc="http://purl.org/dc/elements/1.1/"><item id="1" parentID="0" restricted="1">'
        '<dc:title>Water &amp; Light</dc:title></item></DIDL-Lite>')


class ActionTests(PeerTest):
    def covered(self, urn):
        self.assertEqual({name for service, name in self.cp.exercised if service == urn},
                         set(self.cp.device.service(urn).actions))

    async def test_connection_manager(self):
        protocols = await self.cp.call(CM, 'GetProtocolInfo')
        self.assertEqual(protocols['Source'], '')
        sinks = set(protocols['Sink'].split(','))
        for mime in ('audio/flac', 'audio/x-flac', 'audio/wav', 'audio/aiff', 'audio/mpeg',
                     'audio/aac', 'audio/mp4', 'audio/x-m4a', 'audio/alac', 'audio/ogg',
                     'audio/opus', 'audio/webm', 'audio/dsf', 'audio/wavpack', 'audio/x-ape',
                     'audio/x-ms-wma'):
            self.assertIn(f'http-get:*:{mime}:*', sinks)
        self.assertNotIn('http-get:*:audio/L16:*', sinks)
        ids = await self.cp.call(CM, 'GetCurrentConnectionIDs')
        self.assertEqual(ids['ConnectionIDs'], '0')
        connection = await self.cp.call(CM, 'GetCurrentConnectionInfo', ConnectionID=0)
        self.assertEqual((connection['AVTransportID'], connection['RcsID'], connection['Direction'],
                          connection['PeerConnectionID'], connection['Status']), (0, 0, 'Input', -1, 'OK'))
        self.covered(CM)

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
        self.covered(RC)

    async def test_av_transport(self):
        uri = await self.serve_audio()
        next_uri = uri + '?n=2&variant=B'
        self.assertEqual((await self.av('GetDeviceCapabilities'))['PlayMedia'], 'NETWORK')
        self.assertEqual((await self.av('GetTransportInfo'))['CurrentTransportState'], 'NO_MEDIA_PRESENT')
        await self.av('SetAVTransportURI', CurrentURI=uri, CurrentURIMetaData=DIDL)
        await self.av('SetNextAVTransportURI', NextURI=next_uri, NextURIMetaData=DIDL)
        media = await self.av('GetMediaInfo')
        self.assertEqual((media['CurrentURI'], media['NextURI'], media['NextURIMetaData']), (uri, next_uri, DIDL))
        self.assertIn('Next', (await self.av('GetCurrentTransportActions'))['Actions'].split(','))
        await self.av('SetPlayMode', NewPlayMode='REPEAT_ALL')
        self.assertEqual((await self.av('GetTransportSettings'))['PlayMode'], 'REPEAT_ALL')
        await self.av('Play', Speed='1')
        await self.playing()
        await self.av('Pause')
        await self.av('Seek', Unit='REL_TIME', Target='0:00:04.500')
        # Aim inside a second: UPnP reports whole seconds and the decoder
        # may land a fraction before the requested sample time.
        async with asyncio.timeout(4):
            while (await self.av('GetPositionInfo'))['RelTime'] != '0:00:04':
                await asyncio.sleep(0.03)
        await self.av('Next')
        position = await self.av('GetPositionInfo')
        self.assertEqual((position['TrackURI'], position['TrackMetaData']), (next_uri, DIDL))
        self.assertEqual((await self.av('GetTransportInfo'))['CurrentTransportState'], 'PAUSED_PLAYBACK')
        await self.av('Previous')
        self.assertEqual((await self.av('GetPositionInfo'))['TrackURI'], uri)
        await self.av('Stop')
        self.assertEqual((await self.av('GetTransportInfo'))['CurrentTransportState'], 'STOPPED')
        self.covered(AV)

    async def test_native_queue(self):
        uri = await self.serve_audio()
        async def pq(name, **args):
            return await self.cp.call(PQ, name, **args)
        current = {'QueueName': 'CurrentQueue'}
        await pq('CreateQueue', QueueContext=playlist('named', [(uri, DIDL)]))
        # Duplicate URI with different metadata: the current *entry* must survive a move.
        last = DIDL.replace('Water', 'Wind')
        await pq('AppendTracksInQueue', QueueContext=playlist('CurrentQueue', [(uri, last)]))
        await pq('PlayQueueWithIndex', QueueName='named', Index=2)
        await self.playing()
        await pq('SetQueueLoopMode', LoopMode=2)
        self.assertEqual((await self.av('GetTransportSettings'))['PlayMode'], 'SHUFFLE')
        self.assertEqual((await pq('GetQueueLoopMode'))['LoopMode'], 2)
        await pq('SetQueueLoopMode', LoopMode=3)
        self.assertEqual((await self.av('GetTransportSettings'))['PlayMode'], 'SHUFFLE_NOREPEAT')
        await pq('SetQueueLoopMode', LoopMode=5)
        self.assertEqual((await self.av('GetTransportSettings'))['PlayMode'], 'SHUFFLE_REPEAT_ONE')
        await pq('SetQueueLoopMode', LoopMode=1)
        self.assertEqual((await self.av('GetTransportSettings'))['PlayMode'], 'REPEAT_ONE')
        await pq('MoveTracksInQueue', **current, IndexList='2', ToIndex=1)
        self.assertEqual((await self.av('GetPositionInfo'))['TrackMetaData'], last)
        result = await pq('GetQueueIndex', **current)
        self.assertEqual((result['CurrentIndex'], result['PreloadingIndex'], result['TrackNums']), (1, 1, 2))
        self.assertEqual((await pq('GetQueueLoopMode'))['LoopMode'], 1)
        for name in ('CurrentQueue', 'named'):
            view = ET.fromstring((await pq('BrowseQueue', QueueName=name))['QueueContext'])
            self.assertEqual(view.findtext('ListName'), 'named')
            self.assertEqual(view.findtext('ListInfo/TrackNumber'), '2')
            self.assertEqual([(track.tag, track.findtext('URL'), track.findtext('Metadata'))
                              for track in view.find('Tracks')], [('Track1', uri, last), ('Track2', uri, DIDL)])
        await pq('RemoveTracksInQueue', **current, RangStart=1, RangEnd=2, Action='0')
        self.assertEqual((await pq('GetQueueIndex', **current))['CurrentIndex'], 0)
        self.assertEqual((await self.av('GetTransportInfo'))['CurrentTransportState'], 'NO_MEDIA_PRESENT')
        empty = ET.fromstring((await pq('BrowseQueue', **current))['QueueContext'])
        self.assertEqual(empty.findtext('ListInfo/TrackNumber'), '0')
        self.covered(PQ)

    async def test_lookahead_follows_edited_queue_at_eof(self):
        uri = await self.serve_audio()
        tracks = [(uri, 'first'), (uri + '?stale', 'second'), (uri, 'third')]
        q = {'QueueName': 'CurrentQueue'}
        await self.cp.call(PQ, 'CreateQueue', QueueContext=playlist('edited', tracks))
        await self.cp.call(PQ, 'PlayQueueWithIndex', **q, Index=1)
        await self.playing()
        await self.av('Pause')
        await self.av('Seek', Unit='REL_TIME', Target='0:00:11')
        await self.cp.call(PQ, 'MoveTracksInQueue', **q, IndexList='3', ToIndex=2)
        await self.cp.call(PQ, 'RemoveTracksInQueue', **q, RangStart=3, RangEnd=3, Action='0')
        self.assertEqual((await self.av('GetPositionInfo'))['TrackMetaData'], 'first')
        self.assertEqual((await self.cp.call(PQ, 'GetQueueIndex', **q))['PreloadingIndex'], 2)
        await self.av('Play', Speed='1')
        async with asyncio.timeout(5):
            while (await self.av('GetPositionInfo'))['TrackMetaData'] == 'first':
                await asyncio.sleep(0.03)
        position = await self.av('GetPositionInfo')
        self.assertEqual((position['TrackURI'], position['TrackMetaData']), (uri, 'third'))
        index = await self.cp.call(PQ, 'GetQueueIndex', **q)
        self.assertEqual((index['CurrentIndex'], index['PreloadingIndex']), (2, 0))

    async def test_soap_fault_codes_and_atomic_rejection(self):
        valid = ('http://127.0.0.1/never-fetched', DIDL)
        await self.cp.call(PQ, 'CreateQueue', QueueContext=playlist('fixture', [valid]))
        for code, urn, name, args in [
            (718, AV, 'GetTransportInfo', dict(InstanceID=2)),
            (706, CM, 'GetCurrentConnectionInfo', dict(ConnectionID=27)),
            (402, RC, 'GetVolume', dict(InstanceID=0, Channel='LF')),
            (402, PQ, 'AppendTracksInQueue', dict(QueueContext=playlist('CurrentQueue', [valid, ('file:///invalid', '')]))),
            (402, PQ, 'PlayQueueWithIndex', dict(QueueName='CurrentQueue', Index=0)),
            (402, PQ, 'BrowseQueue', dict(QueueName='unknown')),
            (402, PQ, 'CreateQueue', dict(QueueContext=playlist('TotalQueue', [valid]))),
        ]:
            with self.subTest(action=name), self.assertRaises(UpnpActionResponseError) as caught:
                await self.cp.call(urn, name, **args)
            self.assertEqual((caught.exception.status, caught.exception.error_code), (500, code))
        self.assertEqual((await self.cp.call(PQ, 'GetQueueIndex', QueueName='CurrentQueue'))['TrackNums'], 1)
