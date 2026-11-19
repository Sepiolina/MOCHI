import json
import pathlib

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[3]
GOLDEN = ROOT / "fixtures" / "golden"
RATIFICATION = ROOT / "docs" / "ratification"
GOLDEN_PASSPHRASE = "golden passphrase"      # found by trial; see READING-LOG.md (c11 archive)


@pytest.fixture(scope="session")
def golden():
    if not GOLDEN.is_dir():
        pytest.skip("fixtures/golden is absent")
    return GOLDEN


def read(path) -> bytes:
    return pathlib.Path(path).read_bytes()


def vectors_lines(path):
    """(name, tokens) for every non-comment line of a vectors.txt."""
    for line in pathlib.Path(path).read_text().splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        t = line.split()
        yield t[0], t[1:]


@pytest.fixture(scope="session")
def r5():
    return json.loads((RATIFICATION / "R5-vectors" / "vectors.json").read_text())


@pytest.fixture(scope="session")
def r6():
    return json.loads((RATIFICATION / "R6-vectors" / "vectors.json").read_text())
