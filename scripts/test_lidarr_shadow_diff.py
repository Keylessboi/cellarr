"""Self-test for lidarr-shadow-diff: python3 -I scripts/test_lidarr_shadow_diff.py"""
import copy
import importlib.machinery
import importlib.util
import os
import types
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
_loader = importlib.machinery.SourceFileLoader("shadowdiff", os.path.join(HERE, "lidarr-shadow-diff"))
_spec = importlib.util.spec_from_loader("shadowdiff", _loader)
sd = importlib.util.module_from_spec(_spec)
_loader.exec_module(sd)


class FakeApi:
    def __init__(self, data):
        self.data = data

    def get(self, path, timeout=300):
        return copy.deepcopy(self.data[path])

    def paged(self, path):
        rows = copy.deepcopy(self.data[path.split("?")[0]])
        return rows, len(rows)


def base():
    return {
        "/qualityprofile": [{"id": 1, "name": "Any", "cutoff": 1, "upgradeAllowed": True}],
        "/metadataprofile": [{"id": 1, "name": "Std"}],
        "/rootfolder": [{"id": 1, "path": "/music/"}],
        "/indexer": [{"name": "idx", "implementation": "Tubifarry", "enableRss": True}],
        "/downloadclient": [{"name": "dc", "implementation": "Soulseek", "enable": True,
                             "fields": [{"name": "musicCategory", "value": "music"}]}],
        "/artist": [{"id": 1, "artistName": "A", "foreignArtistId": "x", "path": "/music/A",
                     "monitored": True, "monitorNewItems": "all", "qualityProfileId": 1,
                     "metadataProfileId": 1, "statistics": {"trackFileCount": None}}],
        "/trackfile?artistId=1": [{"id": 5, "path": "/music/A/1.flac", "albumId": 2, "size": 10,
                                   "quality": {"quality": {"name": "FLAC"}}}],
        "/wanted/missing": [{"id": 2}],
        "/queue": [{"downloadId": "abc"}],
    }


def run(live, shadow, known=True):
    args = types.SimpleNamespace(max_shown=20, known=known, show_expected=True, no_albums=True, sample=0)
    return sd.run(args, FakeApi(live), FakeApi(shadow))


class Known(unittest.TestCase):
    def expected_only(self):
        live, shadow = base(), base()
        shadow["/queue"] = []
        shadow["/indexer"][0]["implementation"] = "Torznab"
        shadow["/downloadclient"][0]["implementation"] = "Blackhole"
        shadow["/downloadclient"][0]["fields"][0]["value"] = "other"
        shadow["/artist"][0]["statistics"]["trackFileCount"] = 0
        shadow["/qualityprofile"][0]["cutoff"] = 9
        shadow["/rootfolder"][0]["id"] = 7
        return live, shadow

    def test_expected_do_not_fail(self):
        text, bad = run(*self.expected_only())
        self.assertEqual(bad, 0, text)
        self.assertIn("EXPECTED (7)", text)

    def test_without_flag_same_input_fails(self):
        text, bad = run(*self.expected_only(), known=False)
        self.assertGreater(bad, 0)
        self.assertNotIn("EXPECTED", text)

    def test_clean_is_clean(self):
        self.assertEqual(run(base(), base())[1], 0)

    def test_unexpected_still_fails(self):
        for mutate in [
            lambda s: s["/artist"][0].update(path="/other"),
            lambda s: s["/qualityprofile"][0].update(name="Renamed"),
            lambda s: s["/metadataprofile"][0].update(name="Renamed"),
            lambda s: s["/indexer"][0].update(name="renamed"),
            lambda s: s["/indexer"][0].update(enableRss=False),
            lambda s: s["/rootfolder"][0].update(path="/elsewhere"),
            lambda s: s["/trackfile?artistId=1"][0].update(size=11),
            lambda s: s["/wanted/missing"].append({"id": 3}),
            lambda s: s["/artist"][0]["statistics"].update(trackFileCount=3),  # None vs 3, not 0
        ]:
            live, shadow = base(), base()
            mutate(shadow)
            text, bad = run(live, shadow)
            self.assertGreater(bad, 0, text)

    def test_expected_mixed_with_real_diff_fails(self):
        live, shadow = self.expected_only()
        shadow["/trackfile?artistId=1"] = []
        text, bad = run(live, shadow)
        self.assertEqual(bad, 1, text)


if __name__ == "__main__":
    unittest.main(verbosity=2)
