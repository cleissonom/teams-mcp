import importlib.util
import shutil
import subprocess
import tempfile
import unittest
import zipfile
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/package-release.py"
SPEC = importlib.util.spec_from_file_location("package_release", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class PackageReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.files = {
            "Cargo.toml": b'[package]\nname = "teams-mcp"\nversion = "0.1.0"\n',
            "README.md": b"Public source fixture.\n",
            "src/main.rs": b"fn main() {}\n",
        }
        for name, data in self.files.items():
            self.write(name, data)
        self.manifest(list(self.files) + ["release-files.txt"])

    def write(self, name, data=b"synthetic private fixture"):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def manifest(self, names):
        (self.root / "release-files.txt").write_text("\n".join(names) + "\n")

    def test_exact_source_archive_layout_integrity_and_read_only_inputs(self):
        for name in [
            ".private/evidence.json", "target/debug/teams-mcp", ".env",
            "dist/old.zip", "browser-profile/Default/Cookies", "profiles/user/data.json",
        ]:
            self.write(name)
        before = {name: (self.root / name).stat().st_mtime_ns for name in self.files}
        archive = MODULE.create_archive(self.root)
        self.assertEqual(archive, self.root / "dist/teams-mcp-0.1.0-source.zip")
        with zipfile.ZipFile(archive) as result:
            self.assertIsNone(result.testzip())
            self.assertEqual(set(result.namelist()), {
                f"teams-mcp/{name}" for name in [*self.files, "release-files.txt"]
            })
            for name, expected in self.files.items():
                self.assertEqual(result.read(f"teams-mcp/{name}"), expected)
        for name, expected in self.files.items():
            self.assertEqual((self.root / name).read_bytes(), expected)
            self.assertEqual((self.root / name).stat().st_mtime_ns, before[name])

    def test_forbidden_private_build_profile_credential_and_archive_entries_are_rejected(self):
        for name in [
            ".private/evidence.json", "target/main.rs", ".env", ".env.example",
            ".git/config", "cache/data.json", "build/main.rs", "dist/release.zip",
            "profiles/person/data.json", "browser-profile/Default/Cookies",
            "credentials.json", "tokens.json", "session-state.json", "backup.tar.gz",
            "credentials.txt", "api-secrets.toml", "passwords.yaml", "sessions/account.json",
            "certificate.key", "capture.har", "cookies.sqlite",
        ]:
            with self.subTest(name=name):
                self.write(name)
                self.manifest(["Cargo.toml", name])
                with self.assertRaises(ValueError):
                    MODULE.create_archive(self.root)

    def test_absolute_traversal_and_noncanonical_manifest_paths_are_rejected(self):
        for name in ["/etc/passwd", "../outside.md", "src/../../outside.md",
                     "C:/Users/test/source.md", "src\\main.rs", "src/./main.rs", "src//main.rs"]:
            with self.subTest(name=name):
                self.manifest(["Cargo.toml", name])
                with self.assertRaises(ValueError):
                    MODULE.create_archive(self.root)
        self.assertFalse((self.root / "dist").exists())

    def test_archive_contents_and_metadata_are_reproducible(self):
        first = MODULE.create_archive(self.root).read_bytes()
        second = MODULE.create_archive(self.root).read_bytes()
        self.assertEqual(first, second)

    def test_missing_duplicate_and_empty_manifest_entries_fail_closed(self):
        for names in [["missing.md"], ["README.md", "README.md"], []]:
            with self.subTest(names=names):
                self.manifest(names)
                with self.assertRaises(ValueError):
                    MODULE.create_archive(self.root)

    def test_file_symlinks_and_parent_directory_symlink_escapes_are_rejected(self):
        with tempfile.TemporaryDirectory() as external:
            outside = Path(external)
            (outside / "payload.md").write_text("external synthetic fixture")
            (self.root / "NOTICE").symlink_to(outside / "payload.md")
            (self.root / "linked-docs").symlink_to(outside, target_is_directory=True)
            for name in ["NOTICE", "linked-docs/payload.md"]:
                with self.subTest(name=name):
                    self.manifest(["Cargo.toml", name])
                    with self.assertRaises(ValueError):
                        MODULE.create_archive(self.root)
        self.assertFalse((self.root / "dist").exists())

    def test_manifest_and_output_directory_symlinks_are_rejected(self):
        manifest = self.root / "release-files.txt"
        manifest.unlink()
        manifest.symlink_to(self.root / "README.md")
        with self.assertRaises(ValueError):
            MODULE.create_archive(self.root)
        manifest.unlink()
        self.manifest(["Cargo.toml", "README.md"])
        with tempfile.TemporaryDirectory() as external:
            (self.root / "dist").symlink_to(external, target_is_directory=True)
            with self.assertRaises(ValueError):
                MODULE.create_archive(self.root)
            self.assertEqual(list(Path(external).iterdir()), [])


class RepositoryManifestTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("cargo"), "cargo is required to list the crate package")
    def test_release_manifest_matches_the_cargo_package_include_list(self):
        root = SCRIPT.parents[1]
        listed = subprocess.run(
            ["cargo", "package", "--list", "--allow-dirty", "--locked"],
            cwd=root, check=True, capture_output=True, text=True,
        ).stdout.split()
        generated = {".cargo_vcs_info.json", "Cargo.toml.orig"}
        manifest = [line for line in (root / "release-files.txt").read_text().splitlines() if line.strip()]
        self.assertEqual(sorted(set(listed) - generated), sorted(manifest))


if __name__ == "__main__":
    unittest.main()
