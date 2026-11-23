"""Read-only hardware comparison using the SAME control point as the local tests."""
import argparse
import asyncio
import ipaddress
import json
import sys
from urllib.parse import urlsplit

from aiohttp import ClientSession, ClientTimeout, TCPConnector
from async_upnp_client.exceptions import UpnpActionResponseError
from defusedxml.ElementTree import fromstring

from controlpoint import ControlPoint, OriginRequester, PQ

GETTERS = {'GetQueueIndex': {'QueueName': 'CurrentQueue'}, 'GetQueueLoopMode': {}}
SOAP = 'http://schemas.xmlsoap.org/soap/envelope/'


class ReadOnlyDevice(OriginRequester):
    """No general SOAP, subscriptions, proxies, redirects or off-device links."""
    def __init__(self, location, session):
        super().__init__(location, session)
        ip = ipaddress.IPv4Address(self.origin.hostname)
        if self.origin.scheme != 'http' or self.origin.username is not None or not any(
            ip in ipaddress.ip_network(block) for block in ('10.0.0.0/8', '172.16.0.0/12', '192.168.0.0/16', '127.0.0.0/8')
        ):
            raise ValueError('expected an HTTP description on a literal private IPv4 address')
        self.queue_url = None  # installed only after fetching the device's own description

    def permitted(self, req):
        url = urlsplit(req.url)
        if url.scheme != self.origin.scheme or url.netloc != self.origin.netloc or url.query or url.fragment:
            return False
        if req.method == 'GET':
            return url.path.lower().endswith('.xml') and not req.body
        if req.method != 'POST' or req.url != self.queue_url:
            return False
        soap_action = next((v.strip('"') for k, v in req.headers.items() if k.lower() == 'soapaction'), '')
        namespace, _, name = soap_action.partition('#')
        if namespace != PQ or name not in GETTERS:
            return False
        # Do not trust the header alone: allow only the corresponding fixed body.
        root = fromstring(req.body or '')
        body = root.find(f'{{{SOAP}}}Body')
        if root.tag != f'{{{SOAP}}}Envelope' or body is None or len(body) != 1 or body[0].tag != f'{{{PQ}}}{name}':
            return False
        return [(a.tag, a.text) for a in body[0]] == list(GETTERS[name].items())


async def capture(location, query):
    # Some embedded UPnP servers close keep-alive connections without notice.
    async with ClientSession(timeout=ClientTimeout(total=4), connector=TCPConnector(force_close=True), trust_env=False) as session:
        transport = ReadOnlyDevice(location, session)
        cp = await ControlPoint.connect(location, transport)
        if query and cp.device.has_service(PQ):
            service = cp.device.service(PQ)
            transport.queue_url = service.control_url
            for name, args in GETTERS.items():
                if service.has_action(name):
                    try:
                        result = await cp.call(PQ, name, **args)
                        print(f'{name}: {", ".join(result)}', file=sys.stderr)
                    except UpnpActionResponseError as err:
                        print(f'{name}: SOAP fault {err.error_code}', file=sys.stderr)
        return cp.inventory()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('description_url')
    parser.add_argument('--query', action='store_true')
    args = parser.parse_args()
    print(json.dumps(asyncio.run(capture(args.description_url, args.query)), sort_keys=True, indent=2))
