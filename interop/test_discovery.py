import os
import unittest

from async_upnp_client.search import async_search

from controlpoint import AV, ControlPoint, PQ
from peer import PeerTest


@unittest.skipUnless(os.getenv('TACET_DMR_TEST_SSDP') == '1', 'requires multicast-capable loopback')
class DiscoveryTests(PeerTest):
    multicast = True

    async def test_search_to_description_to_control(self):
        for target in (AV, PQ):
            found = []

            async def response(headers):
                if headers.get('location') == self.location:
                    found.append(headers)

            await async_search(response, timeout=1, search_target=target, source=('127.0.0.1', 0))
            self.assertTrue(found, f'no {target} answer for spawned renderer')
            match = found[0]
            self.assertEqual(match['st'], target)
            self.assertEqual(match['usn'], self.cp.device.udn + '::' + target)
            peer = await ControlPoint.connect(match['location'], self.cp.requester)
            status = await peer.call(AV, 'GetTransportInfo', InstanceID=0)
            self.assertEqual(status['CurrentTransportState'], 'NO_MEDIA_PRESENT')
