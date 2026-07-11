from pathlib import Path
import re
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW_PATH = REPO_ROOT / ".github" / "workflows" / "server-ci.yml"
ENGLISH_TESTING_DOC = REPO_ROOT / "docs" / "en" / "09-testing.md"

RUST_QUALITY_GATES = (
    "python3 tests/ci_workflow_contract_test.py",
    "cargo fmt --all -- --check",
    "cargo clippy --workspace --all-targets -- -D warnings",
    "cargo test --workspace --all-targets",
)

FRONTEND_QUALITY_GATES = (
    "npm ci",
    "npm run typecheck",
    "npm run test",
)


class ServerCiWorkflowContractTest(unittest.TestCase):
    def test_server_ci_matches_linux_quality_gate_contract(self) -> None:
        self.assertTrue(
            WORKFLOW_PATH.is_file(),
            "server CI must live in .github/workflows/server-ci.yml",
        )
        workflow = WORKFLOW_PATH.read_text(encoding="utf-8")

        for event in ("push", "pull_request"):
            self.assertRegex(
                workflow,
                rf"(?ms)^  {event}:\s*\n    branches:\s*\n"
                rf"      - master\s*\n      - DEV\s*$",
                f"{event} must target master and DEV",
            )

        self.assertIn("runs-on: ubuntu-24.04", workflow)
        self.assertIn('test "$(uname -m)" = "x86_64"', workflow)
        self.assertRegex(
            workflow,
            r'(?ms)^\s+matrix:\s*\n\s+postgres:\s*\n'
            r'\s+- "16"\s*\n\s+- "18\.3"\s*$',
            "Rust workspace tests must cover PostgreSQL 16 and 18.3",
        )
        self.assertIn("image: postgres:${{ matrix.postgres }}", workflow)
        self.assertIn("POSTGRES_PASSWORD: test", workflow)
        self.assertIn(
            "TEST_DATABASE_URL: postgresql://postgres:test@127.0.0.1:5432/postgres",
            workflow,
        )
        self.assertIn("node-version: \"20\"", workflow)
        self.assertIn("working-directory: crates/media-core/frontend", workflow)

        for command in (*RUST_QUALITY_GATES, *FRONTEND_QUALITY_GATES):
            self.assertIn(command, workflow)

        self.assertNotIn("build-native-bundle", workflow)

        testing_doc = ENGLISH_TESTING_DOC.read_text(encoding="utf-8")
        for command in (*RUST_QUALITY_GATES, *FRONTEND_QUALITY_GATES):
            self.assertIn(command, testing_doc)
        self.assertIn("Linux AMD64", testing_doc)
        self.assertIn("not a server regression", testing_doc)
        self.assertIn("server-native-bundles.yml", testing_doc)
        self.assertIn("for POSTGRES_VERSION in 16 18.3", testing_doc)


if __name__ == "__main__":
    unittest.main()
