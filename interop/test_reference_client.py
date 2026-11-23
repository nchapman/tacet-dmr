"""Can the test client encode/decode *all* captured firmware signatures?

Offline codec test, NOT a behavioral emulator of unimplemented firmware APIs.
There is no network transport here, even for getters. Project only captured
facts into synthetic device/SCPD documents; never store real device identity.
"""
import json
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

from async_upnp_client.client import UpnpRequester
from async_upnp_client.const import HttpResponse

from controlpoint import ControlPoint

BASE = 'http://fixture.invalid/'
SOAP = 'http://schemas.xmlsoap.org/soap/envelope/'
SAMPLE = {'string': 'Water < Air & 光', 'ui2': 17, 'ui4': 7, 'i4': -1, 'boolean': True}


def elements(parent, **values):
    for name, value in values.items():
        ET.SubElement(parent, name).text = value


class DescriptionReplay(UpnpRequester):
    def __init__(self, reference):
        root = ET.Element('root', xmlns='urn:schemas-upnp-org:device-1-0')
        version = ET.SubElement(root, 'specVersion')
        elements(version, major='1', minor='0')
        device = ET.SubElement(root, 'device')
        elements(device, deviceType='urn:schemas-upnp-org:device:MediaRenderer:1', friendlyName='Fixture',
                 manufacturer='Fixture', modelName='Reference projection', UDN='uuid:fixture')
        services = ET.SubElement(device, 'serviceList')
        self.docs = {BASE + 'description.xml': root}
        for index, (urn, actions) in enumerate(reference.items()):
            path = f'{index}/scpd.xml'
            elements(ET.SubElement(services, 'service'), serviceType=urn, serviceId=f'urn:fixture:serviceId:{index}',
                     SCPDURL=path, controlURL=f'{index}/control', eventSubURL=f'{index}/events')
            doc = ET.Element('scpd', xmlns='urn:schemas-upnp-org:service-1-0')
            elements(ET.SubElement(doc, 'specVersion'), major='1', minor='0')
            action_list = ET.SubElement(doc, 'actionList')
            state_table = ET.SubElement(doc, 'serviceStateTable')
            for datatype in SAMPLE:
                elements(ET.SubElement(state_table, 'stateVariable', sendEvents='no'),
                         name=f'fixture_{datatype}', dataType=datatype)
            for name, signature in actions.items():
                action = ET.SubElement(action_list, 'action')
                elements(action, name=name)
                args = ET.SubElement(action, 'argumentList')
                for direction, side in zip(('in', 'out'), signature.split(' -> '), strict=True):
                    for field in side.split():
                        key, kind = field.split(':')
                        elements(ET.SubElement(args, 'argument'), name=key, direction=direction,
                                 relatedStateVariable=f'fixture_{kind}')
            self.docs[BASE + path] = doc

    async def async_http_request(self, request):
        if request.method != 'GET' or request.url not in self.docs:
            raise AssertionError('reference replay only serves local descriptions')
        return HttpResponse(200, {}, ET.tostring(self.docs[request.url], encoding='unicode'))


class ReferenceCodecTests(unittest.IsolatedAsyncioTestCase):
    async def test_all_captured_actions_without_sending_any(self):
        ref = json.loads((Path(__file__).parent / 'testdata/wiim-ultra.json').read_text())
        cp = await ControlPoint.connect(BASE + 'description.xml', DescriptionReplay(ref))
        self.assertEqual(cp.inventory(), ref)
        for urn, actions in ref.items():
            for name, signature in actions.items():
                with self.subTest(service=urn, action=name):
                    inputs, outputs = [{k: SAMPLE[t] for k, t in (arg.split(':') for arg in side.split())}
                                       for side in signature.split(' -> ')]
                    action = cp.device.service(urn).action(name)
                    request = action.create_request(**inputs)
                    self.assertEqual(request.headers['SOAPAction'], f'"{urn}#{name}"')
                    call = ET.fromstring(request.body).find(f'{{{SOAP}}}Body/{{{urn}}}{name}')
                    self.assertIsNotNone(call)
                    self.assertEqual([arg.tag for arg in call], list(inputs))
                    for arg in call:
                        self.assertEqual(action.argument(arg.tag).coerce_python(arg.text), inputs[arg.tag])
                    envelope = ET.Element(f'{{{SOAP}}}Envelope')
                    response = ET.SubElement(ET.SubElement(envelope, f'{{{SOAP}}}Body'), f'{{{urn}}}{name}Response')
                    for key, val in outputs.items():
                        ET.SubElement(response, key).text = '1' if val is True else str(val)
                    wire = HttpResponse(200, {}, ET.tostring(envelope, encoding='unicode'))
                    self.assertEqual(action.parse_response(urn, wire), outputs)
