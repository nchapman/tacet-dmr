"""Regression oracle independent of both the renderer and Tacet."""
import json
from pathlib import Path

from controlpoint import AV, CM, PQ, RC, playlist
from peer import PeerTest


class ContractTests(PeerTest):
    async def test_advertised_wire_signatures(self):
        reference = json.loads((Path(__file__).parent / 'testdata/wiim-ultra.json').read_text())
        # The speakers add two output arguments to the standardized AV:1
        # GetMediaInfo. Do not require vendor extensions to a standard action.
        reference[AV]['GetMediaInfo'] = reference[AV]['GetMediaInfo'].replace(' TrackSource:string', '').replace(' PreSwitchMode:string', '')
        inventory = self.cp.inventory()
        self.assertEqual(set(inventory), {AV, RC, CM, PQ})
        for urn, actions in inventory.items():
            for name, signature in actions.items():
                with self.subTest(service=urn, action=name):
                    self.assertEqual(signature, reference[urn][name])

    async def test_four_field_queue_index_decodes_as_integers(self):
        await self.cp.call(PQ, 'CreateQueue', QueueContext=playlist('fixture', [('http://127.0.0.1/unused', '')]))
        index = await self.cp.call(PQ, 'GetQueueIndex', QueueName='CurrentQueue')
        self.assertEqual(index, {'CurrentIndex': 1, 'PreloadingIndex': 0, 'CurrentPage': 1, 'TrackNums': 1})
