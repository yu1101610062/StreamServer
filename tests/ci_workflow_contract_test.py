from pathlib import Path
import tempfile
import unittest

import yaml


REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW_PATH = REPO_ROOT / ".github" / "workflows" / "server-ci.yml"
REQUIREMENTS_PATH = REPO_ROOT / "tests" / "requirements-ci.txt"
ENGLISH_TESTING_DOC = REPO_ROOT / "docs" / "en" / "09-testing.md"
BRANCH_WORKFLOW_DOC = REPO_ROOT / "docs" / "zh" / "11-development-workflow.md"
ENVIRONMENT_DOC = REPO_ROOT / "docs" / "zh" / "16-environment-and-dependencies.md"
README_PATH = REPO_ROOT / "README.md"

CONTRACT_DEPENDENCY_INSTALL = (
    "python3 -m pip install --disable-pip-version-check --no-deps "
    "-r tests/requirements-ci.txt"
)
RUST_QUALITY_GATES = (
    "python3 tests/ci_workflow_contract_test.py",
    "cargo fmt --all -- --check",
    "cargo clippy --locked --workspace --all-targets -- -D warnings",
    "cargo test --locked --workspace --all-targets",
)
FRONTEND_QUALITY_GATES = (
    "npm ci",
    "npm run typecheck",
    "npm run test",
)

RUST_RUN_STEPS = {
    "Verify Linux AMD64 runner": (
        'test "$(uname -s)" = "Linux"\n'
        'test "$(uname -m)" = "x86_64"'
    ),
    "Install Linux build dependencies": (
        "sudo apt-get update\n"
        "sudo apt-get install -y --no-install-recommends \\\n"
        "  libdbus-1-dev \\\n"
        "  pkg-config \\\n"
        "  postgresql-client \\\n"
        "  protobuf-compiler"
    ),
    "Set up Rust 1.85.0": (
        "rustup toolchain install 1.85.0 \\\n"
        "  --profile minimal \\\n"
        "  --component rustfmt \\\n"
        "  --component clippy\n"
        "rustup default 1.85.0\n"
        "rustc --version"
    ),
    "Install CI contract dependencies": CONTRACT_DEPENDENCY_INSTALL,
    "Validate CI workflow contract": RUST_QUALITY_GATES[0],
    "Verify PostgreSQL connection": (
        'psql "${TEST_DATABASE_URL}" -v ON_ERROR_STOP=1 -c "select 1"'
    ),
    "Check Rust formatting": RUST_QUALITY_GATES[1],
    "Run Clippy": RUST_QUALITY_GATES[2],
    "Test full Rust workspace": RUST_QUALITY_GATES[3],
}
RUST_USES_STEPS = {"Checkout": "actions/checkout@v4"}
FRONTEND_RUN_STEPS = {
    "Install frontend dependencies": FRONTEND_QUALITY_GATES[0],
    "Type-check frontend": FRONTEND_QUALITY_GATES[1],
    "Test frontend": FRONTEND_QUALITY_GATES[2],
}
FRONTEND_USES_STEPS = {
    "Checkout": "actions/checkout@v4",
    "Set up Node.js 20": "actions/setup-node@v4",
}


def normalized_script(value: object) -> str:
    return str(value).replace("\r\n", "\n").strip()


def mapping_value(parent: object, key: str, path: str, errors: list[str]) -> dict:
    if not isinstance(parent, dict):
        errors.append(f"{path} must be a mapping")
        return {}
    value = parent.get(key)
    if not isinstance(value, dict):
        errors.append(f"{path}.{key} must be a mapping")
        return {}
    return value


def steps_by_name(job: dict, job_name: str, errors: list[str]) -> dict[str, dict]:
    steps = job.get("steps")
    if not isinstance(steps, list):
        errors.append(f"jobs.{job_name}.steps must be a list")
        return {}

    indexed: dict[str, dict] = {}
    for step in steps:
        if not isinstance(step, dict) or not isinstance(step.get("name"), str):
            errors.append(f"jobs.{job_name} contains a step without a string name")
            continue
        name = step["name"]
        if name in indexed:
            errors.append(f"jobs.{job_name} has duplicate step name {name!r}")
            continue
        indexed[name] = step
    return indexed


def validate_steps(
    job: dict,
    job_name: str,
    expected_runs: dict[str, str],
    expected_uses: dict[str, str],
    errors: list[str],
) -> dict[str, dict]:
    indexed = steps_by_name(job, job_name, errors)
    expected_names = set(expected_runs) | set(expected_uses)
    actual_names = set(indexed)
    if actual_names != expected_names:
        errors.append(
            f"jobs.{job_name}.steps names must be exactly {sorted(expected_names)!r}; "
            f"got {sorted(actual_names)!r}"
        )

    for name, expected_run in expected_runs.items():
        step = indexed.get(name, {})
        if set(step) - {"name", "run", "shell"}:
            errors.append(f"jobs.{job_name} step {name!r} has unexpected keys")
        if normalized_script(step.get("run", "")) != normalized_script(expected_run):
            errors.append(
                f"jobs.{job_name} step {name!r} must directly run {expected_run!r}"
            )

    for name, expected_uses_value in expected_uses.items():
        step = indexed.get(name, {})
        if step.get("uses") != expected_uses_value:
            errors.append(
                f"jobs.{job_name} step {name!r} must use {expected_uses_value!r}"
            )
        if "run" in step:
            errors.append(f"jobs.{job_name} step {name!r} must not contain run")

    return indexed


def workflow_contract_errors(workflow_text: str) -> list[str]:
    errors: list[str] = []
    try:
        workflow = yaml.load(workflow_text, Loader=yaml.BaseLoader)
    except yaml.YAMLError as error:
        return [f"workflow must be valid YAML: {error}"]
    if not isinstance(workflow, dict):
        return ["workflow root must be a mapping"]

    triggers = mapping_value(workflow, "on", "workflow", errors)
    for event in ("push", "pull_request"):
        event_config = mapping_value(triggers, event, "workflow.on", errors)
        branches = event_config.get("branches")
        if not isinstance(branches, list) or set(branches) != {"master", "DEV"}:
            errors.append(f"workflow.on.{event}.branches must be master and DEV")

    permissions = mapping_value(workflow, "permissions", "workflow", errors)
    if permissions.get("contents") != "read":
        errors.append("workflow.permissions.contents must be read")

    workflow_env = mapping_value(workflow, "env", "workflow", errors)
    expected_env = {
        "CARGO_TERM_COLOR": "always",
        "REQUIRE_TEST_DATABASE": "1",
        "TEST_DATABASE_URL": "postgresql://postgres:test@127.0.0.1:5432/postgres",
    }
    for key, value in expected_env.items():
        if workflow_env.get(key) != value:
            errors.append(f"workflow.env.{key} must be {value!r}")

    jobs = mapping_value(workflow, "jobs", "workflow", errors)
    if set(jobs) != {"rust", "frontend"}:
        errors.append("workflow.jobs must contain only rust and frontend")
    rust = mapping_value(jobs, "rust", "workflow.jobs", errors)
    frontend = mapping_value(jobs, "frontend", "workflow.jobs", errors)

    if rust.get("runs-on") != "ubuntu-24.04":
        errors.append("jobs.rust.runs-on must be ubuntu-24.04")
    strategy = mapping_value(rust, "strategy", "jobs.rust", errors)
    matrix = mapping_value(strategy, "matrix", "jobs.rust.strategy", errors)
    postgres_versions = matrix.get("postgres")
    if not isinstance(postgres_versions, list) or set(postgres_versions) != {"16", "18.3"}:
        errors.append("jobs.rust.strategy.matrix.postgres must contain 16 and 18.3")

    services = mapping_value(rust, "services", "jobs.rust", errors)
    postgres = mapping_value(services, "postgres", "jobs.rust.services", errors)
    if postgres.get("image") != "postgres:${{ matrix.postgres }}":
        errors.append("jobs.rust.services.postgres.image must use matrix.postgres")
    postgres_env = mapping_value(postgres, "env", "jobs.rust.services.postgres", errors)
    if postgres_env != {
        "POSTGRES_DB": "postgres",
        "POSTGRES_USER": "postgres",
        "POSTGRES_PASSWORD": "test",
    }:
        errors.append("jobs.rust.services.postgres.env must use the isolated test account")
    if postgres.get("ports") != ["5432:5432"]:
        errors.append("jobs.rust.services.postgres must expose 5432:5432")

    validate_steps(rust, "rust", RUST_RUN_STEPS, RUST_USES_STEPS, errors)
    frontend_steps = validate_steps(
        frontend,
        "frontend",
        FRONTEND_RUN_STEPS,
        FRONTEND_USES_STEPS,
        errors,
    )

    defaults = mapping_value(frontend, "defaults", "jobs.frontend", errors)
    default_run = mapping_value(defaults, "run", "jobs.frontend.defaults", errors)
    if default_run.get("working-directory") != "crates/media-core/frontend":
        errors.append("jobs.frontend defaults must target crates/media-core/frontend")
    node_setup = frontend_steps.get("Set up Node.js 20", {})
    node_with = mapping_value(
        node_setup,
        "with",
        "jobs.frontend.steps.Set up Node.js 20",
        errors,
    )
    if node_with.get("node-version") != "20":
        errors.append("frontend Node.js version must be 20")
    if node_with.get("cache-dependency-path") != (
        "crates/media-core/frontend/package-lock.json"
    ):
        errors.append("frontend npm cache must use the checked-in lockfile")

    return errors


class ServerCiWorkflowContractTest(unittest.TestCase):
    def assert_workflow_is_rejected(self, workflow: str) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            workflow_path = Path(temporary_directory) / "server-ci.yml"
            workflow_path.write_text(workflow, encoding="utf-8")
            candidate = workflow_path.read_text(encoding="utf-8")

        parsed_candidate = yaml.load(candidate, Loader=yaml.BaseLoader)
        self.assertIsInstance(parsed_candidate, dict, "negative fixture must remain valid YAML")
        errors = workflow_contract_errors(candidate)
        self.assertTrue(errors, "the workflow contract accepted a non-executing gate")

    def test_server_ci_matches_linux_quality_gate_contract(self) -> None:
        self.assertEqual(
            REQUIREMENTS_PATH.read_text(encoding="utf-8").splitlines(),
            ["PyYAML==6.0.2"],
        )
        errors = workflow_contract_errors(WORKFLOW_PATH.read_text(encoding="utf-8"))
        self.assertEqual(errors, [], "\n".join(errors))

        testing_doc = ENGLISH_TESTING_DOC.read_text(encoding="utf-8")
        for command in (
            CONTRACT_DEPENDENCY_INSTALL,
            *RUST_QUALITY_GATES,
            *FRONTEND_QUALITY_GATES,
        ):
            self.assertIn(command, testing_doc)
        self.assertIn("export REQUIRE_TEST_DATABASE=1", testing_doc)
        self.assertIn("Linux AMD64", testing_doc)
        self.assertIn("not a server regression", testing_doc)
        self.assertIn("server-native-bundles.yml", testing_doc)
        self.assertIn("for POSTGRES_VERSION in 16 18.3", testing_doc)

    def test_repository_docs_point_to_the_canonical_linux_gate(self) -> None:
        branch_doc = BRANCH_WORKFLOW_DOC.read_text(encoding="utf-8")
        self.assertIn("`master` 为可发布分支", branch_doc)
        self.assertIn("`DEV` 为日常集成分支", branch_doc)
        self.assertIn("从 `DEV` 向 `master` 提交发布 PR", branch_doc)
        self.assertNotIn("`main` 保持可发布状态", branch_doc)

        readme = README_PATH.read_text(encoding="utf-8")
        self.assertIn("快速 smoke 子集（不替代完整 Linux 质量门）", readme)
        self.assertIn("[完整 Linux 质量门](docs/zh/09-testing.md)", readme)
        self.assertIn("Quick smoke subset (not the complete Linux quality gate)", readme)
        self.assertIn("[complete Linux quality gate](docs/en/09-testing.md)", readme)

        environment_doc = ENVIRONMENT_DOC.read_text(encoding="utf-8")
        self.assertIn("PostgreSQL 16 是最低兼容基线", environment_doc)
        self.assertIn("CI 同时覆盖 16 和 18.3", environment_doc)
        self.assertIn("native bundle 默认携带 18.3", environment_doc)

    def test_echoed_commands_do_not_satisfy_the_contract(self) -> None:
        workflow = WORKFLOW_PATH.read_text(encoding="utf-8")
        for command in (*RUST_QUALITY_GATES, *FRONTEND_QUALITY_GATES):
            workflow = workflow.replace(
                f"run: {command}",
                f'run: echo "{command}"',
            )

        self.assert_workflow_is_rejected(workflow)

    def test_commented_commands_do_not_satisfy_the_contract(self) -> None:
        workflow = WORKFLOW_PATH.read_text(encoding="utf-8")
        for command in (*RUST_QUALITY_GATES, *FRONTEND_QUALITY_GATES):
            workflow = workflow.replace(
                f"run: {command}",
                f"run: echo skipped # {command}",
            )

        self.assert_workflow_is_rejected(workflow)

    def test_commands_in_the_wrong_job_do_not_satisfy_the_contract(self) -> None:
        workflow = WORKFLOW_PATH.read_text(encoding="utf-8")
        for command in RUST_QUALITY_GATES:
            workflow = workflow.replace(
                f"        run: {command}",
                "        uses: actions/checkout@v4",
                1,
            )
            workflow += f"\n      - name: Misplaced gate\n        run: {command}\n"

        self.assert_workflow_is_rejected(workflow)


if __name__ == "__main__":
    unittest.main()
