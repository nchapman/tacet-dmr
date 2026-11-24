"""Volume policy can be tested before Play opens any physical device."""
import sys

from controlpoint import RC
from peer import PeerTest


def _nonexistent_exclusive_device():
    if sys.platform == 'darwin':
        return 'coreaudio/tacet-test-nonexistent'
    if sys.platform == 'win32':
        return 'wasapi/tacet-test-nonexistent'
    return 'alsa/hw:99,0'


class ExclusiveVolumeTests(PeerTest):
    device = _nonexistent_exclusive_device()
    extra_args = ('-exclusive',)

    async def test_startup_and_factory_reset_are_not_full_gain(self):
        master = dict(InstanceID=0, Channel='Master')
        volume = await self.cp.call(RC, 'GetVolume', **master)
        self.assertEqual(volume['CurrentVolume'], 30)
        await self.cp.call(RC, 'SetVolume', **master, DesiredVolume=45)
        await self.cp.call(RC, 'SelectPreset', InstanceID=0, PresetName='FactoryDefaults')
        volume = await self.cp.call(RC, 'GetVolume', **master)
        self.assertEqual(volume['CurrentVolume'], 30)


class StartupVolumeOverrideTests(PeerTest):
    device = _nonexistent_exclusive_device()
    extra_args = ('-volume', '0', '-exclusive')

    async def test_explicit_zero_is_not_treated_as_a_missing_setting(self):
        volume = await self.cp.call(RC, 'GetVolume', InstanceID=0, Channel='Master')
        self.assertEqual(volume['CurrentVolume'], 0)
