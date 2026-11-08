import importlib.util
import json
import os
import shutil
import tempfile
import unittest
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))


def load_app(home, repos):
    os.environ["CODE_INDEX_HOME"] = home
    os.environ["ADMIN_REPOS_ROOT"] = repos
    spec = importlib.util.spec_from_file_location("admin_app_test", os.path.join(HERE, "app.py"))
    app = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(app)
    app.SERVE_UNIT = ""
    return app


class AdminPanelTest(unittest.TestCase):
    def setUp(self):
        self.base = tempfile.mkdtemp(prefix="ci-admin-", dir=HERE)
        self.home = os.path.join(self.base, "home")
        self.repos = os.path.join(self.base, "repos")
        os.makedirs(self.home)
        os.makedirs(self.repos)
        self.cfg = os.path.join(self.home, "daemon.toml")

    def tearDown(self):
        shutil.rmtree(self.base, ignore_errors=True)

    def write_cfg(self, text):
        with open(self.cfg, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)

    def read_cfg(self):
        with open(self.cfg, encoding="utf-8") as f:
            return f.read()

    def test_write_paths_preserves_sections_and_comments(self):
        self.write_cfg(
            "# заголовок\n"
            "[daemon]\n"
            "http_port = 0\n\n"
            "[[paths]]\n"
            'path = "/srv/repos/erp"\n'
            'language = "bsl"   # комментарий внутри секции\n\n'
            "[[paths]]\n"
            'path = "/srv/repos/bp"\n\n'
            "# перед tools\n"
            "[tools]\n"
            'enabled = ["get_function"]\n\n'
            "[mcp]\n"
            'default_repo = "ut"\n\n'
            "[[cache_targets]]\n"
            'url = "http://127.0.0.1:8011"\n'
        )
        app = load_app(self.home, self.repos)
        before = app.config_paths()
        app.write_paths(before)
        text = self.read_cfg()
        for marker in ("[tools]", "[mcp]", "[[cache_targets]]", "# перед tools",
                       "# комментарий внутри секции"):
            self.assertIn(marker, text)
        self.assertEqual(app.config_paths(), before)

    def test_derived_alias_add_and_remove(self):
        self.write_cfg(
            "[daemon]\n"
            "http_port = 0\n\n"
            "[[paths]]\n"
            'path = "/srv/repos/erp"\n'
            'language = "bsl"\n\n'
            "[tools]\n"
            'enabled = ["get_function"]\n'
        )
        app = load_app(self.home, self.repos)
        self.assertEqual(app.effective_alias(app.config_paths()[0]), "erp")
        self.assertIsNone(app.add_path("bp", "/srv/repos/bp", False))
        self.assertEqual([app.effective_alias(p) for p in app.config_paths()], ["erp", "bp"])
        self.assertIsNone(app.remove_path("erp"))
        self.assertEqual([app.effective_alias(p) for p in app.config_paths()], ["bp"])
        self.assertIn("[tools]", self.read_cfg())

    def test_quoted_alias_roundtrip(self):
        self.write_cfg('[[paths]]\npath = "/srv/repos/q"\nalias = "a\\"b"\n')
        app = load_app(self.home, self.repos)
        app.write_paths(app.config_paths())
        self.assertEqual(app.config_paths()[0]["alias"], 'a"b')

    def test_write_paths_rejects_bad_value(self):
        self.write_cfg('[[paths]]\npath = "/srv/repos/demo"\n')
        app = load_app(self.home, self.repos)
        before = self.read_cfg()
        with self.assertRaises(RuntimeError):
            app.write_paths([{"path": "/srv/repos/demo", "extra": {"nested": 1}}])
        self.assertEqual(self.read_cfg(), before)

    def test_zip_traversal_rejected(self):
        zpath = os.path.join(self.base, "bad.zip")
        with zipfile.ZipFile(zpath, "w") as z:
            z.writestr("../evil.txt", "x")
        app = load_app(self.home, self.repos)
        with self.assertRaises(RuntimeError):
            app.do_unzip("bad", zpath)
        self.assertFalse(os.path.exists(os.path.join(self.base, "evil.txt")))
        self.assertFalse(os.path.exists(os.path.join(self.repos, "bad")))

    def test_zip_size_limit(self):
        zpath = os.path.join(self.base, "big.zip")
        with zipfile.ZipFile(zpath, "w", compression=zipfile.ZIP_DEFLATED) as z:
            z.writestr("big.bin", b"\0" * 2_000_000)
        app = load_app(self.home, self.repos)
        with zipfile.ZipFile(zpath) as z:
            with self.assertRaises(RuntimeError):
                app.check_zip_limits(z, 1_000_000)

    def test_symlink_is_not_deletable(self):
        target = os.path.join(self.repos, "target")
        link = os.path.join(self.repos, "link")
        os.makedirs(target)
        try:
            os.symlink(target, link)
        except (OSError, NotImplementedError):
            self.skipTest("символьные ссылки недоступны")
        app = load_app(self.home, self.repos)
        self.assertFalse(app.safe_repo_dir(link))

    def test_overview_matches_canonical_path(self):
        repo = os.path.join(self.repos, "ut")
        os.makedirs(repo)
        self.write_cfg(f"[[paths]]\npath = {json.dumps(repo + os.sep)}\n")
        app = load_app(self.home, self.repos)
        app.daemon_health = lambda: {
            "status": "running", "version": "test", "uptime_sec": 1,
            "paths": [{"path": repo, "status": "ready"}],
        }
        app.meminfo = lambda: {"total": 100, "available": 50}
        data = app.overview()
        self.assertEqual(data["repos"][0]["alias"], "ut")
        self.assertEqual(data["repos"][0]["status"], "ready")


if __name__ == "__main__":
    unittest.main()
