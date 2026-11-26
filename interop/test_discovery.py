import os
import unittest

from async_upnp_client.search import async_search

from controlpoint import AV, ControlPoint, OH_PRODUCT
from peer import PeerTest


@unittest.skipUnless(
    os.getenv('TACET_ENDPOINT_TEST_SSDP') == '1' or os.getenv('TACET_DMR_TEST_SSDP') == '1',
    'requires multicast-capable loopback',
)
class DiscoveryTests(PeerTest):
    multicast = True

    async def test_search_to_description_to_control(self):
        for target, location, cp in (
            (OH_PRODUCT, self.openhome_location, self.oh),
            (AV, self.upnp_location, self.cp),
        ):
            found = []

            async def response(headers):
                if headers.get('location') == location:
                    found.append(headers)

            await async_search(response, timeout=1, search_target=target, source=('127.0.0.1', 0))
            self.assertTrue(found, f'no {target} answer for spawned endpoint')
            match = found[0]
            self.assertEqual(match['st'], target)
            self.assertEqual(match['usn'], cp.device.udn + '::' + target)
            peer = await ControlPoint.connect(match['location'], cp.requester)
            if target == AV:
                status = await peer.call(AV, 'GetTransportInfo', InstanceID=0)
                self.assertEqual(status['CurrentTransportState'], 'NO_MEDIA_PRESENT')
            else:
                info = await peer.call(OH_PRODUCT, 'Product')
                self.assertTrue(info['Name'])
