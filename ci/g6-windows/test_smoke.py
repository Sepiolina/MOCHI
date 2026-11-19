"""Self-test for orchestrate.evaluate_smoke(): the verdict logic of the installer smoke run.

    python3 -I ci/g6-windows/test_smoke.py

A guest result of FAIL or ERROR, a required check that never reported, or a guest
that did not finish must fail the run; INFO never gates; NOT-RUN is not a pass.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import orchestrate as o  # noqa: E402


def good(**override):
    res = [dict(id=i, status="PASS", title=i, detail="") for i in o.SMOKE_REQUIRED]
    for r in res:
        if r["id"] in override:
            r["status"] = override[r["id"]]
    return res


def main():
    v, p = o.evaluate_smoke(good(), True, [])
    assert v == "PASS" and not p, (v, p)
    # An observation never gates.
    v, p = o.evaluate_smoke(good(**{"A-WEBVIEW2": "INFO", "W-ENV": "INFO"}), True, [])
    assert v == "PASS", (v, p)
    # An assertion that did not hold fails the run and names the check.
    v, p = o.evaluate_smoke(good(**{"A-LEFTOVERS": "FAIL"}), True, [])
    assert v == "FAIL" and any("A-LEFTOVERS" in x for x in p), (v, p)
    # So does a guest-script error.
    v, p = o.evaluate_smoke(good(**{"B-LAUNCH": "ERROR"}), True, [])
    assert v == "FAIL", (v, p)
    # A check the guest never reported is a failure, not an absence.
    res = [r for r in good() if r["id"] != "B-VERB-OPEN"]
    v, p = o.evaluate_smoke(res, True, [])
    assert v == "FAIL" and any("B-VERB-OPEN MISSING" in x for x in p), (v, p)
    # A guest that stopped before the end fails, with its own explanation.
    v, p = o.evaluate_smoke(good(), False, ["SMOKE-ABORT user.ps1 never started"])
    assert v == "FAIL" and any("never started" in x for x in p), (v, p)
    v, p = o.evaluate_smoke(good(), False, [])
    assert v == "FAIL" and any("SMOKE-DONE" in x for x in p), (v, p)
    # Something that could not be judged is reported, and is not a pass.
    v, p = o.evaluate_smoke(good(**{"B-OPENWITH-ZIP": "NOT-RUN"}), True, [])
    assert v == "INCOMPLETE" and not p, (v, p)
    # NOT-RUN never hides a failure.
    v, p = o.evaluate_smoke(good(**{"B-OPENWITH-ZIP": "NOT-RUN", "A-FILES": "FAIL"}), True, [])
    assert v == "FAIL", (v, p)
    # A known gap is reported, does not fail the run, and is never a pass.
    v, p = o.evaluate_smoke(good(**{"A-INSTALL-KEY": "KNOWN-GAP"}), True, [])
    assert v == "FINDINGS" and any("A-INSTALL-KEY" in x for x in p), (v, p)
    v, p = o.evaluate_smoke(good(**{"A-INSTALL-KEY": "KNOWN-GAP", "B-LAUNCH": "FAIL"}), True, [])
    assert v == "FAIL", (v, p)
    md = o.render_smoke_md({"label": "t", "verdict": v, "problems": p, "results": good(), "windows": {},
                            "facts": {}, "counts": {}})
    assert "| `A-FILES` " in md and "## Problems" in md
    print("smoke self-test ok")


main()
