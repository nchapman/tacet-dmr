import unittest

from async_upnp_client.const import HttpRequest

from controlpoint import PQ
from profile_device import ReadOnlyDevice, SOAP


class HardwareProbePolicyTests(unittest.TestCase):
    def setUp(self):
        self.origin = 'http://127.0.0.1:49152'
        self.guard = ReadOnlyDevice(self.origin + '/description.xml', None)
        self.guard.queue_url = self.origin + '/upnp/control/PlayQueue'

    def test_only_same_origin_xml_get(self):
        for path, expected in [('/description.xml', True), ('/upnp/PlayQueueSCPD.xml', True),
                               ('/httpapi.asp?command=getStatusEx', False), ('/description.xml?other', False)]:
            self.assertEqual(self.guard.permitted(HttpRequest('GET', self.origin + path, {}, None)), expected)
        self.assertFalse(self.guard.permitted(HttpRequest('GET', 'http://127.0.0.1:9/description.xml', {}, None)))
        self.assertFalse(self.guard.permitted(HttpRequest('SUBSCRIBE', self.origin + '/events', {}, None)))

    def test_post_requires_exact_getter_header_endpoint_and_arguments(self):
        def permitted(header, action, inner, url=None):
            body = f'<s:Envelope xmlns:s="{SOAP}"><s:Body><u:{action} xmlns:u="{PQ}">{inner}</u:{action}></s:Body></s:Envelope>'
            return self.guard.permitted(HttpRequest('POST', url or self.guard.queue_url,
                                                    {'SOAPAction': f'"{PQ}#{header}"'}, body))
        self.assertTrue(permitted('GetQueueIndex', 'GetQueueIndex', '<QueueName>CurrentQueue</QueueName>'))
        self.assertTrue(permitted('GetQueueLoopMode', 'GetQueueLoopMode', ''))
        self.assertFalse(permitted('GetQueueLoopMode', 'SetQueueLoopMode', '<LoopMode>4</LoopMode>'))
        self.assertFalse(permitted('SetQueueLoopMode', 'SetQueueLoopMode', '<LoopMode>4</LoopMode>'))
        self.assertFalse(permitted('GetQueueIndex', 'GetQueueIndex', '<QueueName>Other</QueueName>'))
        self.assertFalse(permitted('GetQueueLoopMode', 'GetQueueLoopMode', '', self.origin + '/other'))
