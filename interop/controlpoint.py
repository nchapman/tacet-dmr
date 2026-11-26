"""Independent control-point adapter built on async_upnp_client.

Builds typed actions directly from the target device's SCPDs for both OpenHome
Media and standard UPnP AV services.
"""
from urllib.parse import urlsplit

from async_upnp_client.client import UpnpRequester
from async_upnp_client.const import HttpResponse
from async_upnp_client.client_factory import UpnpFactory

# Standard UPnP AV service URNs
AV = 'urn:schemas-upnp-org:service:AVTransport:1'
RC = 'urn:schemas-upnp-org:service:RenderingControl:1'
CM = 'urn:schemas-upnp-org:service:ConnectionManager:1'

# OpenHome Media service URNs
OH_PRODUCT = 'urn:av-openhome-org:service:Product:1'
OH_PLAYLIST = 'urn:av-openhome-org:service:Playlist:1'
OH_INFO = 'urn:av-openhome-org:service:Info:1'
OH_TIME = 'urn:av-openhome-org:service:Time:1'
OH_VOLUME = 'urn:av-openhome-org:service:Volume:1'
OH_RECEIVER = 'urn:av-openhome-org:service:Receiver:1'
OH_SENDER = 'urn:av-openhome-org:service:Sender:1'


class OriginRequester(UpnpRequester):
    """Bound requests to the selected origin, even if an SCPD or redirect points elsewhere."""
    def __init__(self, location, session):
        self.origin = urlsplit(location)
        self.session = session

    def permitted(self, req):
        url = urlsplit(req.url)
        return (url.scheme, url.netloc) == (self.origin.scheme, self.origin.netloc)

    async def async_http_request(self, req):
        if not self.permitted(req):
            raise ValueError('refusing request outside the selected device policy')
        async with self.session.request(req.method, req.url, headers=req.headers, data=req.body,
                                        allow_redirects=False) as res:
            if 300 <= res.status < 400:
                raise ValueError('device redirect refused')
            data = bytearray()
            async for chunk in res.content.iter_chunked(65536):
                data.extend(chunk)
                if len(data) > 2 << 20:
                    raise ValueError('oversize device response')
            try:
                text = data.decode('utf-8-sig')
            except UnicodeDecodeError as err:
                raise ValueError('expected UTF-8 device XML') from err
            return HttpResponse(res.status, res.headers, text)


class ControlPoint:
    def __init__(self, requester, device):
        self.requester = requester
        self.device = device
        self.exercised = set()

    @classmethod
    async def connect(cls, location, requester):
        device = await UpnpFactory(requester).async_create_device(location)
        return cls(requester, device)

    def inventory(self):
        def args(arguments):
            return ' '.join(f'{a.name}:{a.related_state_variable.data_type}' for a in arguments)
        return {s.service_type: {name: f'{args(a.in_arguments())} -> {args(a.out_arguments())}'
                                 for name, a in s.actions.items()} for s in self.device.all_services}

    async def call(self, urn, action, **kwargs):
        cmd = self.device.service(urn).action(action)
        reply = await cmd.async_call(**kwargs)
        expected = {a.name for a in cmd.out_arguments()}
        if set(reply) != expected:
            raise AssertionError(f'{action}: expected outputs {expected}, received {set(reply)}')
        self.exercised.add((urn, action))
        return reply
